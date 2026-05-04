# Spec — Multi-endpoint canonical validation for the translator

**Item:** B7 from the 2026-05-01 readiness backlog.
**Goal:** eliminate the single-point-of-trust on `https://rpc.telos.net` (resp. `https://rpc.testnet.telos.net`) in the consensus client's translator. Today, when the translator's locally-built EVM block disagrees with the canonical reference, the canonical-RPC value is fetched and trusted unconditionally. If that single endpoint is wrong (compromised, on a divergent fork, returning stale data, or just down), the translator silently propagates the error.

**Implementor:** consensus-client maintainers (not LLM-implementable safely).

---

## 1. Problem statement

`crates/translator/src/tasks/final_processor.rs` and `crates/translator/src/tasks/raw_deserializer.rs` use `rpc_fallback_endpoint` as the **single source of truth** for canonical block content when the locally-built block hash mismatches. The architecture is:

```
SHIP block N (from local nodeos)
        │
        ▼
local-build EVM block N (translator)
        │
        ▼ (rpc_fallback_sample_every_n governs whether to compare)
   compare hash with rpc_fallback_endpoint?
        │
        ├─ MATCH → use locally-built block
        ├─ MISMATCH → fetch canonical from rpc_fallback_endpoint, use that
        └─ rpc_fallback_endpoint unreachable → currently: log + use locally-built block (silently)
```

Three failure modes the current design doesn't handle:

