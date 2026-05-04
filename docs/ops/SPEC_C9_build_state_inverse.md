# Spec — `build_state` inverse OR revm-execution mode

**Item:** C9 from the 2026-05-01 readiness backlog.
**Goal:** allow telos-reth v2 quick to recover from MDBX state corruption *without* a full chainspec resync. Today, when the `build_state` path commits a wrong state diff to MDBX (as happened on 2026-04-30), there's no inverse operation — the only recovery is "wipe and resync from chainspec," which took 15 hours.
**Status:** **Not urgent.** Lives behind C8 (fork handling) in the queue. C8 prevents the corruption in the first place; C9 is the cleanup safety net for when prevention fails. Both should exist for a fully production-grade stack, but C8 is the higher-impact item.
**Implementor:** reth + telos-rpc-engine-api maintainers.

---

## 1. Problem statement

`crates/engine/tree/src/tree/payload_validator.rs:1086` — when `--telos.trust_consensus + --telos.build_state` are set, reth's payload validator skips revm execution entirely and applies CL-supplied extra-fields directly to revm's `State<DB>` via `compare_state_diffs`. The `state_override.apply(revm_db)` call commits to the in-memory state buffer, which is then flushed to MDBX once the block ages past `--engine.persistence-threshold`.

There's no inverse operation. The state-diff is forward-only:

```rust
// Current behavior (paraphrased from compare.rs):
state_override.override_balance(addr, new_balance);   // forward: set to new value
// no inverse exists:
// state_override.unwrite_balance(addr, old_balance) — doesn't exist
```

If a wrong state diff is written and then later committed to MDBX, the only way to fix MDBX is to wipe and rebuild. That's the 15-hour cost we paid on 2026-04-30.

---

## 2. Two design options, with recommendation

### Option A: Implement an inverse for `build_state`

The translator already produces forward state diffs. To make it reversible, the translator would need to also produce inverse diffs for each block — i.e., for every account it modifies, record the BEFORE values, not just the AFTER values. Then on a reorg-driven discard, reth could replay the inverse diffs to undo the modifications.

Pros:
- Surgical. Minimal change to reth.
- Preserves the build_state speedup.

Cons:
- Translator has to fetch BEFORE state for every modified account (it currently only has AFTER state from the Antelope contract). This is an extra DB query per modified account per block, ~30-50 queries per block at observed mainnet rates.
- The extra-fields JSON file gets ~2× larger (forward + inverse).
- Storage of inverses is unclear: do we keep them indefinitely (useful for any reorg depth) or only for blocks above persistence-threshold (used and dropped when block is finalized)?

### Option B: Switch to revm-execution mode for canonical-tracking

Add a new mode `--telos.execute-via-revm=true`. In that mode, the engine API runs revm against the block's transactions normally, computing the result trie via standard EVM execution. The CL-supplied extra-fields are used only as a comparison reference (the existing `compare_state_diffs` flow), not as the source of truth.

Pros:
- Reth has working reorg semantics for revm-executed state. No new code needed to support reorg.
- Aligns with how every other Ethereum-compatible execution layer works.
- Eliminates the F2 compare-warning noise (revm and tevm are now expected to match exactly; any divergence is a real bug).

Cons:
- Loses the 10× catchup speed of build_state. Initial sync goes from ~hours to ~days.
- Per-block CPU cost: ~10× higher (running revm vs not).
- Doesn't address the underlying revm-vs-tevm semantic gap (some divergences are real Telos-specific accounting; revm execution would now "be wrong" for those cases until revm is patched).

### Recommendation: Option B, with a phased rollout

Reth's standard reorg-via-engine-API is well-tested and well-understood. Implementing a custom inverse for build_state is a new code path with no test corpus. We'd be writing the bug-finding infrastructure ourselves.

The 10× catchup-speed loss matters for **initial sync** (when bringing up a new node). It does **not** matter for **steady-state operation** (we process at chain rate either way; revm executing 2 blocks/sec is trivial CPU on modern hardware).

Suggested rollout:
1. Implement `--telos.execute-via-revm=true` as an opt-in flag.
2. Continue to default to `--telos.build_state=true` for existing nodes (preserves their already-synced state).
3. New nodes joining the pool use the seeded chainspec (already at block 464.4M) plus revm-execution mode. Catchup is from chainspec to current tip, ~669K blocks, which at 2 bps = ~93 hours. **OK if that's acceptable as a one-time bootstrap cost.**
4. Optionally: keep build_state mode available for emergency rapid sync (e.g. when standing up many nodes in parallel). But once steady-state, switch to revm mode for reorg safety.

If the bootstrap-time cost is unacceptable, fall back to Option A.

---

## 3. Option B implementation outline

### 3.1 Config change

`crates/telos/primitives/traits/src/lib.rs` (the global trust_consensus state):

```rust
// Existing:
pub fn trust_consensus() -> bool { ... }
pub fn build_state() -> bool { ... }

// New:
pub fn execute_via_revm() -> bool {
    // returns true if the --telos.execute-via-revm CLI flag is set.
}
```

Reth launcher gets:

```bash
--telos.execute-via-revm true \
# When this is set, --telos.build_state is implicitly false even if also set.
```

### 3.2 Payload validator change

`crates/engine/tree/src/tree/payload_validator.rs:1076-1180` — the branch logic that selects between trust_consensus modes. Today:

