# F2 investigation — revm/tevm compare warnings

**Date:** 2026-05-01
**Scope:** mainnet `telos-reth-v2-mainnet-quick`. Same code path active on testnet quick and testnet full.
**Purpose:** establish what the warnings mean, where they come from, what the operational impact is, and whether to fix or formally accept.

---

## 1. The warning rate, refreshed

Last 6 hours on mainnet quick:

| Category | Count | % |
| --- | ---: | ---: |
| `Difference in balance` | 837 | 32% |
| `Difference in nonce` | 816 | 31% |
| `Difference in value on revm storage` | 931 | 36% |
| **Total** | **2,584** | — |
| Distinct affected addresses | 49 | — |

Extrapolating: ~10,300/day, ~430/h, one warning every ~8s on average. Tightly clustered around ~50 EOAs and a handful of contract storage slots. Consistent with the prior 24h sample (9,361 warnings, ~390/h). Rate is stable.

---

## 2. The mechanism, end-to-end

### 2.1 Where the comparison fires

`crates/engine/tree/src/tree/payload_validator.rs:1086-1130`. When reth receives a block via `engine_newPayload` with `trust_consensus + build_state` flags:

```rust
} else if reth_telos_primitives_traits::trust_consensus() &&
    reth_telos_primitives_traits::build_state()
{
    // trust_consensus WITH build_state: skip executor.finish() (nothing was executed),
    // but read state diffs from the CL extra fields and apply them to the revm State<DB>.
    drop(executor);
    let block_hash = input.hash();
    let extra_fields_path = format!("/tmp/telos-extra-fields/{block_hash:?}.json");
    match parse_extra_fields_from_file(&extra_fields_path) {
        Ok(Some(extra_fields)) => {
            let statediffs_account = extra_fields.statediffs_account...;
            let statediffs_accountstate = extra_fields.statediffs_accountstate...;
            ...
            compare_state_diffs(
                &mut db,
                statediffs_account,
                statediffs_accountstate,
                ...
            );
        }
        ...
    }
}
```

**Critical:** in this mode, **revm executes nothing**. The executor is dropped without `executor.finish()`. The block's resulting state comes entirely from the CL's extra-fields, not from EVM execution.

### 2.2 What `compare_state_diffs` actually does

`crates/telos/rpc-engine-api/src/compare.rs:134-`. Per row in `statediffs_account`:

```rust
if let Ok(revm_row) = revm_db.basic(row.address) {
    if let Some(unwrapped) = revm_row {
        if unwrapped.balance != row.balance {
            warn!("Difference in balance, address: {:?} - revm: {:?} - tevm: {:?}", ...);
            state_override.override_balance(revm_db, row.address, row.balance);
        }
        if unwrapped.nonce != row.nonce {
            warn!("Difference in nonce, ...");
            state_override.override_nonce(...);
        }
        ...
    }
}
```

For storage rows, same pattern. After all rows are processed, `state_override.apply(revm_db)` commits the corrected state to revm's `State<DB>` buffer.

Persisted MDBX state is therefore **the tevm-supplied values**, not revm's. The warnings are noise from this pipeline; the persisted data is canonical.

---

## 3. Why the warnings fire 9k/day

In `trust_consensus + build_state` mode, **revm holds the pre-block state**. When `compare_state_diffs` iterates rows in `statediffs_account`, each row is a post-tx value (one per transaction touching that address in this block). For any address with at least one tx in the block, `revm.basic(addr)` returns the address's value from the previous block — which differs from any post-tx value in the new block. So:

> ⇒ Every (address, tx) pair where the address was modified produces 2 warnings (balance + nonce).

If 49 distinct addresses are touched ~8.7 times each over 6 h (≈ 1 tx every 41 minutes per address — plausible for active relayers on a 0.5s blocktime chain), we'd expect ~852 balance warnings and ~852 nonce warnings. Observed: 837 + 816. **The rate matches the "every modified address fires a warning per tx" model.**

So the warnings are not bug indicators — they're the natural consequence of:

1. revm having no role in computing state in `build_state` mode (it doesn't execute, so it can't possibly match).
2. `compare_state_diffs` being designed to emit a warning for every address that needs overriding.

Equivalent in spirit to logging every cache miss in a system that is *structurally* a cache miss for every key.

---

## 4. The "off-by-one tx" pattern explained

Earlier observation: revm's value at warning N equals tevm's value at warning N-1, with a stable per-tx delta (~0.4 TLOS for `0xb8ff877…`, ~200 wei to ~0.0002 TLOS for `0x339d413c…`).

Re-examining: the two consecutive warnings I saw at the same address were from **different blocks**, not the same block. Inter-warning gaps of milliseconds are indicative of catchup-mode block processing, not multi-iter loops within one block. After block N applies, the override commits, and MDBX state for the address becomes tevm's post-block-N value. When block N+1 is processed, revm's pre-block state (from MDBX) is exactly that. So the warning emits revm = tevm-from-prev-block, tevm = tevm-of-this-block. The "off-by-one" is "off by one block worth of activity for that address."

