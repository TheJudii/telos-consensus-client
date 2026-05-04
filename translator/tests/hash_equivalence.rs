//! Hash equivalence regression test against a reference RPC.
//!
//! This test reads a corpus of (block_number, expected_hash) pairs from
//! `tests/fixtures/hash_equivalence_corpus.json` and, for each entry,
//! queries the RPC endpoint configured via `TEL_TEST_RPC` and asserts
//! equality.
//!
//! This is a live-endpoint test: without `TEL_TEST_RPC` set, it prints
//! a skip message and exits cleanly. This lets it run opt-in in CI
//! against a deployed node without blocking the default `cargo test`
//! developer flow.
//!
//! The corpus includes one known-diverge block (category "K") that is
//! EXPECTED to fail when queried by number against a reth instance
//! whose `CanonicalHeaders` index is stale at that height. That single
//! mismatch is reported as INFO, not FAIL. All other blocks must match.
//!
//! Extending the corpus: edit `build_corpus.py` with new block numbers
//! and re-run against canonical to regenerate the JSON. See
//! `docs/regression-corpus.md` for guidance on category coverage.

use serde::Deserialize;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct CorpusEntry {
    block_number: u64,
    block_hash: String,
    #[serde(default)]
    #[allow(dead_code)]
    parent_hash: String,
    #[serde(default)]
    #[allow(dead_code)]
    timestamp: String,
    #[allow(dead_code)]
    tx_count: u64,
    category: String,
    note: String,
}

#[derive(Debug, Deserialize)]
struct Corpus {
    #[allow(dead_code)]
    reference_rpc: String,
    #[allow(dead_code)]
    generated: u64,
    blocks: Vec<CorpusEntry>,
}

/// Minimal HTTP POST JSON-RPC client.
///
/// Returns (status_code, body). Supports only http:// URLs; TLS is NOT
/// supported on purpose — for TLS-protected endpoints, run the test
/// against a local reverse-proxy or use a plaintext canonical endpoint.
fn http_post_json(url: &str, json: &str) -> Result<(u16, String), String> {
    let url = url.strip_prefix("http://").ok_or_else(|| {
        "TEL_TEST_RPC must be an http:// URL for this test; no TLS support".to_string()
    })?;
    let (host_port, path) = match url.find('/') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, "/"),
    };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(80)),
        None => (host_port.to_string(), 80u16),
    };
    let mut stream = TcpStream::connect_timeout(
        &format!("{host}:{port}").parse().map_err(|e| format!("addr: {e}"))?,
        Duration::from_secs(5),
    )
    .map_err(|e| format!("connect: {e}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         User-Agent: telos-hash-equivalence-test/1.0\r\n\
         Connection: close\r\n\r\n{json}",
        len = json.len(),
    );
    stream.write_all(req.as_bytes()).map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = match text.find("\r\n\r\n") {
        Some(i) => (&text[..i], &text[i + 4..]),
        None => return Err("no http body".to_string()),
    };
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, body.to_string()))
}

fn rpc_block_hash(endpoint: &str, block_number: u64) -> Result<Option<String>, String> {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"eth_getBlockByNumber","params":["{:#x}",false]}}"#,
        block_number
    );
    let (status, response) = http_post_json(endpoint, &body)?;
    if status != 200 {
        return Err(format!("http status {status}: {}", response.chars().take(200).collect::<String>()));
    }
    let v: serde_json::Value = serde_json::from_str(&response)
        .map_err(|e| format!("json parse: {e} body={}", response.chars().take(200).collect::<String>()))?;
    if v.get("result").is_none() || v["result"].is_null() {
        return Ok(None);
    }
    Ok(Some(v["result"]["hash"].as_str().unwrap_or_default().to_lowercase()))
}

#[test]
fn hash_equivalence_against_corpus() {
    let endpoint = match std::env::var("TEL_TEST_RPC") {
        Ok(e) if !e.is_empty() => e,
        _ => {
            eprintln!(
                "SKIP: set TEL_TEST_RPC=http://your-node:8545 to run hash_equivalence_against_corpus"
            );
            return;
        }
    };

    let corpus_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hash_equivalence_corpus.json");
    let raw = fs::read_to_string(&corpus_path)
        .unwrap_or_else(|e| panic!("cannot read corpus at {corpus_path:?}: {e}"));
    let corpus: Corpus = serde_json::from_str(&raw).expect("corpus deserialization failed");

    eprintln!(
        "Running hash_equivalence against endpoint={} corpus_size={}",
        endpoint, corpus.blocks.len(),
    );

    let mut matches = 0usize;
    let mut known_diverges = 0usize;
    let mut missing = 0usize;
    let mut mismatches: Vec<String> = Vec::new();

    for entry in &corpus.blocks {
        let actual = match rpc_block_hash(&endpoint, entry.block_number) {
            Ok(v) => v,
            Err(e) => {
                mismatches.push(format!(
                    "RPC_ERROR bn={} cat={} note=\"{}\" err={}",
                    entry.block_number, entry.category, entry.note, e
                ));
                continue;
            }
        };
        let expected = entry.block_hash.to_lowercase();
        match actual {
            None => {
                missing += 1;
                mismatches.push(format!(
                    "MISSING bn={} cat={} note=\"{}\" expected={}",
                    entry.block_number, entry.category, entry.note, expected
                ));
            }
            Some(got) => {
                if got == expected {
                    matches += 1;
                } else if entry.category == "K" {
                    known_diverges += 1;
                    eprintln!(
                        "INFO known-diverge bn={} cat=K got={} expected_canonical={} (note: {})",
                        entry.block_number, got, expected, entry.note,
                    );
                } else {
                    mismatches.push(format!(
                        "MISMATCH bn={} cat={} note=\"{}\" got={} expected={}",
                        entry.block_number, entry.category, entry.note, got, expected
                    ));
                }
            }
        }
    }

    eprintln!(
        "hash_equivalence result: matches={} known_diverges={} missing={} mismatches={}",
        matches,
        known_diverges,
        missing,
        mismatches.len(),
    );

    if !mismatches.is_empty() {
        for m in &mismatches {
            eprintln!("  {m}");
        }
        panic!(
            "hash_equivalence failed: {} blocks did not match canonical",
            mismatches.len()
        );
    }
    assert!(matches > 0, "corpus was empty or all skipped");
}