1. **Endpoint compromise.** If `rpc.telos.net` is somehow taken over (or returns subtly wrong responses), our reth nodes will adopt that bad data without any cross-check.
2. **Endpoint on a divergent fork.** During a microfork, `rpc.telos.net` might briefly return the losing fork (it's just a node, it can be wrong for a few seconds). We'd take that as truth.
3. **Endpoint stale.** If `rpc.telos.net` is lagging, we get older data than what's actually canonical.

---

## 2. Proposed design — 2-of-3 majority validation

Replace `rpc_fallback_endpoint` (single string) with `rpc_fallback_endpoints` (list of strings). On every cross-check (sample_every_n) or on hash mismatch:

```
fetch block N from each of {endpoint_a, endpoint_b, endpoint_c}
    │
    ▼
group results by hash:
    │
    ├─ all 3 agree    → high-confidence canonical, use that hash
    ├─ 2 of 3 agree   → majority canonical, use that hash, log_warn (one endpoint diverged)
    ├─ 1 unique each  → log_error, refuse to commit block N, fall back to LIB-tracking
    └─ 0 reachable    → log_error, fall back to LIB-tracking until quorum returns
```

The translator continues to operate (no crash, no service restart) even when fewer than 2 endpoints are reachable — it just enters degraded mode where it cannot independently verify against canonical and will defer non-final blocks until quorum is back.

### 2.1 Config changes

```toml
# Before:
rpc_fallback_endpoint = "https://rpc.telos.net"

# After:
rpc_fallback_endpoints = [
    "https://rpc.telos.net",                         # mainnet primary (Telos infra)
    "https://mainnet.telos.eosrio.io",               # mainnet secondary (EOS Rio infra)
    "https://api.telos.kanda.global",                # mainnet tertiary (Kandaweather infra)
]
rpc_fallback_quorum = 2                               # 2-of-3 required for confidence
```

For testnet:
```toml
rpc_fallback_endpoints = [
    "https://rpc.testnet.telos.net",
    "https://testnet.telos.eosrio.io",
    # third testnet endpoint TBD
]
rpc_fallback_quorum = 2
```

### 2.2 Endpoint requirements

For an endpoint to count toward quorum:

- Must be operated by a **distinct organization** from the others (no two endpoints from the same provider).
- Must serve full archival history (not pruned beyond our quick-sync window).
- Must be on the same chain ID and major fork-version as the local node.
- Must respond to `eth_blockNumber` within 5s (otherwise treated as unreachable for that round).

If only 1 endpoint is reachable, the translator falls back to "trust local SHIP" (the same as today's `irreversible_only=true` behavior). It does NOT trust the lone reachable canonical endpoint, because that's exactly the single-point-of-trust we're trying to eliminate.

---

## 3. Implementation outline

### 3.1 Config struct change

`client/src/config.rs`:

```rust
#[derive(Deserialize, Debug, Clone)]
pub struct AppConfig {
    ...
    /// RPC endpoints used for canonical cross-validation. At least
    /// `rpc_fallback_quorum` of these must agree for a block to be
    /// considered canonical-verified. Order doesn't matter.
    pub rpc_fallback_endpoints: Vec<String>,

    /// Number of endpoints from `rpc_fallback_endpoints` that must agree
    /// on a block hash for canonical verification to succeed. Should be
    /// `(N/2) + 1` where N = endpoint count. Defaults to 2.
    #[serde(default = "default_rpc_fallback_quorum")]
    pub rpc_fallback_quorum: usize,
    ...
}

fn default_rpc_fallback_quorum() -> usize { 2 }
```

Backward-compat: deserialize the old `rpc_fallback_endpoint` field if present and translate to a 1-element `rpc_fallback_endpoints` with `rpc_fallback_quorum = 1` and emit a startup WARN. Don't break existing configs.

### 3.2 Translator logic change

`crates/translator/src/tasks/final_processor.rs` (where the RPC fallback fires today). Replace single-endpoint fetch with parallel fan-out + tally:

```rust
async fn fetch_canonical_with_quorum(
    block_num: u32,
    endpoints: &[String],
    quorum: usize,
) -> Result<CanonicalBlock, FetchError> {
    let mut handles = vec![];
    for ep in endpoints {
        let ep = ep.clone();
        handles.push(tokio::spawn(async move {
            fetch_block_via_rpc(&ep, block_num).await
        }));
    }

    let mut results: HashMap<B256, Vec<CanonicalBlock>> = HashMap::new();
    let mut reachable = 0;
    for h in handles {
        if let Ok(Ok(block)) = h.await {
            reachable += 1;
            results.entry(block.hash).or_default().push(block);
        }
    }

    // Find the hash with the most votes
    if let Some((winning_hash, votes)) = results.iter().max_by_key(|(_, v)| v.len()) {
        if votes.len() >= quorum {
            return Ok(votes[0].clone());
        }
    }

    if reachable < quorum {
        return Err(FetchError::InsufficientQuorum { reachable, required: quorum });
    } else {
        // We had enough endpoints reachable but they disagreed
        return Err(FetchError::EndpointsDisagree { tally: results.iter().map(|(h,v)| (h.clone(), v.len())).collect() });
    }
}
```

### 3.3 Behavior on quorum failure

If `fetch_canonical_with_quorum` returns `InsufficientQuorum`:
- Log at WARN with the count of reachable endpoints.
- Defer the block: don't emit `engine_newPayload` for it. Hold it in the translator's pending queue.
- Re-check quorum every 30s (configurable). When quorum is restored, drain the pending queue.
- Critical: don't crash. The CL stays running; reth's existing canonical state is unaffected.

If `fetch_canonical_with_quorum` returns `EndpointsDisagree`:
- Log at ERROR. Include the tally of (hash, vote_count) so the operator can see which endpoint is the outlier.
- Defer the block as above.
- This is a serious signal — at least one endpoint is wrong. Wire to alerting.

### 3.4 Metrics

Add three metrics for monitoring:

- `telos_translator_rpc_fallback_total{endpoint, outcome}` — counter, outcomes = `success / unreachable / wrong_hash`.
- `telos_translator_rpc_fallback_quorum_failures_total` — counter.
- `telos_translator_rpc_fallback_disagreements_total{outlier_endpoint}` — counter; key on which endpoint disagreed with majority.

Disagreement rate is the most operationally important — a sustained nonzero `disagreements_total` means an endpoint needs to be evicted from the list.

---

## 4. Test plan

### 4.1 Unit tests (in `crates/translator/src/tasks/`)

Mock 3 HTTP endpoints with controllable responses. Test cases:

| Scenario | Setup | Expected |
| --- | --- | --- |
| All agree | Endpoints A, B, C return same hash | Return that hash, no warning |
| 2 of 3 agree | A, B return X; C returns Y | Return X, log_warn naming C as outlier, increment `disagreements_total{outlier=C}` |
| 1 unique each | A returns X, B returns Y, C returns Z | Return Err::EndpointsDisagree, defer block |
| 0 reachable | All 3 timeout | Return Err::InsufficientQuorum, defer block |
| 1 reachable, others timeout | A returns X, B+C timeout | Return Err::InsufficientQuorum (1 < 2 quorum) |

### 4.2 Integration test on testnet

1. Run modified translator on testnet quick with 3 endpoints configured.
2. Soak for 48h with no induced failures. Confirm zero `disagreements_total` (or document any genuine disagreements observed).
3. Simulate one endpoint outage: block traffic to one endpoint via iptables for 10 minutes. Confirm translator continues operating (other 2 endpoints provide quorum), `unreachable` counter increments for the blocked endpoint.
4. Simulate genuine disagreement: temporarily point one endpoint to a different chain or a stale snapshot. Confirm `disagreements_total{outlier=...}` increments and block is deferred. (Do this on testnet only; do not interfere with mainnet endpoints.)

### 4.3 Mainnet promotion gate

After 48h clean testnet soak with 2-of-3 quorum continuously achievable:

- Pre-deploy snapshot of mainnet quick (per `PRE_DEPLOY_BACKUP_RUNBOOK.md`).
- Apply the same config change to mainnet quick.
- Watch the canonical-comparison monitor for any new MISMATCH or quorum-failure signals.
- 48h additional soak on mainnet before declaring done.

---

## 5. Effort estimate

- Config struct + backward-compat parsing: 0.5 day
- Translator fan-out logic: 1 day
- Unit tests: 1 day
- Metrics integration: 0.5 day
- Testnet soak (wall-clock): 2 days
- Mainnet promotion + soak: 2 days

**Total engineering: ~3 days. Total wall-clock to mainnet: ~7 days.**

---

## 6. Endpoint procurement (operational)

The implementation requires at least 2 distinct, trusted-and-stable Telos mainnet RPC endpoints beyond `rpc.telos.net` for the quorum to actually reduce single-point-of-trust risk. Operators to confirm:

- Which infrastructure providers (BPs, dapps, third-party services) currently run public Telos mainnet RPC?
- Of those, which have committed SLAs / are operationally trusted?
- Does any provider already document a versioned API endpoint we can rely on?

A single-provider quorum (e.g. 3 endpoints all run by Telos infra) doesn't actually solve the trust problem — if Telos infra is compromised, all 3 are compromised together. The list MUST include endpoints from organizationally-distinct operators.

---

## 7. What this doesn't fix

- **Hostile chain producers.** If a majority of BPs collude to produce a wrong block, the on-chain state is wrong and every endpoint will agree on the wrong value. Multi-endpoint quorum can't detect this. (Practical mitigation: monitor the BP set + on-chain governance.)
- **Subtle JSON-RPC differences between endpoints.** Different node implementations may differ on edge cases (e.g. transaction order, gas-used reporting). The hash comparison should still hold (block hash is deterministic), but receipts/state may differ in non-block-hash ways. Out of scope for this change.
- **Endpoint discovery.** The endpoint list is static config, not dynamically discovered. Adding/removing endpoints is a config edit + service restart. A future enhancement could fetch the list from a signed manifest.