```rust
let output = if reth_telos_primitives_traits::trust_consensus() &&
    !reth_telos_primitives_traits::build_state()
{
    // trust_consensus without build_state: skip everything, empty output
    BlockExecutionOutput::default()
} else if reth_telos_primitives_traits::trust_consensus() &&
    reth_telos_primitives_traits::build_state()
{
    // trust_consensus WITH build_state: skip executor.finish(), apply CL extra-fields
    drop(executor);
    apply_extra_fields_via_compare_state_diffs(...);
    BlockExecutionOutput::default()
} else {
    // Normal execution path (non-trust_consensus)
    executor.finish(...)
};
```

After this change:

```rust
let output = if reth_telos_primitives_traits::execute_via_revm() {
    // NEW: run revm against the block's txs, then compare against CL extra-fields
    let exec_output = executor.finish(...);
    if reth_telos_primitives_traits::trust_consensus() {
        // Compare-but-don't-override: extra-fields are reference, not authority
        compare_state_diffs(&mut db, ..., panic_mode: false, do_storage: true);
        // Emit metrics on divergence; don't override.
    }
    exec_output
} else if reth_telos_primitives_traits::trust_consensus() &&
    !reth_telos_primitives_traits::build_state()
{
    BlockExecutionOutput::default()
} else if reth_telos_primitives_traits::trust_consensus() &&
    reth_telos_primitives_traits::build_state()
{
    drop(executor);
    apply_extra_fields_via_compare_state_diffs(...);
    BlockExecutionOutput::default()
} else {
    executor.finish(...)
};
```

Note: in `execute_via_revm` mode with `trust_consensus`, `compare_state_diffs` is called with `panic_mode: false` and the override-on-mismatch behavior is suppressed (revm's value is the truth). Today `compare_state_diffs` overrides revm to match tevm; in the new mode, the comparison is observational only.

### 3.3 Compare module change

`crates/telos/rpc-engine-api/src/compare.rs:134-` — add a parameter `override_on_mismatch: bool` (default `true` for backward compat). When called from the new `execute_via_revm` path, pass `false`. When mismatch detected, log only; don't apply state_override.

### 3.4 Reth's natural reorg path takes over

Once the engine API uses revm execution:

- `engine_newPayload(N+1)` validates by re-running revm. If parent is wrong, fails the payload.
- `engine_forkchoiceUpdated(head=X)` causes reth to reorg in-memory state to make X the head. This already works for revm-executed state — built-in reth functionality.
- Persistence-threshold + backpressure-threshold work as designed: blocks below threshold are committed to MDBX, blocks above are reorg-able in memory.
- No `build_state` inverse needed because no `build_state` apply is happening.

### 3.5 Migration path for existing nodes

A node already running on `build_state` (today's mainnet/testnet quick) can switch to revm mode without resync:

1. Pre-deploy snapshot.
2. Stop services.
3. Edit launcher: replace `--telos.build_state` with `--telos.execute-via-revm true`.
4. Start services.
5. Reth restart re-reads from MDBX. State at the current LIB is canonical (because today's `build_state` path persists tevm-correct state). Reth then ingests new blocks via revm execution from this point forward.
6. F2 compare warnings should drop to ~zero (revm now matches tevm naturally; remaining warnings are real divergences that need investigation).

---

## 4. Test plan

### 4.1 Unit / integration

- Build `--telos.execute-via-revm=true` mode, point at a known chain at a known height, confirm reth advances correctly.
- Switch a node from `build_state` to `execute_via_revm` mid-operation, confirm no state divergence.
- Run a 100-block batch with both modes in parallel, diff the resulting MDBX state — must be identical.

### 4.2 F2 follow-up

The F2 compare-warning corpus from 2026-05-01 (`F2_INVESTIGATION_revm_tevm_compare.md`) becomes a test set. After C9 ships, run revm execution against the addresses that previously produced warnings. Expect zero warnings; any remaining warnings are real semantic divergences that need filing as proper bugs.

### 4.3 Catchup performance

Bootstrap a fresh node from the seeded chainspec with both modes. Compare:
- build_state catchup: ~10 bps observed yesterday during recovery, ~1.5 hours from chainspec to tip.
- execute_via_revm catchup: TBD, expect ~2 bps (chain rate). 669K blocks at 2 bps = ~93 hours.

If the gap is larger than expected, gate Option B rollout on whether we can tolerate that bootstrap time.

---

## 5. Effort estimate

- New flag + payload_validator branch: 3 days
- compare.rs `override_on_mismatch` parameter: 0.5 day
- Migration testing: 1 day
- Catchup benchmarking: 0.5 day
- Documentation: 0.5 day

**Total: ~6 days engineering, no soak required (revm-mode is well-understood reth code path).**

---

## 6. Out of scope

- Implementing Option A's `build_state` inverse. If Option B is impractical for any reason, file a separate spec.
- Resolving the F2 compare-warning root cause beyond what the comparison logging surfaces.
- Hybrid mode (use build_state for initial catchup, switch to revm at tip). Possible future optimization; not needed for v1.

---

## 7. Sequencing

C9 should land **after** C8 (fork handling) is shipped and proven. Reasoning:

- C8 prevents corruption from happening in the first place. That's the critical fix.
- C9 makes recovery cheaper when corruption *does* happen anyway (e.g. from a future bug we haven't anticipated). It's defense-in-depth, not the primary defense.

If both ship, the stack is meaningfully more robust. But the order matters: C8 first.
