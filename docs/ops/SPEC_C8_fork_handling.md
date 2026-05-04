# Spec — Proper fork handling for telos-reth v2 quick (head-tracking mode)

**Item:** C8 from the 2026-05-01 readiness backlog. Sequel to `POSTMORTEM_2026-04-30_fork_mvp.md`.
**Goal:** allow `telos-reth-v2-mainnet-quick` to track Antelope head (not just LIB) without the silent state-corruption failure mode that bit us on 2026-04-30.
**Status:** **NOT recommended for implementation until Savanna instant finality is committed-but-not-yet-active on Telos mainnet.** Once Savanna is live, the LIB lag drops to ~1 block and this entire effort becomes redundant. Worth specifying so we have it ready if Savanna slips materially past the planned activation date.

**Implementor:** maintainers of telos-consensus-client + telos-reth-v2 (deep changes in both repos).

---

## 1. What today's stack does, and why it can't go to head

Today, `irreversible_only=true` is set in the translator's SHIP request. SHIP only streams blocks past LIB. The translator never sees a non-finalized block. Reth therefore can't ingest a block that gets reorged.

The cost is the ~165s LIB lag.

To remove that lag we need to flip `irreversible_only=false`. That's the change attempted on 2026-04-30 that caused state corruption. Three reasons it failed:

1. The translator's RPC fallback only validates 1 in 10 empty blocks (`rpc_fallback_sample_every_n=10`). Forked empty blocks slip through.
2. When the RPC fallback DOES detect a divergence, it fetches the canonical block but doesn't tell reth to discard the forked block already submitted. reth keeps the orphan in its chain.
3. `--engine.persistence-threshold=2` (default) commits state to MDBX within 2 blocks of head. Once a forked block is past that threshold, MDBX is permanently wrong.

Each is necessary; none alone is sufficient. The 2026-04-30 MVP fixed only #2 (incompletely, by detecting hash mismatches at the CL→reth boundary which is the wrong layer). It didn't touch #1 or #3. Result: corruption.

---

## 2. The corrected design

### 2.1 Three coupled changes

| Layer | Change | Why |
| --- | --- | --- |
| Translator config | `rpc_fallback_sample_every_n = 1` | Validate every block at translator-build time against canonical, before reth sees it. Already in flight for B6. |
| Translator runtime | Emit `engine_forkchoiceUpdated(head=canonical_block, safe=safe, finalized=last_lib)` whenever the RPC-fallback success path replaces a locally-built block. | Tells reth to discard the orphan and adopt the canonical chain. Today the translator only substitutes the next block; reth retains the orphan. |
| Reth launcher | `--engine.persistence-threshold = 20` (or smaller) | Keep all reorg-able blocks in reth's `CanonicalInMemoryState` buffer, where the standard reorg machinery can drop orphan ExecutedBlocks cleanly without any MDBX writes. Worst observed fork depth = 7; threshold of 20 gives ~3× margin. |

Combine all three with the existing pre-deploy snapshot procedure (`PRE_DEPLOY_BACKUP_RUNBOOK.md`) and the canonical-comparison monitor.

### 2.2 Why each is necessary

- **Without `sample_every_n=1`:** 90% of empty fork blocks aren't checked. The translator builds them from forked SHIP data, sends them to reth as normal forward steps with valid parent_hash, and reth has no way to detect the issue.
- **Without fcU rewind:** even if we detect a fork-block at the translator and substitute the canonical version going forward, the orphan block is still in reth's canonical chain. The next legitimate block will have a parent_hash pointing at the canonical predecessor, not at our orphan, and reth will get confused.
- **Without lower persistence-threshold:** the orphan eventually moves past the threshold and lands in MDBX. MDBX has no inverse for build_state extra-fields (see C9 spec). Once on disk, it stays on disk. The 2026-04-30 incident set threshold=500 to absorb in-memory reorgs, but that just enlarged the corruption window when the fork-handler missed.

### 2.3 Rejected alternatives

| Alternative | Why rejected |
| --- | --- |
| Just lower persistence-threshold to 2 (original value) | Without fcU rewind, orphan still lands in MDBX after 2 blocks. Not enough margin. |
| Rely solely on the translator-level RPC fallback | Without fcU rewind, reth keeps the orphan even when the fallback fires. |
| Implement a build_state inverse so MDBX can be rolled back | Possible (see C9 spec) but much deeper change. The in-memory rewind via persistence-threshold is dramatically simpler. |
| Run all reth nodes through revm (not build_state) | Loses the 10× catchup speed. Rejected for performance — but worth keeping in mind as a fallback if C9 is needed later. |

