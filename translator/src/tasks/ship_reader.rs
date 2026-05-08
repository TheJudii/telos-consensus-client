use eyre::{eyre, Result};
use futures_util::stream::SplitStream;
use futures_util::StreamExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::info;
use tracing::{debug, error};

pub async fn ship_reader(
    mut ws_rx: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    raw_ds_tx: mpsc::Sender<Vec<u8>>,
    mut stop_rx: mpsc::Receiver<()>,
) -> Result<()> {
    let mut counter: u64 = 0;

    loop {
        // Read the websocket
        let message = tokio::select! {
            message = ws_rx.next() => message,
            shutdown = stop_rx.recv() => {
                if shutdown.is_some() {
                    break;
                }
                return Err(eyre!("shutdown channel closed before SHIP reader stopped"));
            }
        };

        counter += 1;
        match message {
            Some(Ok(msg)) => {
                debug!("Received message {counter}, sending to raw ds pool...",);
                // write to the channel
                if raw_ds_tx.is_closed() {
                    continue;
                }
                if let Err(e) = raw_ds_tx.send(msg.into_data()).await {
                    error!("Receiver dropped {:?}", e);
                    return Err(eyre!(
                        "raw deserializer dropped before SHIP message could be sent: {e}"
                    ));
                }
                debug!("Sent message {counter} to raw ds pool...");
            }
            Some(Err(e)) => {
                error!("Error receiving message: {}", e);
                return Err(eyre!("error receiving SHIP websocket message: {e}"));
            }
            None => {
                return Err(eyre!("SHIP websocket closed"));
            }
        }
    }
    info!("Exiting ship reader...");
    Ok(())
}
