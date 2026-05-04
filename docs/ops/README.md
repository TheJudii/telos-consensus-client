# `docs/ops/` — operational documentation

This directory exists on the `archive/2026-04-30-fork-handling-mvp-attempted-and-reverted` branch as a forensic record of the 2026-04-30 fork-handling MVP incident plus the corrected operational practices that came out of it. **The source code on this branch must NOT be merged.** It was attempted and reverted; this branch preserves the artifact for reference only.

For the full timeline and corrected design, start with `POSTMORTEM_2026-04-30_fork_mvp.md`.

## Index

### Incident docs (2026-04-30 fork-handling MVP)

| File | Purpose |
| --- | --- |
| `POSTMORTEM_2026-04-30_fork_mvp.md` | Severity, root cause, timeline, what to do differently, action items. **Read first.** |
| `FORK_HANDLING_MVP_DEPLOYMENT.md` | Original deployment writeup (pre-rollback) — what was deployed, where, with what flags. |
| `fork-handling-mvp.diff` | The exact source diff that was deployed. Same content as the commits on this branch. |

### Production-grade operational practices

| File | Purpose |
| --- | --- |
| `PRE_DEPLOY_BACKUP_RUNBOOK.md` | Mandatory pre-deploy snapshot procedure. Run this before any reth/CL persistence-affecting change. |
| `PRODUCTION_READINESS_STATUS.md` | Live status doc. Updated 2026-05-01 to reflect the incident and current readiness. |
| `CLIENT_CONTRACT_LIB_TRACKING.md` | Client-facing contract for the v2 quick node's LIB-tracking semantics. |
| `MULTI_NODE_DEPLOYMENT_PLAN_REFRESH_2026-05-01.md` | Refresh of the original multi-node plan, with sequencing for an interim 2-node phase. |
| `RUNBOOK_C10_multi_node_infra.md` | Step-by-step runbook for the 2-node interim deployment. |

### Specifications for future work (NOT in this branch's source)

| File | Purpose |
| --- | --- |
| `SPEC_B7_multi_endpoint_canonical_validation.md` | Replace the single-trust on rpc.telos.net with 2-of-3 quorum across organizationally-distinct endpoints. |
| `SPEC_C8_fork_handling.md` | The CORRECT fork-handling design (replaces the broken MVP on this branch). Do not implement until Savanna instant finality activation slips materially past plan. |
| `SPEC_C9_build_state_inverse.md` | Either a `build_state` inverse, or switching to revm-execution mode. Defense-in-depth, lower priority than C8. |

### Investigations

| File | Purpose |
| --- | --- |
| `F2_INVESTIGATION_revm_tevm_compare.md` | Diagnosis of the 9k-warning/day revm/tevm compare warnings. Concludes they are structural to `build_state` mode, not bugs. Recommends formal acceptance + log-level downgrade. |

### Test reports

| File | Purpose |
| --- | --- |
| `RETH_V2_TEST_REPORT_2026-04-29_mainnet.md` | Pre-incident mainnet UAT report. 45/45 pass. Useful as a baseline reference for what "healthy" looks like. |

---

## Deployed but not in this repo

The following operational artifacts are running on the production host but their source lives in `https://github.com/TheJudii/telos-reth-v2` (or as ad-hoc scripts in the operator's working folder, not yet a versioned repo):

- `auto_heal_consensus_quick.sh` + `telos-autoheal.service` — installed at `/usr/local/bin/` and `/etc/systemd/system/`. Watches the consensus journal for `Executor hash mismatch` and restarts the affected services.
- `canonical-monitor.py` + `telos-canonical-monitor.service` — installed similarly. Continuously compares both quick nodes against canonical RPC.
- `telos-snapshot.sh` + `telos-snapshot.service` + `telos-snapshot.timer` — daily cold-copy snapshots of reth datadir + CL DB. 04:00 UTC. 7-day retention.

These should be moved into a versioned repo. Candidate: a fresh `telos-reth-ops` repo under TheJudii.

---

## Git history note

The single source-change commit on this branch (`0058b01...`) is the entire delta of the MVP:

- `client/src/client.rs` — fork detection + `handle_reorg` + `hashes_equal` helper.
- `client/src/data.rs` — `get_latest_block()` helper.
- `translator/src/tasks/raw_deserializer.rs` — `irreversible_only: false`.

If any of those small helper functions (notably `get_latest_block` in data.rs) end up being useful in a future correct implementation, cherry-pick them individually. Do NOT cherry-pick the fork-detection or the `irreversible_only` flip — those are the broken parts.