---

## 3. Implementation outline

### 3.1 Translator: emit fcU on RPC-fallback success

The RPC fallback path in `crates/translator/src/tasks/final_processor.rs` (and the RPC fallback in the receipt-fetch path) currently:

```rust
// pseudocode of today
if locally_built_hash != canonical_hash {
    let canonical_block = fetch_canonical_block(rpc_endpoint, block_num).await?;
    return canonical_block;  // substitute and continue
}
```

New behavior:

```rust
// pseudocode of new design
if locally_built_hash != canonical_hash {
    let canonical_block = fetch_canonical_block(rpc_endpoint, block_num).await?;
    // NEW: emit a fcU rewind to reth so the orphan is discarded
    let last_lib_hash = self.last_lib_hash.clone();
    let common_ancestor = walk_back_to_common_ancestor(
        locally_built_block.parent_hash,
        canonical_block.parent_hash,
    )?;
    self.execution_api.fork_choice_updated(
        head_block_hash:      common_ancestor.hash,
        safe_block_hash:      common_ancestor.hash,
        finalized_block_hash: last_lib_hash,
    ).await?;
    return canonical_block;
}
```

The walk-back finds the deepest common ancestor of the locally-built fork and the canonical chain. For depth-1 reorgs (47% of mainnet forks), the common ancestor is just `block_num - 1`. For deeper reorgs, follow each side's parent_hash chain (via local CL DB and via canonical RPC) until they meet.

After the fcU rewind:
- Reth discards every ExecutedBlock above `common_ancestor` from its in-memory buffer.
- The next `engine_newPayload` from the translator (with the canonical block replacing the orphan) succeeds; reth canonicalizes it.
- Subsequent blocks process normally.

### 3.2 Edge cases

- **Walk-back exceeds LIB:** if the common ancestor would be at or below LIB, that means LIB itself was reorged — impossible under Savanna BFT. Treat as fatal; abort with `Error::DeepReorgBeyondLIB`. Operator response: investigate (this should never happen on a healthy chain).
- **Local CL DB doesn't have the canonical chain's ancestors:** walk forward via canonical RPC's `eth_getBlockByHash` to locate them; build a temporary chain map in memory.
- **Concurrent reorgs:** if a second reorg arrives mid-walk, restart the walk. Implementations should not assume the chain is stable during fork resolution.
- **fcU returns INVALID:** treat as fatal; the orphan was already past persistence-threshold, MDBX is corrupt. Stop the CL and require manual intervention.
- **fcU returns SYNCING:** retry once; if still SYNCING, fall back to the original "skip and substitute" behavior, log_error, increment a metric.

### 3.3 Reth launcher change

Single-line edit in `/usr/local/bin/telos-reth-v2-mainnet-quick`:

```bash
# Before:
--engine.persistence-threshold 2 \
--engine.persistence-backpressure-threshold 16 \

# After:
--engine.persistence-threshold 20 \
--engine.persistence-backpressure-threshold 30 \
```

20 is well above worst observed fork depth (7). 30 keeps the backpressure margin reasonable. Memory cost: ~20 ExecutedBlocks held in `CanonicalInMemoryState` is small (hundreds of MB at most, even during heavy tx activity).

### 3.4 Translator config change

```toml
# Both mainnet-v2-quick and testnet-v2-quick configs:
rpc_fallback_sample_every_n = 1   # was 10; B6 deploy already in flight on testnet

# When this spec ships, also flip:
# (in raw_deserializer.rs source — currently irreversible_only=true post-PR-#71-rollback)
# irreversible_only: false,
```

Both source and config changes ship together as a single deploy.

---

## 4. Test plan

### 4.1 Unit tests

In `crates/translator/src/tasks/final_processor.rs`:

| Test | Setup | Expected |
| --- | --- | --- |
| RPC-fallback emits fcU on substitute | Mock canonical RPC returning hash Y; locally-built block has hash X | `engine_forkchoiceUpdated` called with `head=common_ancestor, safe=common_ancestor, finalized=last_lib` |
| Walk-back finds depth-1 ancestor | Local block N parent is A; canonical block N parent is also A | Common ancestor = N-1 with parent-hash A |
| Walk-back finds depth-3 ancestor | Local chain N-3 → N-2-fork → N-1-fork → N-fork; canonical chain N-3 → N-2 → N-1 → N | Common ancestor = N-3 |
| Walk-back beyond LIB returns Err | Configure LIB at N; common ancestor would be at N-2 | Returns `Error::DeepReorgBeyondLIB` |
| fcU INVALID response is fatal | Mock reth returning fcU INVALID | CL stops with `Error::ForkChoiceUpdated`; doesn't continue silently |
| fcU SYNCING retried once | Mock reth returning SYNCING then VALID | Second fcU succeeds, processing continues |

