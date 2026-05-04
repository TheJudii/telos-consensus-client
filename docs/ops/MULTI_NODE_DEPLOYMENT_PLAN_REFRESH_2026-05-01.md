# Multi-Node Deployment Plan — Refresh 2026-05-01

Companion to `MULTI_NODE_DEPLOYMENT_PLAN.md` (2026-04-19). This document audits the original plan against current state, marks which line items are done, partially done, or blocked, and re-sequences the remaining work with concrete next steps.

**Bottom line:** the original plan still describes the right destination. None of it needs to be torn up. But ~6 weeks have passed and almost none of the multi-node work has been started — current production state is still single-host on `135.181.1.160`. With the lessons from the 2026-04-30 fork-handling MVP incident factored in, the right next steps are smaller and more conservative than the plan's original §Sequencing implied.

---

## 1. Status by section of the original plan

| Original plan section | Status | Notes |
| --- | --- | --- |
| Topology (3 serving + 1 archive + 1 standby) | **Not started.** Still single-node. | Original recommendation stands. No changes needed. |
| Per-node forwarder accounts (`forward.1`..`forward.4`, `forward.ar1`) | **Not started.** Existing nodes use `rpc.evm@rpc`. | Owner key required; on-chain ops needed. |
| Load balancer tier (nginx/Cloudflare/HAProxy) | **Not started.** | LB choice still open; needs a decision. |
| Routing rules (write-pool / read-pool) | **Not started.** | Will be written after LB choice. |
| Health checks (`/healthz/forwarder`) | **Partially.** `rpc_health_aggregator.py` written but not deployed. | One-step deploy when first node-2 stands up. |
| Snapshot pipeline (versioned bucket, manifest sigs) | **Partially.** Daily local snapshots are now automated by `telos-snapshot.timer` (see this folder). Bucket-uploaded manifest-signed bundles are not. | Local snapshots are useful for single-node disaster recovery; bucket pipeline is what's needed for bootstrapping new nodes from a known good seed. |
| Bootstrap script (`scripts/bootstrap_node.sh`) | **Not written.** | Highest-leverage missing piece. |
| Forwarder account provisioning (`cleos system newaccount` + `set action permission`) | **Not started.** | Requires owner key; ~30 min of cleos work per account. |
| Rolling upgrade procedure | **Documented, not exercised.** | Will be tested first time node-2 is upgraded. |
| Failure-mode response table | **Documented.** Autoheal handles one specific case. | Most other failure modes still rely on operator response. |
| Cost estimate (~$1,350/mo) | **Still accurate** for the original 5+1 topology. | See §3 for a scaled-back option. |

---

## 2. New items not in the original plan

Things we've learned about between 2026-04-19 and 2026-05-01 that the original plan doesn't cover:

1. **Pre-deploy snapshot pattern.** `PRE_DEPLOY_BACKUP_RUNBOOK.md` is now mandatory before any persistence-affecting deploy on any node. The multi-node bootstrap script must include the snapshot step before it ever runs the deploy.
2. **Canonical-comparison monitor.** Now running on the box; should run on every node in the multi-node topology, with the per-node results aggregated into the LB's health-check signal so a divergent node automatically drops from the write-pool.
3. **F1 (head-tracking) is deferred.** The original plan implicitly assumed all nodes would track head. Per the 2026-04-30 post-mortem, all reth-quick nodes will track LIB until Savanna full activation. Multi-node deploy doesn't change this. Update the client contract to apply pool-wide.
4. **F2 (compare warnings) is structural noise.** Doesn't affect multi-node deployment but the per-node logging needs filtering or downgrading or every node will emit ~10k WARN/day to syslog.
5. **`/data/tmp/telos-extra-fields` cleanup.** Each new node will accumulate this directory at ~30 GB/month. Needs a lifecycle-aware cleanup as part of the per-node provisioning.
6. **Reth-full archival recovery.** Was "in progress" on Apr 19; still in progress today. **The archive node in the topology cannot stand up until reth-full archival is verified.** This is the long-pole item.

---

## 3. Scaled-back interim topology — recommended next step

Going from single-node to 5+1 nodes in one shot is a big jump. Recommend an intermediate **2-node phase** that captures most of the redundancy benefit at a fraction of the cost and time:

```
                          ┌──────────────────────────────────┐
                          │      Public load balancer         │
                          │   - read pool (both nodes)        │
                          │   - write pool (both nodes)       │
                          │   - active/active                 │
                          └─────────────┬────────────────────┘
                                        │
        ┌───────────────────────────────┼───────────────────────────────┐
        │                                                               │
   ┌────▼────┐                                                     ┌────▼────┐
   │ node-1  │                                                     │ node-2  │
   │(current)│                                                     │  (new)  │
   │ Hetzner │                                                     │  TBD    │
   └─────────┘                                                     └─────────┘
```

