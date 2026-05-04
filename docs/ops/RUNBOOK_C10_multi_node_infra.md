# Runbook — Multi-node infrastructure deployment

**Item:** C10 from the 2026-05-01 readiness backlog.
**Goal:** concrete, step-by-step procedure for going from today's single-node deployment (Hetzner `135.181.1.160`) to the topology described in `MULTI_NODE_DEPLOYMENT_PLAN_REFRESH_2026-05-01.md` (interim 2-node phase, then 4–5 node target).
**Status:** runbook only. Provisioning, DNS, LB choice, and bucket procurement need operator decisions before any of this can run. Each gate is called out.
**Audience:** ops engineer with ssh access, sudo on Hetzner, and the on-chain account-creation key.

This is a checklist runbook, not a design doc. For the *why*, see the refresh plan. This doc says *what* to do, *in what order*, with concrete commands and validation gates.

---

## Phase 0: Decisions required (gate before starting)

| # | Decision | Default if undecided |
| --- | --- | --- |
| 1 | LB choice (nginx / Cloudflare / HAProxy / Hetzner Cloud LB) | Hetzner Cloud LB (low latency, same DC, cheap). |
| 2 | Bucket provider for snapshot bundles | Backblaze B2 (cheap, no egress fees within EU) |
| 3 | DC for node-2 | Hetzner Falkenstein (same DC as node-1) for first node; second DC introduced at node-3 phase |
| 4 | Public DNS strategy (`v2.rpc.telos.net` etc.) | TBD — not blocking phase 1; can be done before LB goes public |
| 5 | Per-node forwarder accounts: how many to provision now (5 = full topology, 2 = interim) | 2 (`forward.1`, `forward.2`) for interim phase |

If decisions 1, 2, 3 aren't made: the runbook stops at the LB-provision step. The other phases can proceed without them.

---

## Phase 1: Provision 2 forwarder accounts (1 day, gates phase 4+)

Owner-key access required. Run from a machine with `cleos` configured for mainnet.

```bash
# Create forward.1
cleos system newaccount \
  <CREATOR_ACCOUNT> forward.1 \
  PUBKEY_OWNER_FORWARD_1 \
  --stake-net "1.0000 TLOS" --stake-cpu "10.0000 TLOS" --buy-ram "10.0000 TLOS"

# Add fwd permission (scoped to eosio.evm::raw)
cleos set account permission forward.1 fwd \
  '{"threshold":1,"keys":[{"key":"PUBKEY_FWD_1","weight":1}],"accounts":[]}' \
  active

cleos set action permission forward.1 eosio.evm raw fwd

# Repeat for forward.2 with separate keys
```

**Validation:**
```bash
cleos get account forward.1 | grep -A3 "fwd:"
# should show the fwd permission with the scoped action
cleos get action_permissions eosio.evm | grep "raw" | grep "forward.1"
# should show the scoped permission entry
```

**Gate:** keys for forward.1 (fwd) and forward.2 (fwd) checked into ops vault. Owner keys never leave secure storage.

---

## Phase 2: Provision node-2 hardware (1 day, can parallel with Phase 1)

### 2.1 Order

Hetzner dedicated server, model AX52 or equivalent:
- 64 GB DDR5 ECC
- 2× 2 TB NVMe SSD (RAID 1)
- 1 Gbps uplink, unlimited traffic
- Same DC as node-1 (Falkenstein) for low p2p latency
- Ubuntu 22.04 LTS

Cost: ~$130/mo at current Hetzner pricing.

### 2.2 Prepare host

```bash
# As root, fresh install:
apt-get update && apt-get -y upgrade
apt-get -y install build-essential pkg-config libssl-dev libclang-dev curl python3-pip rsync

# Rust (for compile-from-source recovery scenarios)
curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

# Disk layout — same as node-1 (LVM with vg0-data on the second NVMe)
# (operator-specific — see Hetzner LVM setup docs; use vg0-data for /data, vg0-root for /)
```

### 2.3 SSH config

On ops workstation, add to `~/.ssh/config`:

```
Host telos-reth-node-2
    HostName <ip-from-hetzner>
    User root
    Port 22
    IdentityFile ~/.ssh/telos-ops
```

Test: `ssh telos-reth-node-2 'uptime'` — should return uptime line.

---

