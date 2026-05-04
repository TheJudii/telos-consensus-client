# Client contract — telos-reth v2 quick (LIB-tracking mode)

**Audience:** application developers and integrators consuming `https://<reth-v2-quick-rpc>` (mainnet or testnet).

**Status:** active design as of 2026-05-01. Subject to change once Savanna instant finality activates fully on Telos mainnet (see §6).

This document defines what the reth-v2 quick node *guarantees* and what it explicitly *does not*. Read it before integrating; the semantics differ in important ways from a default Ethereum / EVM RPC node and from the existing `https://rpc.telos.net` v1 production endpoint.

---

## 1. TL;DR

The reth-v2 quick node serves **only finalized state**. Its `eth_blockNumber` returns the height of the most recently finalized (LIB) block, not the head of the chain. This is a deliberate design choice — it prevents a class of state-corruption failure that would otherwise occur during Antelope DPoS microforks.

The cost is a **bounded ~165-second lag** behind the canonical chain head. Both the v2 quick node and the canonical chain advance at the same rate; the offset is constant, not drifting.

If you need head-of-chain semantics (sub-second latency, willingness to handle reorgs), use the v1 production RPC at `https://rpc.telos.net`.

---

## 2. Block-tag semantics

The standard Ethereum engine API defines three block tags: `latest`, `safe`, `finalized`. On a default Ethereum node these are distinct points on the chain. On reth-v2 quick they collapse to the same value:

| Tag | Definition on reth-v2 quick | Definition on v1 prod (`rpc.telos.net`) |
| --- | --- | --- |
| `latest` | The most recently finalized (LIB) block | The most recent head block (~6s lag from production) |
| `safe` | Same as `latest` (LIB) | A few blocks behind head |
| `finalized` | Same as `latest` (LIB) | LIB |
| `pending` | Not supported | Not supported |
| `earliest` | Genesis of the seeded chainspec (block 464,467,181 mainnet, 419,000,000 testnet) | Real chain genesis (block 1) |

**Implication:** an `eth_getBlockByNumber("latest")` on reth-v2 quick returns the same block as `eth_getBlockByNumber("finalized")`. Tooling that polls `latest` for tx confirmation will see confirmation only after the tx has reached LIB.

---

## 3. Tip-tracking semantics

- The v2 quick node tracks the canonical chain at exactly the **LIB offset** — currently ~330 blocks (~165 seconds at 0.5-second block time) behind the chain head. This offset is **set by Antelope's BFT finalization window**, not by node performance.
- The offset is **stable, not drifting**. The v2 quick node and the canonical head advance at the same rate (~2 blocks/sec); the gap is constant.
- The offset will collapse to roughly **one block (~0.5 seconds)** once Savanna instant finality is fully activated on Telos mainnet. At that point this document's contract is unchanged in spirit but the latency cost goes to ~zero.

---

## 4. What the v2 quick node *does* guarantee

| Guarantee | Description |
| --- | --- |
| **Bit-for-bit canonical state** at every block returned. State at any historical height matches `https://rpc.telos.net` exactly. |
| **No reorg exposure.** Once a block is returned by this node, it cannot be subsequently reverted. |
| **Receipt/log correctness.** `eth_getTransactionReceipt`, `eth_getLogs` on returned blocks return values consistent with canonical at that height. |
| **Forwarder path.** `eth_sendRawTransaction` is intercepted and forwarded to the local nodeos via the Antelope `eosio.evm::raw` action. Returns the EVM transaction hash synchronously after native submission succeeds. The forwarder retries on transient nodeos failures (6 attempts, exponential backoff, ~3.15 s worst case before giveup). |
| **State persistence at any depth.** State at qTip-30,000 (and deeper, up to the seeded-chainspec genesis) is queryable and matches canonical. |

---

## 5. What the v2 quick node *does not* guarantee