**Why 2 nodes first:**
- Survives one-node failure (the most common availability incident).
- Tests the LB tier, rolling-upgrade procedure, snapshot bootstrap, and cross-node consistency monitors at small scale.
- Validates the bootstrap pipeline before scaling to 4–5 nodes.
- ~$300/month vs $1,350 for full topology.

**What this 2-node phase doesn't give you:**
- Archive reads (still single-node-only on the existing host).
- Capacity to handle one node failure during a maintenance window (you'd have zero redundancy if node-1 needs maintenance and node-2 is rebooting).
- The on-chain audit clarity of per-node forwarder accounts (could still be done at 2-node scale; cheap to add now or later).

After 2-node has been operational for 30 days without incident, scale to the original 5+1 plan.

---

## 4. The next 5 actionable items

In sequence, with explicit owners (assumed: ops/team-lead approval gates on items 1, 2, 6; everything else can be implemented by the team).

| # | Item | Effort | Dependencies | Outcome |
| --- | --- | --- | --- | --- |
| 1 | **Decide on LB.** Pick nginx/HAProxy/Cloudflare/etc. The choice constrains everything downstream (TLS, cert mgmt, rate-limiting capability). | 1 day decision + 1 day prototype | Operator preference + budget | LB tier provisioned in test mode |
| 2 | **Provision `forward.1` and `forward.2` on-chain.** Owner key required. Two accounts, two scoped `fwd` permissions, two new private keys checked into a vault. | 0.5 day | Owner key access | Node-2 ready to receive its key |
| 3 | **Write `bootstrap_node.sh`.** Single script that takes a target hostname, pulls latest local snapshot, copies it over, installs services, configures forwarder key, starts services, validates against canonical, and registers with the LB. Test on a throwaway VM first. | 2 days | Daily local snapshot already running ✅ | Bootstrap pipeline verifiable |
| 4 | **Build the snapshot bundle pipeline.** Extends today's local-only daily snapshot to also upload a versioned manifest-signed bundle to a bucket (S3-compatible). Required for bootstrapping new nodes from a known seed without trusting the source node directly. | 2 days | Bucket procurement (~1 hour with AWS/Backblaze/etc.) | Bootstrap can pull from bucket |
| 5 | **Stand up node-2 in read-only mode.** Run for 7 days under LB read-only traffic. Don't add to write-pool yet. | 1 week wall-clock | Items 1-4 | Node-2 proves bootstrap + LB read path |

Items 6+ are the original plan, but only after node-2 has been stable for ≥7 days:

| # | Item | When |
| --- | --- | --- |
| 6 | Add node-2 to write-pool. | After 7 days clean read-only |
| 7 | Provision and bootstrap node-3, node-4. | Per-original plan, 1 week apart |
| 8 | Archive node bootstrap (gated on reth-full archival recovery being verified — currently the long pole). | Whenever archival recovery completes |
| 9 | Migrate existing single-node host to become node-5 / standby. | After all replacements are stable |

---

## 5. Cost — current vs interim vs full

| Phase | Monthly | Note |
| --- | ---: | --- |
| Today (single-node Hetzner) | ~$150 | The baseline we're paying now |
| Interim 2-node + LB | ~$300–400 | One additional node + LB or Cloudflare paid tier |
| Original plan (5 + 1 archive + LB) | ~$1,350 | 9× current; full redundancy + archive |

---

## 6. Open questions for the operator

These need resolution before §4 can start:

1. **LB choice and DC.** Same DC as Hetzner (e.g. Hetzner Cloud LB) for low latency, or geographically distributed for region failure tolerance? Currently single-DC is acceptable per the plan's "Out of scope" section.
2. **Bucket provider.** AWS S3, Backblaze B2, Wasabi, or self-hosted MinIO. Cost is small (~$50/mo for 2 TB); choice depends on operational preference and trust model for snapshot signatures.
3. **DNS strategy.** Currently no public DNS for v2 quick endpoints. When does that go live, and via what registrar / management? Affects the LB cert chain.
4. **When to pursue F1 (LIB-tracking removal).** If Savanna activation lands inside the 2-node phase, the lag becomes irrelevant naturally. If it doesn't, decide whether to invest in the corrected fork-handling fix from `POSTMORTEM_2026-04-30_fork_mvp.md` §7.

---

## 7. What's *not* changing

The original plan's §Topology, §Per-node identity, §Routing rules, §Bootstrapping procedure, §Forwarder account provisioning, §Rolling upgrades, and §Failure modes sections are still correct. This refresh is additive: it's about *sequencing* the work so smaller wins land sooner and lessons from 2026-04-30 are baked in before scale.
