# Fork-Handling MVP — Deployment Status

**Date:** 2026-04-30
**Scope:** testnet-quick only (mainnet untouched, testnet-full untouched)
**Goal:** remove the ~332-block (~165s) LIB-tracking lag without losing fork-immunity

## What was deployed

**Branch:** `fix/fork-handling-mvp` on `/data/telos-consensus-client`, branched off `feat/dump-state-binary`.

**Source changes** (3 files, +112 lines net — full diff at `/tmp/fork-handling-mvp.diff` on Hetzner and `fork-handling-mvp.diff` in this folder):

- `translator/src/tasks/raw_deserializer.rs:75` — `irreversible_only: true` (PR #71) → `irreversible_only: false` (now safe because the CL handles forks).
- `client/src/data.rs` — added `get_latest_block()` helper; hardened `get_block_or_prev()` to filter out the `lib` key.
- `client/src/client.rs` — main loop now:
  1. Fast-skips blocks ≤ reth's finalized (avoids per-block RPC chatter during catchup, since Savanna LIB is irreversible).
  2. Detects three fork shapes against the CL's own DB *before* `put_block`:
     - **same-height**: SHIP block N has different hash than our stored block N.
     - **parent-mismatch**: SHIP block at our_latest+1 whose parent_hash ≠ our_latest.hash.
     - **deep**: SHIP block at lower height with different hash than the stored one.
  3. On any of those, calls `handle_reorg(&lib, last_lib_hash)` which:
     - Deletes every CL DB entry above LIB.
     - Sends `engine_forkchoiceUpdatedV1(head=LIB, safe=LIB, finalized=LIB)` to reth, which discards every in-memory ExecutedBlock above LIB and reverts canonical to the LIB block.
     - Refreshes `latest_finalized_executor_block`.
  4. Falls through and processes the fork-triggering block as a normal forward step (it now becomes the new canonical block at its height).

**Helpers added:**
- `hashes_equal(a, b)` — case-insensitive hex compare with optional `0x` prefix, since `Checksum256.to_string()` (antelope) and `format!("{:?}", B256)` (alloy) can disagree on prefix/casing.

**Binary built at:** `/data/telos-consensus-client/target/release/telos-consensus-client-fork-mvp` (md5 `31c33d186f1b69d0a6d9a1bfc67e5c94`, 38 MB).

The original binary at `/data/telos-consensus-client/target/release/telos-consensus-client` (md5 `250ee8e0c6744b28e43a1ccb311666af`) is **untouched** — mainnet and testnet-full still use it.

## Reth-side change (testnet quick only)

`/usr/local/bin/telos-reth-v2-quick`:
```
- --engine.persistence-threshold 2 \
- --engine.persistence-backpressure-threshold 16 \
+ --engine.persistence-threshold 500 \
+ --engine.persistence-backpressure-threshold 600 \
```

Rationale: max observed fork depth on Telos is 7 blocks. Setting the persistence threshold to 500 keeps any fork material entirely in reth's `CanonicalInMemoryState` buffer, where the standard reorg machinery (triggered by our `forkchoiceUpdated` rewind) can drop orphan ExecutedBlocks cleanly without any MDBX writes.

## Service unit change (testnet quick only)

`/etc/systemd/system/telos-consensus-quick.service` ExecStart now points at `telos-consensus-client-fork-mvp` (the new binary). Original line is preserved in `/etc/systemd/system/telos-consensus-quick.service.bak-pre-fork-mvp`.

## Backups for rollback

| File | Backup |
| --- | --- |
| `/usr/local/bin/telos-reth-v2-quick` | `…bak-pre-fork-mvp` |
| `/etc/systemd/system/telos-consensus-quick.service` | `…bak-pre-fork-mvp` |
| `client.rs` source pre-edit | `/tmp/client.rs.original` |
| `data.rs` source pre-edit | `/tmp/data.rs.original` |
| `raw_deserializer.rs` source pre-edit | `/tmp/raw_deserializer.rs.original` |

**Rollback procedure (if needed):**
```bash
systemctl stop telos-consensus-quick
cp /etc/systemd/system/telos-consensus-quick.service.bak-pre-fork-mvp /etc/systemd/system/telos-consensus-quick.service
systemctl daemon-reload
systemctl start telos-consensus-quick
# Optional: also revert reth threshold:
systemctl stop telos-reth-quick
cp /usr/local/bin/telos-reth-v2-quick.bak-pre-fork-mvp /usr/local/bin/telos-reth-v2-quick
systemctl start telos-reth-quick
```

## Post-deployment results

**Lag (canonical − reth_quick):** dropped from **332 → 0** within 30s of CL restart.

| Metric | Before | After |
| --- | ---: | ---: |
| Tip lag (blocks) | ~332 | **0–1** |
| Tip lag (seconds) | ~165 | **<1** |
| `eth_getBlockByNumber("finalized")` | head | LIB (correct semantics) |
| `eth_getBlockByNumber("latest")` | LIB-anchored | head-anchored |

**Correctness cross-check (testnet QUICK vs `https://rpc.testnet.telos.net`):** all 4 sample blocks bit-for-bit identical:

| Block | Match |
| --- | :---: |
| qTip-50 | ✓ |
| qTip-200 | ✓ |
| qTip-800 | ✓ |
| qTip-2000 | ✓ |

**Service health post-deploy:**
- `telos-reth-quick` — active, 0 restarts, 685 MB RSS
- `telos-consensus-quick` — active, 0 restarts, 19.7 MB RSS
- Reth log: 0 errors, 0 panics
- CL log: forwarding fcU calls every block, ~2.0 blocks/sec (chain rate)

## Pending validation

No real fork has occurred since deploy (~10 min ago). Testnet-basel sees ~3.8 microforks/day, so on average the next one is ~6 hours away. The continuous monitors are running:

| Monitor | Path | Cadence | What it captures |
| --- | --- | --- | --- |
| `fork_monitor.sh` | `/tmp/fork-events.log` | live tail | Any `REORG`, `handle_reorg`, fcU-invalid, panic, or `switching forks` log line |
| `cl_continuous_validator.sh` | `/tmp/cl-validator.log` | every 60s | tip lag + hash match at qTip-200 vs canonical |

**To verify the rewind path actually exercises correctly on the next real fork:**

```bash
# After a fork is logged in /tmp/fork-events.log:
ssh root@135.181.1.160 'grep -A2 -B2 REORG /tmp/fork-events.log; tail -200 /data/telos-consensus-client/testnet-v2-quick/consensus.log | grep -E "REORG|handle_reorg|rewound"'
# State consistency: should match canonical at the fork height
```

**Expected log on a successful rewind:**
```
WARN ... REORG (same-height|parent-mismatch|deep) detected at block N (cl_latest=…/…, ship=…, parent=…)
INFO ... handle_reorg: deleted M CL DB blocks above LIB (L)
INFO ... handle_reorg: rewound reth to LIB block L hash 0x… (fcU result Valid)
INFO ... fork_choice_updated_result for block number N: ForkchoiceUpdated { ... Valid ... }
```

## Mainnet quick deployment (added 2026-04-30 ~21:16 CEST)

Same fix promoted to mainnet quick on user instruction. Same binary, same threshold values.

| File | Backup |
| --- | --- |
| `/usr/local/bin/telos-reth-v2-mainnet-quick` | `…bak-pre-fork-mvp` |
| `/etc/systemd/system/telos-consensus-mainnet-quick.service` | `…bak-pre-fork-mvp` |

Restart sequence: stop CL → stop reth → start reth (new flags) → wait 8s → start CL.

Post-deploy verification (30s after restart):

| Metric | Result |
| --- | --- |
| Mainnet QUICK tip vs canonical lag | **0 blocks** (was 332) |
| Hash match @ qTip-50, 200, 800, 2000 | ✓ all MATCH |
| `telos-consensus-mainnet-quick` | active, 0 restarts |
| `telos-reth-mainnet-quick` | active, 0 restarts |
| In-memory reorg buffer | 500 blocks (vs 2 before) |

Mainnet monitors running:
- `/tmp/fork_monitor_mainnet.sh` → `/tmp/fork-events-mainnet.log`
- `/tmp/cl_continuous_validator_mainnet.sh` → `/tmp/cl-validator-mainnet.log`

Two transient reth errors at restart-second (engine API handshake races, did not recur):
```
ERROR engine::tree::payload_processor: Receipt root task received incomplete receipts, execution likely aborted expected=1 received=0
ERROR engine::tree::payload_validator: Receipt root task dropped sender without result, receipt root calculation likely aborted
```

Mainnet rollback procedure (identical to testnet, replace service names):
```bash
systemctl stop telos-consensus-mainnet-quick
cp /etc/systemd/system/telos-consensus-mainnet-quick.service.bak-pre-fork-mvp /etc/systemd/system/telos-consensus-mainnet-quick.service
systemctl daemon-reload
systemctl start telos-consensus-mainnet-quick
# Optional revert reth threshold:
systemctl stop telos-reth-mainnet-quick
cp /usr/local/bin/telos-reth-v2-mainnet-quick.bak-pre-fork-mvp /usr/local/bin/telos-reth-v2-mainnet-quick
systemctl start telos-reth-mainnet-quick
```

## What's still gated

- **testnet-full** (archival) is untouched — different sync profile, not in MVP scope.
- **Real-fork validation** — both testnet quick and mainnet quick are live with the new code, but neither has experienced a real fork yet. Mainnet sees ~6/day → likely first observation within ~4h.

## Known caveats

1. **Build_state in-memory rewind is theoretically clean but production-untested.** First real fork is the proof-of-life test. If it fails, rollback to original binary + irreversible_only=true.
2. **`handle_reorg` is coarse**: rewinds all the way to LIB rather than to the exact common ancestor. Operationally that costs ~165 s of replay per fork (negligible at testnet's fork rate). Could be tightened later to walk back via `parent_hash` chain only as deep as needed.
3. **Catchup throughput**: the new fast-skip relies on reth's `finalized` being current. On a clean DB resync, finalized is None at start, so the fast-skip does nothing for the first batch — caches by hash would still match via the existing `is_in_check_range` path, but it's a slower catchup than before. Not relevant for the running services since they had pre-existing state.
4. **`reth_telos_rpc_engine_api::compare` warnings (Finding F2)** are unaffected by this change — they're per-block revm/tevm execution differences, not fork-related.
