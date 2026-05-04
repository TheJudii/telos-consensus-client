# Post-mortem — Fork-handling MVP deployment, 2026-04-30

**Severity:** Production state corruption on `reth-mainnet-v2-quick`. Recovered via 15-hour resync from chainspec. No user-visible impact (the affected node is a parallel quick-sync node; canonical mainnet RPC was unaffected throughout).

**Owner:** Justin (Claude assistant ran the deployment under instruction)

**Duration of impact:** ~17 minutes from deploy to corruption-detection-and-rollback (~21:14–21:31 CEST). Recovery window: 15 hours.

---

## 1. What happened

We attempted to remove the ~332-block tip-tracking lag (Finding F1) by reversing PR #71's `irreversible_only=true` setting and adding a fork-handling code path in the consensus client. The fork handler was deployed to testnet quick first, observed in steady-state for ~15 minutes, then promoted to mainnet quick on user instruction. Within minutes of mainnet deploy, a real Antelope microfork occurred and our fork-handler did **not** catch it. The translator silently committed forked-fork blocks to reth, which persisted them to MDBX. State at ~700 historical heights diverged permanently.

Detection came from a continuous validator script that compared QUICK's hash to canonical's hash at qTip-200 every 60 seconds. The first MISMATCH appeared 3 minutes after mainnet deploy. We rolled back the binary and reth flags, confirmed both services were back on the original code path, then resynced reth-mainnet-v2-quick from a seeded chainspec. Resync completed in ~15 hours.

---

## 2. Why the MVP failed