## Phase 3: Bootstrap snapshot pipeline (2 days)

### 3.1 Bucket setup

- Create a Backblaze B2 bucket `telos-reth-v2-snapshots-mainnet` (or chosen provider).
- Generate an application key with read-write to that bucket.
- Store credentials in `/etc/telos/snapshot-uploader.env` (mode 0600, owner root).

### 3.2 Extend the existing snapshot script

`/usr/local/bin/telos-snapshot.sh` (already deployed for local snapshots) gets a new `--upload` flag. Edit to add an upload step after the local cold-copy:

```bash
upload_snapshot() {
    local snap_path=$1
    local node=$2
    local tag=$3

    # tar + zstd the snapshot directory
    local tarball="/tmp/telos-snap-${tag}-${node}.tar.zst"
    tar -C "$(dirname "$snap_path")" -cf - "$(basename "$snap_path")" \
        | zstd -3 -T0 -o "$tarball"

    # Compute manifest with sha256
    local sha=$(sha256sum "$tarball" | cut -d' ' -f1)
    local manifest_path="/tmp/telos-snap-${tag}-${node}.manifest"
    cat > "$manifest_path" <<EOF
tag: $tag
node: $node
sha256: $sha
size_bytes: $(stat -c %s "$tarball")
created: $(date -u +%Y-%m-%dT%H:%M:%SZ)
schema: v1
EOF

    # Sign manifest (offline ops key)
    openssl dgst -sha256 -sign /etc/telos/snapshot-signing.pem -out "${manifest_path}.sig" "$manifest_path"

    # Upload tarball + manifest + signature
    source /etc/telos/snapshot-uploader.env
    b2 upload-file "$BUCKET_NAME" "$tarball" "snapshots/${tag}/${node}.tar.zst"
    b2 upload-file "$BUCKET_NAME" "$manifest_path" "snapshots/${tag}/${node}.manifest"
    b2 upload-file "$BUCKET_NAME" "${manifest_path}.sig" "snapshots/${tag}/${node}.manifest.sig"

    rm -f "$tarball" "$manifest_path" "${manifest_path}.sig"
}
```

### 3.3 Bootstrap script

`/usr/local/bin/telos-bootstrap-node.sh` — invoked on a fresh node to pull the latest snapshot from the bucket and start services.

```bash
#!/usr/bin/env bash
set -euo pipefail
NODE=$1                # e.g. mainnet-quick (matches snapshot tag)
TARGET_DATA_DIR=/data/reth-${NODE//-/-v2-}

# 1. Pull latest manifest
source /etc/telos/snapshot-uploader.env
TAG=$(b2 ls "$BUCKET_NAME" "snapshots/" | sort -r | head -1 | awk '{print $1}' | sed 's|snapshots/||;s|/.*||')
[ -z "$TAG" ] && { echo "no snapshots in bucket"; exit 1; }
echo "Bootstrapping from snapshot: $TAG"

# 2. Download tarball + manifest + sig
mkdir -p /tmp/bootstrap
b2 download-file-by-name "$BUCKET_NAME" "snapshots/${TAG}/${NODE}.tar.zst"   "/tmp/bootstrap/${NODE}.tar.zst"
b2 download-file-by-name "$BUCKET_NAME" "snapshots/${TAG}/${NODE}.manifest"  "/tmp/bootstrap/${NODE}.manifest"
b2 download-file-by-name "$BUCKET_NAME" "snapshots/${TAG}/${NODE}.manifest.sig" "/tmp/bootstrap/${NODE}.manifest.sig"

# 3. Verify signature
openssl dgst -sha256 -verify /etc/telos/snapshot-signing.pub -signature "/tmp/bootstrap/${NODE}.manifest.sig" "/tmp/bootstrap/${NODE}.manifest" || \
    { echo "manifest signature verification failed"; exit 1; }

# 4. Verify sha256 of tarball matches manifest
EXPECTED_SHA=$(grep "^sha256:" "/tmp/bootstrap/${NODE}.manifest" | awk '{print $2}')
ACTUAL_SHA=$(sha256sum "/tmp/bootstrap/${NODE}.tar.zst" | cut -d' ' -f1)
[ "$EXPECTED_SHA" != "$ACTUAL_SHA" ] && { echo "tarball sha256 mismatch"; exit 1; }

# 5. Extract
mkdir -p "$(dirname "$TARGET_DATA_DIR")"
tar -C /tmp/bootstrap -xf "/tmp/bootstrap/${NODE}.tar.zst" --use-compress-program=unzstd

# 6. Move into place (atomic rename)
mv "/tmp/bootstrap/${NODE}" "$TARGET_DATA_DIR"

# 7. Configure services (copy unit files from a known location, adapt config for this host)
# (operator runs this part — paths and configs are node-specific)

echo "bootstrap complete; review service configs and start manually"
```

