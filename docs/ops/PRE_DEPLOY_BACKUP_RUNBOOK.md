# Pre-deploy backup runbook — reth + consensus client

**When to use:** before *any* change that touches reth's `--datadir`, the consensus client's binary, the consensus client's `data_path`, the reth launcher's flags affecting persistence (`--engine.persistence-threshold`, `--engine.persistence-backpressure-threshold`, `--telos.build_state`, `--telos.trust_consensus`), or the SHIP request flags in `raw_deserializer.rs`.

**Why:** the 2026-04-30 fork-handling MVP deployment cost a 15-hour resync because no snapshot existed. A 60-second cold copy would have made rollback a 5-minute operation. This runbook formalises the pattern so we don't pay that cost again.

---

## Pre-deploy: take a snapshot

Replace `mainnet-quick` with the actual node name (e.g. `testnet-quick`, `testnet-full`, `mainnet-quick`).

```bash
NODE=mainnet-quick
DESCRIPTION="fork-handling-mvp"   # short, no spaces
DEPLOY_TAG="$(date +%Y%m%d-%H%M%S)-$DESCRIPTION"
SNAP_DIR=/data/backups/$DEPLOY_TAG-$NODE
mkdir -p "$SNAP_DIR"

# Stop services in order: CL first (so it stops feeding reth), then reth.
systemctl stop telos-consensus-${NODE}
systemctl stop telos-reth-${NODE}

# Cold-copy reth datadir and CL DB. -a preserves perms + symlinks + mtimes.
cp -a /data/reth-${NODE//-/-v2-}                                       "$SNAP_DIR/reth-datadir"
cp -a /data/telos-consensus-client/${NODE//-quick/-v2-quick}/db        "$SNAP_DIR/cl-db"

# Manifest with sizes + the unit/launcher files we may need to restore.
{
  echo "tag: $DEPLOY_TAG"
  echo "node: $NODE"
  echo "description: $DESCRIPTION"
  echo "snapshot taken: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "reth datadir size: $(du -sh "$SNAP_DIR/reth-datadir" | cut -f1)"
  echo "cl db size: $(du -sh "$SNAP_DIR/cl-db" | cut -f1)"
} > "$SNAP_DIR/manifest.txt"
cp /etc/systemd/system/telos-consensus-${NODE}.service "$SNAP_DIR/"
cp /usr/local/bin/telos-reth-v2-${NODE}                "$SNAP_DIR/"

# Restart services to resume normal operation while you prepare the deploy.
systemctl start telos-reth-${NODE}
sleep 8
systemctl start telos-consensus-${NODE}
echo "snapshot ready: $SNAP_DIR"
```

**Expected sizes** (as of 2026-04-30):
- mainnet-quick reth datadir: ~917 MB
- mainnet-quick CL DB: ~10 MB
- testnet-quick reth datadir: ~984 MB
- Full snapshot copy time on local SSD: ~30–60 s

**Total downtime for the snapshot itself:** ~30–60 seconds. Cheap insurance.

---

## Deploy

Proceed with the actual change. Document the exact files modified in `$SNAP_DIR/manifest.txt` so rollback is unambiguous.

---

## Rollback (if the deploy goes bad)

```bash
SNAP_DIR=/data/backups/<the-tag-from-above>-<node>
NODE=<node>

systemctl stop telos-consensus-${NODE}
systemctl stop telos-reth-${NODE}

# Quarantine the broken state (so we can forensically compare later).
mv /data/reth-${NODE//-/-v2-} /data/reth-${NODE//-/-v2-}.broken
mv /data/telos-consensus-client/${NODE//-quick/-v2-quick}/db \
   /data/telos-consensus-client/${NODE//-quick/-v2-quick}/db.broken

# Restore datadirs.
cp -a "$SNAP_DIR/reth-datadir" /data/reth-${NODE//-/-v2-}
cp -a "$SNAP_DIR/cl-db"        /data/telos-consensus-client/${NODE//-quick/-v2-quick}/db

# Restore service units / launchers if those changed.
cp "$SNAP_DIR/telos-consensus-${NODE}.service" /etc/systemd/system/
cp "$SNAP_DIR/telos-reth-v2-${NODE}"           /usr/local/bin/
chmod +x /usr/local/bin/telos-reth-v2-${NODE}
systemctl daemon-reload

# Restart in the right order.
systemctl start telos-reth-${NODE}
sleep 8
systemctl start telos-consensus-${NODE}
```

Total rollback time: ~2 minutes. Compare to a 15-hour resync from chainspec.

---

## When NOT to use this runbook

- Pure config changes that don't affect persistence semantics (e.g., adjusting log level, RPC port).
- Changes that only touch monitoring scripts, cron jobs, or supporting tools.
- Anything where the failure mode is "service crashes immediately" rather than "service runs but corrupts state silently." Crash-on-start is self-evident; silent corruption is what this snapshot guards against.

---

## Validation gate (before declaring deploy successful)

After deploy + restart, run this hash-comparison sweep:

```bash
M=http://127.0.0.1:8477   # mainnet quick; adjust port for testnet (8677)
P=https://rpc.telos.net   # canonical for the chain (rpc.testnet.telos.net for testnet)
q=$(curl -s -X POST -H Content-Type:application/json \
        -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' $M \
   | python3 -c 'import sys,json;print(int(json.load(sys.stdin)["result"],16))')
fail=0
for delta in 50 200 800 2000; do
  bn=$((q-delta)); bn_hex=$(printf 0x%x $bn)
  q_hash=$(curl -s -X POST -H Content-Type:application/json \
              -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getBlockByNumber\",\"params\":[\"$bn_hex\",false],\"id\":1}" $M \
        | python3 -c 'import sys,json;r=json.load(sys.stdin).get("result");print(r["hash"]) if r else print("none")')
  p_hash=$(curl -s -X POST -H Content-Type:application/json -A "Mozilla/5.0" \
              -d "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getBlockByNumber\",\"params\":[\"$bn_hex\",false],\"id\":1}" $P \
        | python3 -c 'import sys,json;r=json.load(sys.stdin).get("result");print(r["hash"]) if r else print("none")')
  if [ "$q_hash" = "$p_hash" ]; then echo "Block $bn MATCH"; else echo "Block $bn MISMATCH"; fail=1; fi
done
[ $fail -eq 0 ] && echo "DEPLOY VALIDATED" || echo "ROLL BACK NOW"
```

If any block shows MISMATCH at 200+ blocks back from tip, **roll back immediately** — the corruption window is what we're racing.

For changes that affect fork-handling specifically (e.g. `irreversible_only`, fork-emit code), add a continuous validator that runs every 60 s and an Antelope-fork monitor that tails the local nodeos journal for `switching forks`. Do not declare success until the deploy has survived ≥10 real fork events on testnet without divergence.

---

## Backup retention

- Keep the snapshot for at least 7 days after a successful deploy. If the deploy is later found to have introduced a subtler bug (e.g. F2-style log warnings) you may want to roll back even days later.
- After 30 days, delete unless the snapshot is still being used as a forensic reference.
- Snapshots from a *failed* deploy: keep at least 30 days for post-mortem analysis. Tag them with `corrupted-` prefix in the path so they aren't accidentally restored from.