| Non-guarantee | What this means in practice |
| --- | --- |
| **No head-of-chain visibility.** A transaction included in the chain is not visible via this node until it reaches LIB (~165 s post-inclusion). Polling `eth_getTransactionReceipt` immediately after `eth_sendRawTransaction` returns success will return `null` until LIB advances past the inclusion block. |
| **No `pending` tag.** Mempool state is not exposed. Use the forwarder's response or the v1 endpoint if you need pending visibility. |
| **No state below the seeded chainspec genesis.** Mainnet quick starts at block 464,467,181; testnet quick at 419,000,000. Queries below those heights return `block not found`. Use a full archival node for deeper history. |
| **No subscription to new blocks at chain head.** WebSocket subscriptions (`eth_subscribe newHeads`) emit blocks as they reach LIB on this node, not as they're produced on the chain. ~165s latency on each subscription event. |
| **No fork-history visibility.** The node never sees orphaned (losing-fork) blocks. If your application logic depends on observing reorgs, this node is unsuitable. |

---

## 6. Roadmap interaction with Savanna

Telos mainnet is in the process of activating Savanna instant finality, which will reduce LIB lag from ~165 s (current DPoS) to ~0.5–1 s (BFT finality). When that activation is complete:

- The 165 s offset shrinks to ~1 block. The contract above is structurally unchanged but the latency cost becomes negligible.
- This node becomes a reasonable default for almost all client use cases (no longer just "use it for finality, use v1 prod for tip").
- The architectural choices that made head-tracking unsafe (see §8) remain unchanged — they're handled by Savanna at the chain layer rather than the node layer.

Until Savanna activation is complete, the contract in §3–§5 holds.

---

## 7. When to use this node vs `rpc.telos.net`

| Use case | Recommended endpoint |
| --- | --- |
| Tx confirmation polling, fast-feedback UIs, MEV-style applications | **rpc.telos.net** (head-tracking, ~6 s latency) |
| Historical state lookups, indexer backfills | reth-v2 quick (fast, finalized, no reorg risk) |
| `eth_call` simulations against a known-stable head | **rpc.telos.net** (or revm-based simulation tools) |
| Receipt/log queries past LIB | reth-v2 quick (faster than v1 due to reth's static-files + indexed log layout) |
| Subgraph / The Graph indexers | reth-v2 quick (deterministic finalized blocks; no reorg cleanup needed) |
| `eth_sendRawTransaction` for production tx submission | Either endpoint works; the v2 forwarder is faster on the submission side but confirmation visibility is slower |

Mixed-mode applications can use both: send transactions and read pending state via v1 prod, read finalized state and historical queries via v2 quick.

---

## 8. Why the design is this way (briefly)

The `--telos.trust_consensus + --telos.build_state` mode in reth bypasses revm execution and applies state diffs from the consensus layer's extra-fields directly to MDBX. This is a performance optimization (10x+ catch-up speed during sync) but has a side effect: **there is no inverse operation**. Once a state diff is applied, it cannot be cleanly reverted.

If the node ever ingested a block that later got orphaned by an Antelope reorg, the orphaned state would be stuck in MDBX permanently. Savanna BFT finalization makes this impossible at the protocol level for finalized blocks — hence the choice to feed the node only finalized blocks.

This is documented in detail in `POSTMORTEM_2026-04-30_fork_mvp.md` (the 2026-04-30 incident proved the failure mode is real and the LIB-only contract is the correct mitigation pre-Savanna).

---

## 9. Endpoints

| Network | RPC URL | Chain ID |
| --- | --- | ---: |
| Mainnet | `https://<v2-quick-mainnet-rpc>` (TBD) | 40 |
| Testnet | `https://<v2-quick-testnet-rpc>` (TBD) | 41 |

For now, both nodes are operating on Hetzner `135.181.1.160` ports 8477 (mainnet) and 8677 (testnet) and should not be considered public production endpoints. Public DNS is gated on completion of the multi-node deployment (see `MULTI_NODE_DEPLOYMENT_PLAN.md`).

---

## 10. Versioning

This contract is versioned alongside the running binary. Material changes (e.g. semantic changes to block tags, tip-lag characteristics, supported methods) require a contract version bump and a notice to integrators.

| Version | Date | Summary |
| --- | --- | --- |
| 1.0 | 2026-05-01 | Initial contract; LIB-tracking, ~165s lag, finalized-only semantics. |