The fork-handler I added in `client.rs` only fired when SHIP delivered a block whose hash directly conflicted with one already in the CL DB at the same height (or whose `parent_hash` didn't match our stored latest). That model assumed every SHIP-delivered block was authoritative.

But the failure path on Telos is different. When Antelope DPoS reorgs:

1. SHIP delivers a block from the **losing** fork to the consensus client (the translator).
2. The translator builds an EVM block from that input. The EVM block's `parent_hash` chains correctly back to our previous block (because the loser shares the parent).
3. The translator's pre-existing safety net is `rpc_fallback_sample_every_n=10`: it cross-checks 1 in 10 *empty* blocks against canonical RPC, and **all** blocks containing transactions. Empty losing-fork blocks have a 90% chance of slipping through.
4. The translator hands the wrong block to reth via `engine_newPayload`. `parent_hash` is intact, so reth accepts it. fork-handler in `client.rs` sees a normal forward step, does nothing.
5. With `--engine.persistence-threshold=500` (the value I set to absorb in-memory reorgs), the wrong block survives long enough in the in-memory buffer for reth to commit it. After 500 blocks, the wrong state hits MDBX permanently.

So the MVP was solving the wrong problem: it caught reth-vs-CL hash conflicts, but the actual failure mode is at the translator → CL boundary, before any block reaches reth's engine API with a conflicting hash.

PR #71 worked precisely because `irreversible_only=true` made step 1 impossible. Removing it without addressing step 3 was the bug.

---

## 3. Timeline (UTC)

| Time | Event |
| --- | --- |
| 19:02 | testnet quick deploy: new binary + persistence-threshold=500 + monitors |
| 19:03 | testnet quick: lag drops 332 → 0, all sample blocks match canonical |
| 19:14 | mainnet quick deploy: same change, same monitors |
| 19:14:17 | mainnet quick: lag drops 332 → 0 |
| 19:15:39 | First translator hash-mismatch logged: block 465,135,670 |
| 19:16:00 | Second mismatch: block 465,135,710 (RPC fallback fired) |
| 19:16:09 | Third: block 465,135,730 |
| 19:17:44 | Fourth: block 465,135,919 |
| 19:17:46 | Validator caught its first MISMATCH at qTip-200=465,135,723 |
| 19:21 | Decision to roll back. CL + reth restored to original binary + flags on both nodes. |
| 19:31 | Re-check after rollback: corruption persists at 465,135,723. Resync required. |
| 23:19 | Resync started. Quarantined corrupted state to `/data/backups/2026-04-30-corrupted-pre-resync/`. |
| **2026-05-01 14:30** | **Resync complete.** lag=336 (LIB-tracking baseline), 8/8 verification samples match canonical. |

---

## 4. Why testnet escaped, mainnet didn't

testnet-basel had **zero microforks** during the 30-minute window the MVP was active (verified in nodeos journals). Mainnet had at least 4 (logged hash-mismatches at blocks 670, 710, 730, 919 — and presumably more that the 1-in-10 sampling didn't catch).

Microfork rate on Telos mainnet is ~6/day = one every ~4 hours. In any 17-minute window the probability of *zero* forks is roughly `e^(-6 * 17/1440) ≈ 93%`. So we had ~7% chance of escaping mainnet too. We didn't.

The lesson is not "wait longer on testnet." It's "demonstrate that the fork-handling code actually fires on a real fork before promoting." 30 minutes of steady-state with no fork events proves only that the no-fork case works.

---

## 5. What we should have done differently

1. **Treat steady-state operation as zero evidence.** The fork-handler's job is fork handling. Until a real fork is observed, the deploy hasn't been validated for its purpose.
2. **Pre-deploy snapshot of `reth-datadir` + CL DB.** This is the lesson Justin called out: a 60-second cold copy would have made rollback a 5-minute operation instead of a 15-hour resync. Now formalised as a runbook checklist (`PRE_DEPLOY_BACKUP_RUNBOOK.md`).
3. **Match the threshold change to the failure window.** I set `persistence-threshold=500` to absorb in-memory reorgs, but that **made things worse** by holding 500 blocks of potentially-wrong state in-memory long enough for the persistence layer to commit it. With threshold=2 (original), only ~2 blocks of forked state can be committed before the legacy `ExecutorHashMismatch` would have surfaced loudly. The threshold bump should have been gated on the fork-handler being proven correct, not deployed simultaneously.
4. **Read the existing safety nets fully.** I missed that `rpc_fallback_sample_every_n=10` exists and is the actual translator-level safeguard. Setting it to 1 would have caught all four (and presumably more) microforks at translator-build time, where the fix is cheap. The MVP's fork-handler is structurally placed too late in the pipeline.
5. **Acknowledge the deploy-too-fast invitation.** The user had no obligation to ask "are you sure?" — that's the operator's prerogative. I should have answered with the demonstrability gate (item 1), not gone ahead. That's on me, not the user.

---

## 6. Was Savanna already in scope?

Yes — Telos's Savanna instant-finality activation makes this whole class of failure essentially free of cost: LIB lag drops to ~1 second once Savanna is fully active. PR #71's `irreversible_only=true` is correct *and* cheap once Savanna lands. Until then, the lag we're trying to remove is the safety margin that prevents this exact bug.

The original recommendation in the deployment doc was "keep PR #71 until Savanna lands." Today validates that recommendation. We should not retry a head-tracking deploy until either:

- Savanna activation closes the gap naturally, or
- A demonstrably-correct fix is shipped (see §7) and observed across ≥10 real microforks on testnet.

---

## 7. What a correct fix needs (if we revisit later)

Three coupled changes; any one alone is insufficient:

1. **`rpc_fallback_sample_every_n = 1`** in the consensus config. Every block validated against canonical RPC at the translator. The 10× sampling was an optimization that's only safe under PR #71.
2. **Wire fcU rewind into the translator's RPC-fallback success path.** When the translator detects a hash mismatch and fetches the canonical block, it must also emit `engine_forkchoiceUpdated(head=canonical_block)` to tell reth to discard the previously-submitted forked block. Today's RPC-fallback only substitutes the next block — it leaves the orphan in reth's chain.
3. **Persistence-threshold sized to the worst observed fork depth, not above it.** With max observed depth=7, threshold=20 (or smaller) is enough to keep all reorgs in-memory. 500 was overkill and increased the corruption window when the fork-handler missed.

Plus the operational gates:

- Pre-deploy snapshot of `reth-datadir` + CL DB (formalised, not a note).
- Testnet must observe ≥10 real microforks with the fork-handler firing correctly each time before mainnet promotion.
- Tip-lag and validator monitors run for ≥48 h on testnet before mainnet deploy.

Realistic timeline: 1–2 weeks of focused engineering, plus the soak window. **Not recommended** unless Savanna activation slips far past the current schedule.

---

## 8. Action items

- [x] Roll back both nodes to pre-MVP state.
- [x] Resync mainnet quick from chainspec; verify 8 sample blocks across the corruption window match canonical.
- [x] Document the failure mode and the corrected design (this doc).
- [x] Add pre-deploy backup pattern to the runbook (`PRE_DEPLOY_BACKUP_RUNBOOK.md`).
- [x] Update `PRODUCTION_READINESS_STATUS.md` to reflect F1 status (still open; harder than initially scoped).
- [ ] Decide: pursue corrected fix (§7) or wait for Savanna. **My recommendation: wait for Savanna.**
- [ ] If pursuing a corrected fix later: open a tracking ticket with §7 as the spec, plus the soak gates.
- [ ] Decide on retention of `/data/backups/2026-04-30-corrupted-pre-resync/` (~917 MB). Default: keep 30 days, then delete.
