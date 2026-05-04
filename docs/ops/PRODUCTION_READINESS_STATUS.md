# telos-reth v2 — Production Readiness Status

**As of:** 2026-05-01 (post-incident) ⟵ updated; prior 2026-04-29 mainnet UAT and 2026-04-19 testnet body retained below
**Validation host:** Hetzner `135.181.1.160`
**Mainnet binary:** `reth/v2.0.0-4e09679/x86_64-unknown-linux-gnu`
**Testnet branch (prior):** `fix-keccak-empty-eoa` (HEAD `9a5605d`)
**Summary:** **~95% ready for testnet, ~80% for mainnet** — unchanged. F1 (tip-tracking lag) attempted and reverted; remains open.

---

## Update 2026-05-01 — fork-handling MVP attempt + incident

Attempted to close F1 by reversing PR #71's `irreversible_only=true` and adding a fork-handling code path. Deployed to testnet quick (no fork events during window — survived) then to mainnet quick (microfork hit within minutes — state diverged at ~700 historical heights). Rolled back both nodes to the pre-MVP binary; mainnet quick required a 15-hour resync from chainspec to clear the divergence. Recovery verified: 8/8 spot-check blocks match canonical, lag back to LIB-tracking baseline of 336, both services healthy.

Full post-mortem in `POSTMORTEM_2026-04-30_fork_mvp.md`. Pre-deploy snapshot pattern formalised in `PRE_DEPLOY_BACKUP_RUNBOOK.md` so we don't repeat the rollback cost.

| Status changed | New state |
| --- | --- |
| F1 (tip lag) | Still open. No partial fix exists. Recommendation: wait for Savanna full activation rather than retry the MVP. |
| F3 (autoheal) | ✅ Installed and active on Hetzner: `/usr/local/bin/auto_heal_consensus_quick.sh` + `telos-autoheal.service`. Watches `telos-consensus-quick` journal for `Executor hash mismatch`. |
| F5 (disk pressure) | Partial: `/var/log` cleanup freed ~4.6 GB; `/data/tmp/telos-extra-fields` (54 GB) flagged for separate lifecycle-aware cleanup. |
| Pre-deploy backup runbook | ✅ Written, committed to working folder. Mandatory before any reth/CL persistence-affecting change. |

Lessons applied going forward:

- Steady-state operation of a fork-handler is zero evidence; demonstrate against a real fork before promotion.
- Pre-deploy snapshot of `reth-datadir` + CL DB is mandatory, not optional.
- Don't bump persistence-threshold and change fork-handling semantics in the same deploy — gate one on the other.
- Autoheal does not make `irreversible_only=false` safe (it operates downstream of where the failure occurs).

---

## Update 2026-04-29 — mainnet UAT

---

## Update 2026-04-29 — mainnet UAT

Read-only mainnet UAT today: **45/45 PASS**, conformance bit-for-bit, state persistence holds at qTip-30000 (the session-4 fix is alive on mainnet too), block hash + stateRoot + receiptsRoot match canonical at 5 sample blocks. Forwarder retry verified at design level: synthetic harness passes 3 scenarios (transient failure, persistent failure, happy path); production journals show 0 natural retry events over 14 days (nodeos has been reliably reachable). Mainnet QUICK currently runs ~331 blocks (~165s) behind canonical — stable, not drifting, but materially larger than testnet's 1-block lag.

| Mainnet axis | Status |
| --- | --- |
| Services up (nodeos-mainnet, telos-consensus-mainnet-quick, telos-reth-mainnet-quick) | ✅ all active |
| Block correctness (hash/stateRoot/receiptsRoot vs canonical) | ✅ 5/5 sample blocks match |
| State persistence at qTip-30000 | ✅ matches canonical for busy EOA |
| Conformance (offline signing payload) | ✅ packed_trx_hex bit-equal |
| Forwarder retry logic | ✅ enabled + design-verified (synthetic) |
| Tip lag tracking | ⚠ 331 blocks (~165s) stable — Finding F1 (attempted fix on 2026-04-30, reverted; see post-mortem) |
| revm/tevm compare warnings | ⚠ logged during active windows; persisted state still correct — Finding F2 |
| Operational hygiene (autoheal, testnet nodeos, disk, swap) | ⚠ Findings F3 (✅ installed 2026-05-01), F4–F6 |
| Forwarder live test | 🔒 gated on funded probe key (G1) |
| Forwarder load test | 🔒 gated on funded senders (G2) |
| Key rotation rehearsal | 🔒 gated on owner key (G3) |

Full report and artifacts: `RETH_V2_TEST_REPORT_2026-04-29_mainnet.md` and supporting files in this folder.

---

## Prior status (2026-04-19, testnet)

---

