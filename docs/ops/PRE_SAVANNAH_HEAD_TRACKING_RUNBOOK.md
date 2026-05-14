# Pre-Savannah Head-Tracking Runbook

This branch is for operating before Savannah instant finality while still
tracking reversible SHIP head blocks. The safety contract is:

- SHIP runs with `irreversible_only = false`.
- Every block is checked against canonical EVM RPC before reth receives it.
- A block is forwarded only when local SHIP data matches RPC quorum.
- A fork block is skipped when RPC quorum proves it is not canonical.
- If RPC quorum is unavailable, the pipeline stalls and retries the same block.

## Required Consensus Config

Use at least three independently operated EVM RPC endpoints in production and
require a 2-of-3 quorum:

```toml
rpc_fallback_endpoints = [
  "https://rpc-a.example",
  "https://rpc-b.example",
  "https://rpc-c.example",
]
rpc_fallback_quorum = 2
rpc_fallback_retry_interval_secs = 5
rpc_fallback_sample_every_n = 1
```

The legacy `rpc_fallback_endpoint` key is still accepted as a one-endpoint
compatibility mode, but that is not recommended for production.

## Runtime Behavior

- `canonical hash quorum unavailable`: no block is forwarded; the current block
  is retried after `rpc_fallback_retry_interval_secs`.
- `Skipping noncanonical SHIP block`: a reversible fork block was observed and
  skipped. Occasional occurrences are expected before Savannah.
- `receipt unavailable ... canonical RPC quorum includes it`: the tx is in the
  canonical block but receipts are not yet consistently available; the block is
  retried.
- `Canonical RPC quorum omits tx ...`: the raw action is omitted from the
  canonical EVM block. Consensus skips that action and still validates the final
  block hash before forwarding anything to reth.

## Restart/Resume Behavior

- A SHIP websocket close/reset is treated as a run failure, not a clean stop.
- If `max_retry` is unset, the client retries indefinitely after
  `retry_interval` milliseconds. Set `max_retry` only when an external
  supervisor is responsible for long-lived restarts.
- On restart, the client resumes from the latest stored block that is at or
  below reth's current head and verifies that stored hash against reth before
  requesting SHIP data. A mismatch is fatal and should be investigated rather
  than replayed through.

## Reth Settings

Use the matching `telos-reth-v2` branch. The Telos binary defaults are set before
CLI parsing:

```text
--engine.persistence-threshold 20
--engine.persistence-backpressure-threshold 30
--engine.memory-block-buffer-target 30
```

Keep the explicit flags in systemd/launcher scripts anyway. They make production
state auditable and protect operators who accidentally run a different binary.

## Promotion Checklist

1. Build both branches from a clean checkout.
2. Start reth with a fresh datadir or a verified pre-branch snapshot.
3. Start consensus with 2-of-3 RPC quorum and `rpc_fallback_sample_every_n = 1`.
4. Sync to tip and compare `eth_getBlockByNumber("latest", false)` against at
   least two independent public RPCs for the last 1,000 blocks.
5. Restart nodeos once and confirm consensus retries, reconnects, and resumes
   near reth head rather than replaying from LIB.
6. Soak at head for 24 hours before widening operator rollout.
7. Alert on repeated quorum stalls, repeated receipt-included stalls, or any
   consensus process exit that does not recover automatically.
