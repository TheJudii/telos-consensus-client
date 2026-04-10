//! Standalone probe that connects to SHIP, pulls a range of blocks,
//! and reports whether the eosio.evm::raw action in each block has a
//! parseable PrintedReceipt. Used to diagnose the testnet divergence.

use clap::Parser;
use telos_translator_rs::block::TelosEVMBlock;
use telos_translator_rs::translator::{Translator, TranslatorConfig};
use telos_translator_rs::types::translator_types::ChainId;
use tokio::sync::mpsc;
use tracing::info;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, default_value = "ws://127.0.0.1:18081")]
    ship: String,
    #[arg(long, default_value = "http://127.0.0.1:18889")]
    http: String,
    #[arg(long)]
    start: u32,
    #[arg(long)]
    stop: u32,
    #[arg(long, default_value_t = 41)]
    chain_id: u64,
    #[arg(long, default_value = "0000000000000000000000000000000000000000000000000000000000000000")]
    prev_hash: String,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let config = TranslatorConfig {
        chain_id: ChainId(args.chain_id),
        evm_deploy_block: Some(136_393_755),
        evm_start_block: args.start,
        evm_stop_block: Some(args.stop),
        prev_hash: args.prev_hash,
        validate_hash: None,
        http_endpoint: args.http,
        ship_endpoint: args.ship,
        raw_message_channel_size: 1000,
        block_message_channel_size: 1000,
        final_message_channel_size: 1000,
    };
    let (tx, mut rx) = mpsc::channel::<TelosEVMBlock>(1000);
    let handle = tokio::spawn(async move {
        let mut count = 0u32;
        let mut with_tx = 0u32;
        while let Some(block) = rx.recv().await {
            count += 1;
            let n = block.transactions.len();
            if n > 0 {
                with_tx += 1;
                info!(
                    "probe block={} evm_tx={} receipts_root={:#x} tx_root={:#x}",
                    block.block_num,
                    n,
                    block.header.receipts_root,
                    block.header.transactions_root,
                );
            }
        }
        info!("probe finished: {} blocks, {} with txs", count, with_tx);
    });
    if let Err(e) = Translator::new(config).launch(Some(tx)).await {
        eprintln!("launch failed: {e:?}");
    }
    let _ = handle.await;
    Ok(())
}