## What "production ready" means here

| Axis | Definition | Status |
| --- | --- | --- |
| Functional correctness | EVM tx round-trip through our stack matches canonical | Verified on testnet |
| State persistence | `build_state` + `trust_consensus` retains hashed state | Verified via regression test + live canonical hash match |
| Mempool acceptance | EOA-originated txs accepted by our mempool | Fixed in `fcd3cc7` (KECCAK_EMPTY) |
| Forwarder path | `eth_sendRawTransaction` → nodeos → canonical inclusion | Verified in block 419,580,503 |
| Tip tracking | reth-quick within 1 block of canonical over multi-hour window | Verified |
| Archival recovery | reth-full can recover state history back to block ~143M | Recovery nodeos in progress |
| Observability | 72h sustained probe data | Harness deployed, run pending |
| Load tolerance | Measured concurrent forwarder throughput and failure modes | Test written, run pending |
| Security review | Signer key exposure, config hardening | Reviewed; actions queued |
| Operational runbook | Documented recovery paths for known failure modes | Written |
| HA topology | Horizontal scale model beyond single-node | Plan written |

---

## Readiness by area

### 1. Core consensus + state — **100%**

- PR #10 merged: trie short-circuit preserving `BundleState` on the `trust_consensus` path.
- Regression test (`21950e9`) guards against regression.
- Live canonical hash match confirmed; no state divergence observed over multi-hour windows.

### 2. Mempool — **100%**

- `KECCAK_EMPTY` fix for empty EOAs landed in `fcd3cc7`.
- Forwarder live test confirms an empty-EOA-originated tx is accepted, wrapped, forwarded, and included on canonical.

### 3. Forwarder path (Antelope signing + submission) — **95%**

- Full implementation in `crates/telos/rpc/src/{antelope,telos_client}.rs` (commit `3cfb569`).
- Offline conformance binary (`9a5605d`) asserts signing payloads are bit-for-bit correct.
- Live end-to-end tx included on canonical testnet (tx `0xc5244ffbec64df985abaf2da8774956019636dc4c7fc5b4b7f3326c9268062af`, block 419,580,503, 1.3s end-to-end).
- **Not yet measured:** high-concurrency behavior. Load test script (`forwarder_load_test.py`) is written and ready; needs a funded set of sender accounts + a green window to run.

### 4. reth-full archival — **70%**

- Recovery nodeos running under systemd (`nodeos-testnet-basel-recovery.service`) against the preserved blocks.log, using `--hard-replay-blockchain`.
- Phase 1 (block-log verification) was at ~62% at last check; Phase 2 (state rebuild) follows.
- When recovery nodeos reaches tip, cutover is: point `consensus-full`'s config at SHIP port 18082, restart, then monitor reth-full catch-up.
- **Blocker on mainnet readiness, not testnet** — the current validation deployment serves testnet reads correctly from reth-quick.

### 5. Consensus-quick stability — **100%**

- Root cause identified for Apr 19 14:09–14:16 CEST restart loop: executor hash mismatch after reth-quick in-memory state diverged from durable state (see runbook §2).
- Recovery path documented: `systemctl restart telos-reth-quick; sleep 10; systemctl restart telos-consensus-quick`.
- Self-healed naturally in the observed incident.
- **Automated:** `auto_heal_consensus_quick.sh` + `telos-autoheal.service` tails the journal, detects the known bad pattern, and runs the restart sequence with a 30-minute cooldown and a 3-attempts-per-2-hours escalation gate.

### 6. Observability — **90%**

- Cron `/data/scripts/health-check.sh` already running every 10 min (monitors both 8577 and 8677).
- `forwarder_observability.py` + systemd timer written to sample every 5 min with a real burn-tx probe — 72h run begins once installed and a probe-account key is provisioned.
- Dashboard aggregator (`rpc_health_aggregator.py`) ready to expose `/healthz` + `/metrics` on port 9090; dashboard-integration doc written.
- **Not yet done:** dashboard wiring to consume the aggregator (depends on dashboard repo).

### 7. Security — **75%**

- Written review (`SECURITY_REVIEW_SIGNER_KEY.md`) covers 6 findings (F1–F6) with recommended actions.
- F1 (static long-lived key — High) and F6 (config file perms — Medium) are the most important; both have concrete action items listed.
- **Rotation automation ready:** `rotate_forwarder_key.sh` implements the rotation checklist end-to-end (generate → add-key → config-update → restart → validate → remove-old-key), supports `--dry-run`, and auto-rolls-back if the post-rotation forwarder smoke test fails.
- **Not yet done:** on-chain creation of the scoped `forward@fwd` permission, first rotation run, config perms to 0600, systemd hardening on the reth services.
- **Blocker on mainnet readiness** until F1 + F6 are executed.

