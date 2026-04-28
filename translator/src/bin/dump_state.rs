//! Dumps the current EVM state from a running nodeos's eosio.evm contract
//! tables into a JSON file matching the format consumed by
//! `injection::generate_extra_fields_from_json`.
//!
//! Used to seed reth state at a quick-sync chain-spec anchor block, fixing
//! the long-tail-account-zero-balance bug (#77).

use clap::Parser;
use std::fs::File;
use std::io::Write;
use std::time::Instant;
use telos_translator_rs::injection::dump_storage;

#[derive(Parser, Debug)]
struct Args {
    /// HTTP endpoint of the nodeos to dump state from (e.g. http://localhost:8888 for mainnet).
    #[arg(long)]
    http: String,
    /// block_delta for the chain (mainnet=36, testnet=57).
    #[arg(long)]
    block_delta: u32,
    /// Output JSON file path.
    #[arg(long)]
    out: String,
    /// If set, write JSON pretty-printed (default: compact).
    #[arg(long, default_value_t = false)]
    pretty: bool,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    let args = Args::parse();
    eprintln!(
        "dump_state: nodeos={} block_delta={} out={}",
        args.http, args.block_delta, args.out
    );

    let t0 = Instant::now();
    let state = dump_storage(&args.http, args.block_delta).await;
    let elapsed = t0.elapsed();

    let json = if args.pretty {
        serde_json::to_string_pretty(&state)?
    } else {
        serde_json::to_string(&state)?
    };

    let mut f = File::create(&args.out)?;
    f.write_all(json.as_bytes())?;
    f.flush()?;

    let bytes = json.len();
    eprintln!(
        "dump_state: complete in {:.1}s, wrote {} bytes to {}",
        elapsed.as_secs_f64(),
        bytes,
        args.out
    );

    // Quick stats from the JSON we just wrote (re-parse to avoid
    // depending on private struct fields).
    let parsed: serde_json::Value = serde_json::from_str(&json)?;
    let evm_block = parsed.get("evm_block_num").and_then(|v| v.as_u64());
    let n_accounts = parsed
        .get("accounts")
        .and_then(|v| v.as_array())
        .map(|a| a.len());
    eprintln!(
        "dump_state: evm_block_num={:?}, accounts={:?}",
        evm_block, n_accounts
    );

    Ok(())
}