**No latent bug.** The pattern reflects per-block override-and-resurface.

---

## 5. Operational implications

### What's *not* affected

- **Persisted MDBX state.** Cross-checked at 8/8 sample blocks across the previously-corrupted window post-resync, and at qTip-30,000 in the 2026-04-29 UAT. State always matches canonical because tevm overrides revm before the trie task runs.
- **Block hash, stateRoot, receiptsRoot.** Computed off the post-override state.
- **Standard EVM RPC reads** (`eth_getBalance`, `eth_getTransactionCount`, `eth_getStorageAt`). These read from MDBX, which has the post-override (tevm-correct) state.

### What *might* be affected

- **`debug_traceTransaction` / `debug_traceCall`.** These can run revm against historical state. If revm's per-tx execution diverges from canonical (which is what the compare warnings signal), traces could show internal opcode-level state that doesn't match what an authoritative trace from `rpc.telos.net` would produce. Worth a focused test: pick one of the divergent txs (e.g. the 0xe1a1ce3e tx we used as a UAT fixture) and run `debug_traceTransaction` against both endpoints, diff the result.
- **`eth_call` simulations.** `eth_call` runs revm against a snapshot of state. If state at the snapshot point is correct (which it is, because of the override) but revm's *execution* of the simulated call uses the same opcode semantics that produced the divergence, simulations could differ subtly from `rpc.telos.net`'s. Same test as traces would surface this.
- **Log noise.** 10k/day fills journal volume and obscures real signals. Current daily journal pressure isn't critical, but a consumer of these warnings (alerting, dashboard) has to filter aggressively.

### What's the worst case

If tooling that *should* return canonical-equivalent results (e.g. a DEX UI's local simulation) calls `eth_call` on this node and the call's execution path hits the divergence pattern, the simulated result could differ from the actual on-chain result by the same per-tx delta seen in the warnings. For most use cases this is irrelevant (the on-chain result is what gets executed). For pre-trade simulation it could be material.

---

## 6. Root cause (best current understanding)

The per-tx delta exists because revm and the Telos native EVM contract account for transactions in slightly different ways. The most likely candidates, in rough order:

1. **Gas refund / fee burn timing.** Telos has a non-standard fee model (the `eosio.evm` contract handles balance changes on entry/exit of EVM execution, with potential differences in how unused-gas refund is credited). revm uses standard Ethereum refund rules.
2. **Intrinsic gas calculation.** Telos applies a price multiplier on EVM gas at the contract layer; revm uses standard Ethereum intrinsic gas.
3. **Nonce-on-tx-start vs nonce-on-tx-success timing.** revm always increments nonce at tx execution start. Telos may track this slightly differently in its account table — though this would only show as a nonce divergence on revert, and the warnings fire on every tx not just reverts.

The deltas observed (~0.4 TLOS per tx for the busy relayer; smaller for other addresses) are dependent on each address's tx kind (e.g. transfer vs contract call vs token transfer). That's consistent with gas/fee accounting, not a fixed-offset bug.

To pin it down would require a focused investigation: pick one specific divergent tx, fetch its full receipt + trace from canonical, replay the same tx through revm in isolation, diff the per-step state. ~half day of focused work. Not in scope for this readiness pass.

---

## 7. Recommendation

**Formally accept** the warnings as expected output of the `build_state` architecture, with three operational measures:

1. **Document this in `CLIENT_CONTRACT_LIB_TRACKING.md`** — add a paragraph that `debug_*` and `eth_call` results may differ from `rpc.telos.net` by small per-tx amounts. Clients needing canonical execution should use `rpc.telos.net` for those methods.
2. **Suppress the warnings or downgrade to debug-level.** They emit at WARN today, which pollutes journals. Either gate them behind a `RUST_LOG=...compare=warn` env var explicitly, or change the level to debug. They're not actionable signals.
3. **One-time deep investigation** (~half day) to trace one divergent tx and confirm the gas/fee accounting hypothesis. If confirmed, document; if a real bug surfaces, file properly. **Not on the critical path** for production readiness.

**Not recommended:** trying to make revm match Telos at the EVM-execution level. That would require porting the Telos contract's accounting into revm itself — major engineering, with no operational gain (the persisted state is already canonical via the override mechanism).

---

## 8. Followups

- [ ] Suppress / downgrade the warnings (1-line change in compare.rs, ~30 min plus build+deploy).
- [ ] Add a paragraph to the client contract about `debug_*` and `eth_call` result variability.
- [ ] Optional: trace one divergent tx for a concrete root cause document.
- [ ] Add "compare warning rate" to monitoring dashboards as a tripwire — a sudden spike (10× baseline) would indicate a real divergence has appeared, distinct from the structural baseline.