### 8. Operational runbook — **100%**

- `OPERATIONAL_RUNBOOK.md` covers 10 scenarios (blocks.log corruption, executor hash mismatch, consensus-full first-block error, mempool EOA rejection, forwarder failure, disk-fill, restart order, health probes, cron monitor, escalation).
- Matches current systemd unit structure on the hetzner box.

### 9. Horizontal scaling / HA — **plan only, 0% deployed**

- `MULTI_NODE_DEPLOYMENT_PLAN.md` written: per-node forwarder accounts, LB topology, rolling upgrade pattern, cost estimate, 6–8 week deployment sequencing.
- **Not applicable to single-node testnet validation** — required before any mainnet launch.

### 10. PR submission — **artifacts ready, user submits**

- `PR_telos_reth_v2_production_readiness.md` describes the 5 commits, file diff summary, live validation results, and how to review.
- Compare URL: https://github.com/TheJudii/telos-reth-v2/compare/claude/telos-reth-fork-jedxC...fix-keccak-empty-eoa?expand=1
- Open that link in a browser to file the PR; body text is in the markdown file.

---

## Overall percentage

**Testnet production readiness: ~90%.**
Gap is entirely non-functional items (72h observability data, load numbers, executing the security action items). The stack is functionally correct end-to-end and has been serving correct state for multi-hour windows.

**Mainnet production readiness: ~70%.**
Gap items:

- reth-full archival recovery must complete (currently in progress; hours of wall-time remaining).
- Security F1 + F6 must be executed (custom forward permission, 0600 config, systemd hardening).
- Multi-node deployment must be stood up (6–8 weeks of project work).
- Load test numbers must be collected and be within target.

---

## Deliverables in `outputs/`

| File | Purpose | Status |
| --- | --- | --- |
| `PR_telos_reth_v2_production_readiness.md` | PR body for the working branch | Ready |
| `OPERATIONAL_RUNBOOK.md` | 10 scenarios, on-box runbook | Ready |
| `SECURITY_REVIEW_SIGNER_KEY.md` | 6 findings + rotation checklist | Ready |
| `MULTI_NODE_DEPLOYMENT_PLAN.md` | HA topology, sequencing, cost | Ready |
| `forwarder_live_test.py` | End-to-end forwarder smoke test | Passing |
| `forwarder_load_test.py` | Concurrent load test | Ready to run |
| `forwarder_observability.py` | Periodic burn-tx probe | Ready to deploy |
| `telos-forwarder-obs.service` | Systemd unit for obs probe | Ready |
| `telos-forwarder-obs.timer` | Systemd timer (5 min cadence) | Ready |
| `install_observability.sh` | Deploy script for the obs probe | Ready |
| `rpc_health_aggregator.py` | Local HTTP endpoint fusing both reth ports | Ready to deploy |
| `rpc-health-aggregator.service` | Systemd unit for the aggregator | Ready |
| `DASHBOARD_INTEGRATION.md` | How to wire the external dashboard | Ready |
| `grafana_dashboard.json` | Grafana dashboard consuming `/metrics` | Ready |
| `auto_heal_consensus_quick.sh` | Journal-tailing self-heal for consensus-quick | Ready |
| `telos-autoheal.service` | Systemd unit for the autoheal daemon | Ready |
| `rotate_forwarder_key.sh` | End-to-end forwarder key rotation | Ready |
| `generate_load_test_senders.py` | Senders generator / funding helper | Ready |
| `deploy_all.sh` | One-shot deploy of ops bundle to hetzner | Ready |
| `PRODUCTION_READINESS_STATUS.md` | This file | Ready |

## Next actions, shortest path to 100% testnet-ready

1. Submit the PR via the compare URL (5 min, outside this session).
2. Fund a dedicated probe account with ~2 TLOS (5 min).
3. Run `PROBE_KEY=0x... HETZ_HOST=135.181.1.160 ./deploy_all.sh` from the outputs directory (5 min; deploys observability probe, aggregator, autoheal daemon in one shot).
4. Let the 72h observability run collect data (passive).
5. `python3 generate_load_test_senders.py --generate 5 --out senders.json` → fund the printed addresses → `python3 forwarder_load_test.py --senders senders.json` (~20 min, exercises concurrency).
6. Apply SECURITY_REVIEW action items 1 and 2 — config file `chmod 0600`, systemd hardening, dedicated `reth` user (30 min + restart window).
7. First key rotation via `rotate_forwarder_key.sh --dry-run` → rehearse → real run (60 min, requires owner key).

After steps 1–7: testnet ready at 100%, mainnet at ~85% (remaining: reth-full archival recovery verified, multi-node deployment).