### 4.2 Integration test on testnet — synthetic fork

1. Deploy modified CL + reth flags to testnet quick. Pre-deploy snapshot first.
2. Confirm normal operation for 30 minutes with the canonical-comparison monitor.
3. Inject a synthetic fork via test harness:
   - Pause SHIP feed for 5 seconds during a known-empty-block window.
   - During the pause, manually craft an EVM block with the same height as a canonical block but different parent or different tx ordering.
   - Inject it into the translator's stream as if SHIP delivered it.
   - Confirm:
     - Translator's RPC fallback fires.
     - `engine_forkchoiceUpdated` is called with the correct common-ancestor hash.
     - Reth discards the synthetic block.
     - State at next canonical block matches canonical RPC.

This is the test the 2026-04-30 MVP didn't have. No deploy without it.

### 4.3 Real-fork validation on testnet — long soak

After unit + synthetic tests pass:

1. Deploy to testnet quick.
2. Start the canonical-comparison monitor (already running).
3. Wait for ≥10 real Antelope microforks on testnet-basel. Per the per-day rate (~3.8/day), this takes ~3 days.
4. For each real fork observed:
   - Confirm the translator's `disagreements_total` metric incremented appropriately.
   - Confirm the canonical-comparison monitor saw zero MISMATCH events (i.e., reth's state matched canonical at every check height during and after the fork).
   - Diff the on-disk MDBX state against canonical at qTip-50 immediately after the fork resolved. Should match.
5. Document each fork event with timestamp + depth + outcome in a log file.

Only after 10 consecutive forks pass cleanly does mainnet promotion become possible.

### 4.4 Mainnet promotion gate

- Pre-deploy snapshot of mainnet quick.
- Same config + binary as testnet (after 10 clean forks there).
- 48h additional soak on mainnet, monitoring for divergence.
- Mainnet has 6 forks/day, so a 48h window catches ~12 fork events. Any MISMATCH or `disagreements_total > 0` aborts the promotion and triggers rollback.

---

## 5. Effort estimate

Translator side:
- fcU emission in RPC-fallback success path: 2 days
- Walk-back-to-common-ancestor logic: 1 day
- Edge cases (deep-reorg-beyond-LIB, fcU INVALID/SYNCING handling): 1 day
- Unit tests: 1 day

Test infrastructure:
- Synthetic-fork harness: 2 days
- Test fixtures: 0.5 day

Soak windows (wall-clock, not effort):
- Testnet: ≥3 days for 10 real forks, plus debugging time per fork
- Mainnet: 48 h post-promotion

Plus B7 (multi-endpoint quorum) is recommended as a precondition — without quorum, this design still has single-point-of-trust on canonical RPC.

**Total: 7-10 days engineering + ~5-7 days wall-clock soak. Don't compress.**

---

## 6. Rollback plan (in case the deploy fails real-fork test)

Pre-deploy snapshot is the rollback. Per `PRE_DEPLOY_BACKUP_RUNBOOK.md`:

1. Stop services.
2. Restore reth datadir + CL DB from snapshot.
3. Restore launcher script + service unit.
4. `systemctl daemon-reload && start`.

Total rollback: ~2 minutes.

If the failure was caught quickly enough that no MDBX corruption has been persisted (i.e., fork resolution worked but something else broke), simpler revert: just flip `rpc_fallback_sample_every_n` back to 10 and revert `irreversible_only` to true. ~30 seconds, no MDBX touch.

---

## 7. Don't ship this until

- Savanna activation is publicly slipped past Q3 2026 (or whenever the planned activation is), AND
- B7 (multi-endpoint quorum) is shipped first, AND
- The synthetic-fork test in §4.2 passes, AND
- The 10-real-forks testnet validation in §4.3 passes.

If Savanna activates on schedule, this whole effort is wasted work. The right move is to **specify it now (this doc) and queue it** rather than implement speculatively.

---

## 8. Out of scope

- HSM-backed forwarder key handling (separate concern).
- Cross-DC node deployment (covered in `MULTI_NODE_DEPLOYMENT_PLAN_REFRESH_2026-05-01.md`).
- Replacing the `build_state` path with revm execution (see C9 spec — alternative, not necessary for this spec).
- Dynamic threshold tuning (e.g., increase persistence-threshold during high-fork-rate periods). Static value of 20 is sufficient given observed depth distribution.