**Test on a throwaway VM first.** Don't run this on real node-2 until the bucket pipeline + signing key flow is verified.

---

## Phase 4: Stand up node-2 (1 day + 7 day soak)

### 4.1 Bootstrap

```bash
# On node-2
ssh telos-reth-node-2

# Run bootstrap script (pulls latest snapshot, verifies, extracts)
/usr/local/bin/telos-bootstrap-node.sh mainnet-quick

# Configure services with forward.2 key
mkdir -p /etc/telos
cat > /etc/telos/forwarder-key <<EOF
PRIVKEY_FWD_2_HERE
EOF
chmod 0600 /etc/telos/forwarder-key

# Edit /usr/local/bin/telos-reth-v2-mainnet-quick to use forward.2 account/key
# (replace --telos.signer_account rpc.evm with forward.2; key from /etc/telos/forwarder-key)

# Install systemd units (copy from snapshot or from a known git repo)
cp /tmp/bootstrap/telos-consensus-mainnet-quick.service /etc/systemd/system/
cp /tmp/bootstrap/telos-reth-v2-mainnet-quick           /usr/local/bin/
chmod +x /usr/local/bin/telos-reth-v2-mainnet-quick
systemctl daemon-reload

# Start in order
systemctl start telos-reth-mainnet-quick
sleep 8
systemctl start telos-consensus-mainnet-quick
sleep 30

# Validate against canonical
M=http://127.0.0.1:8477
P=https://rpc.telos.net
q=$(curl -s -X POST -H Content-Type:application/json -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' $M | jq -r .result)
p=$(curl -s -X POST -H Content-Type:application/json -A "Mozilla/5.0" -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' $P | jq -r .result)
echo "node-2 tip: $q  canonical: $p"
# lag should be ~330 (LIB-tracking baseline)
```

### 4.2 Add canonical-comparison monitor and snapshot timer to node-2

Same procedure as on node-1. From ops workstation:

```bash
scp /Volumes/CodexWorkspaces/Claude/reth-v2/canonical-monitor.py             telos-reth-node-2:/usr/local/bin/
scp /Volumes/CodexWorkspaces/Claude/reth-v2/telos-canonical-monitor.service  telos-reth-node-2:/etc/systemd/system/
scp /Volumes/CodexWorkspaces/Claude/reth-v2/telos-snapshot.sh                telos-reth-node-2:/usr/local/bin/
scp /Volumes/CodexWorkspaces/Claude/reth-v2/telos-snapshot.{service,timer}   telos-reth-node-2:/etc/systemd/system/

ssh telos-reth-node-2 'systemctl daemon-reload; systemctl enable --now telos-canonical-monitor telos-snapshot.timer'
```

### 4.3 7-day soak — DO NOT add to LB write-pool yet

Watch:
- `/var/log/telos-canonical-monitor.log` for any MISMATCH events.
- `journalctl -u telos-consensus-mainnet-quick` for crashes / unusual errors.
- `/var/log/telos-snapshot.log` for the daily 04:00 UTC snapshot timer.

If 7 days pass with zero MISMATCH and no service restarts beyond the daily snapshot window, proceed to phase 5.

---

## Phase 5: LB tier (2 days, gated on Phase 0 decision 1)

### 5.1 Provision LB

If Hetzner Cloud LB chosen:
- Create LB in Falkenstein DC.
- Backend pool: node-1 + node-2 on port 8477 (mainnet quick).
- Health check: `POST /` with `{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}`, expecting JSON response with non-null result, every 5 s, 2 s timeout, 3 consecutive failures = unhealthy.
- TLS terminator: cert from chosen CA, valid for the public DNS name.

If nginx self-hosted:
- Provision a small VM (~$15/mo).
- Install nginx + Let's Encrypt for TLS.
- Config:

```nginx
upstream reth_v2_quick {
    server 135.181.1.160:8477 max_fails=3 fail_timeout=30s;
    server <node-2-ip>:8477   max_fails=3 fail_timeout=30s;
    keepalive 32;
}

limit_req_zone $binary_remote_addr zone=public_read:10m rate=50r/s;
limit_req_zone $binary_remote_addr zone=public_write:10m rate=10r/s;

server {
    listen 443 ssl http2;
    server_name v2.rpc.telos.net;
    ssl_certificate     /etc/letsencrypt/live/v2.rpc.telos.net/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/v2.rpc.telos.net/privkey.pem;

    location / {
        # Method-based routing for write endpoints
        if ($request_body ~* "eth_sendRawTransaction") {
            limit_req zone=public_write burst=30 nodelay;
            set $is_write 1;
        }
        if ($is_write != 1) {
            limit_req zone=public_read burst=200 nodelay;
        }

        proxy_pass http://reth_v2_quick;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
        proxy_read_timeout 30s;
    }

    location /healthz {
        return 200 "OK";
        add_header Content-Type text/plain;
    }
}
```

### 5.2 Initial: read-only behind LB

DNS: point `v2.rpc.telos.net` → LB IP.

Don't enable write traffic yet. Read-only soak for 24 h, watching:
- 5xx error rate on the LB. Should be ~0.
- Latency p50/p95 (target: p50 < 50 ms, p95 < 200 ms for typical reads).
- Per-backend traffic distribution (should be roughly 50/50 between node-1 and node-2).

### 5.3 Enable write traffic

After 24 h clean read-only:

- Update LB config to also route `eth_sendRawTransaction` to backend pool (already in the nginx example above).
- 24 h additional soak watching forwarder probe success rate (synthetic burn-tx test from `forwarder_observability.py`).

If clean, the 2-node phase is done. Proceed to multi-node scale-out per the original plan §Sequencing.

---

## Phase 6: Decommissioning the snapshot retention task

The single-node `telos-snapshot.timer` keeps 7 daily snapshots locally. With 2 nodes + bucket-uploaded bundles, the local snapshot retention is less critical (we have offsite backups in the bucket). Reduce local retention to 2 days to save disk:

```bash
sed -i 's/^RETENTION_DAYS=7/RETENTION_DAYS=2/' /usr/local/bin/telos-snapshot.sh
```

Run on both nodes.

---

## Phase 7: Operational handoff

After all of the above, the new normal is:

| Task | Who | Cadence |
| --- | --- | --- |
| Daily snapshot upload to bucket | Automated (telos-snapshot.timer) | 04:00 UTC daily |
| LB health check + alert on backend down | LB / Cloudflare / monitoring | continuous |
| Canonical-comparison monitor per-node | telos-canonical-monitor.service | every 60 s |
| Forwarder probe (synthetic burn-tx) | telos-forwarder-obs.timer (existing, needs probe key) | every 5 min |
| Reth + CL upgrade | Manual, rolling, per the original plan §Rolling upgrades | as needed |
| Snapshot validation (random restore drill) | Manual | quarterly |

The last item — quarterly snapshot restore drill — is critical and easy to forget. Pick a snapshot from the bucket, restore on a throwaway VM, confirm services come up clean, validate against canonical at the snapshot timestamp. Catches silent corruption in the snapshot pipeline that would otherwise only be discovered during an actual incident.

---

## Phase 8: When this runbook is no longer enough

This runbook covers the 2-node interim. To go from 2 nodes to the full 5+1 topology, repeat Phases 4–5 for each new node. Variables that may need different defaults:

- Node-3 in a different DC (cross-region tolerance).
- Archive node (different specs: 128 GB RAM, 8 TB NVMe, no quick sync, full archival).
- Per-node forwarder accounts beyond `forward.2` (just repeat Phase 1).

For full 5+1 the runbook stays the same shape; only the per-node details change.

---

## Out of scope (deliberately)

- HSM-backed forwarder key handling. Software keys with `chmod 0600` is the current acceptable bar.
- Multi-region failover. Single-DC tolerance is the goal for now per the original plan.
- Automatic CPU-stake top-ups on forwarder accounts. Manual ops procedure for now.
- Reth-full archival deployment. Gated on the ongoing reth-full archival recovery work — separate runbook when that completes.
