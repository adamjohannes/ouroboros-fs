# Operations

The operator's guide. Audience: whoever is going to run OuroborosFS
in production. Cross-references:

- `docs/ARCHITECTURE.md` — what the system is and how it works.
- `docs/SECURITY.md` — threat model and trust boundary.
- `samples/systemd/` — example unit files.
- `samples/config/` — example TOML configs.
- `CHANGELOG.md` — what changed in each release.

## Deployment

The supported shape: each ring node is its own systemd unit; the
gateway is a separate unit. `samples/systemd/` ships templated units
that match this shape.

Quick start:

```bash
sudo cp samples/systemd/ouroboros-node@.service \
        samples/systemd/ouroboros-gateway.service \
        /etc/systemd/system/
sudo cp samples/systemd/ouroboros.env /etc/default/ouroboros
sudo chmod 600 /etc/default/ouroboros

# Generate the cluster-wide auth token. Every node + the gateway must
# share this. Edit /etc/default/ouroboros to match.
openssl rand -hex 32

sudo useradd --system --no-create-home --shell /usr/sbin/nologin ouroboros
sudo install -d -o ouroboros -g ouroboros /var/lib/ouroboros

sudo install -o root -g root -m 0755 \
  target/release/ouroboros_fs /usr/local/bin/

sudo systemctl daemon-reload
sudo systemctl enable --now ouroboros-node@7000 \
                            ouroboros-node@7001 \
                            ouroboros-node@7002 \
                            ouroboros-gateway
```

The ring still needs to be wired (`NODE NEXT`) once on first boot.
Two options:

1. Send `NODE NEXT` lines manually with `nc` after authenticating.
2. Bring up `dev-network` once with the same auth token and ports,
   let it wire the ring, then `Ctrl-C` it. The wiring persists in
   each node's in-memory state for the lifetime of the process.

For multi-host deployments, replace `127.0.0.1` in the unit files
with each node's actual address. The ring is direction-sensitive
(node N → node N+1); make sure the `NODE NEXT` topology forms a
proper cycle.

## Capacity planning

| Resource | Sizing |
|---|---|
| **Disk per node** | Each chunk is stored both as content (the owner) and as backup (the predecessor's backup/). Effective on-cluster footprint = 2× user-data, distributed evenly across the N nodes. Plan for `2 × total_data_size / N` per node. |
| **RAM per node** | The `file_tags` map is `name → (start_port, size, parts)`. ~100 bytes per file. A million files ≈ 100 MB. Chunk reads load the *whole* chunk into RAM during integrity verification (see `open_chunk_verified`); plan for at least `max_chunk_size + small_overhead` headroom. The 50 GB hard cap on file size applies. |
| **CPU per node** | Negligible at idle. Per-chunk SHA-256 (write + verify) is the dominant cost; modern x86 hits ~500 MB/s. |
| **Network** | PUSH fans out (start node sends N-1 simultaneous chunks). Plan upstream bandwidth `~= peak push throughput * (N-1)/N`. PULL fetches in parallel; cap is `FETCH_CONCURRENCY = 4`. |
| **Off-cluster backup** | See "Cluster-level backup" below. |

### Topology recommendations

- **Minimum 3 nodes.** With 2, every chunk and its backup live on
  the same node — losing one host loses everything.
- **Odd counts simplify any future quorum work.** If you're
  expecting to grow into NEXT_STEPS.md §7.1 (replication factor
  > 2), prefer 3, 5, 7.
- **Co-locate by failure domain, not by speed.** A 5-node ring
  spread across 5 racks survives a rack-level outage; the same
  ring squeezed into one rack doesn't.

## Operator runbook

### Symptom: a node is down and won't come back

1. Confirm: `curl http://gateway:8000/ready` returns 503 if the
   ring has lost quorum, 200 with one Dead in the netmap if not.
   `cat /etc/systemd/system/ouroboros-node@<port>.service` and
   `journalctl -u ouroboros-node@<port>` for the node's view.
2. Try `systemctl restart ouroboros-node@<port>`. The node's
   `--storage-root` is preserved across restarts; on-disk content
   survives. If gossip is healthy, the rest of the ring marks it
   Alive within ~1 gossip cycle.
3. If the host is gone, see "Disk failure" below.

### Symptom: disk failure on a node

1. Replace the disk; restore `/var/lib/ouroboros/<port>/` from the
   off-host snapshot (see "Cluster-level backup → Restore" below).
2. Start the node. The healer on the predecessor will see it's
   alive again and run anti-entropy refill (§1.5b) — every chunk
   the predecessor's backup/ holds gets pushed back to the
   respawned node's content/.
3. Verify: `curl -H "Authorization: Bearer $TOKEN"
   http://gateway:8000/file/list | jq length` should match the
   pre-failure file count.

### Symptom: simultaneous loss of all N nodes (rare)

Disaster recovery from off-cluster snapshots:

1. Pick the most recent snapshot timestamp that exists for
   **every** node.
2. For each node, restore that snapshot to its `<storage_root>`.
3. Bring nodes up in any order; redeploy the systemd units.
4. Run `NETMAP DISCOVER` and `TOPOLOGY WALK` from any node.
5. `FILE LIST` should now reflect the restored state.

### Symptom: PULL emits a short body + truncation trailer

The body ends with `\nERR truncated expected=<E> got=<G>\n`.

- Cause: a chunk's owner *and* its predecessor (the backup
  holder) both failed mid-pull. The chunk is unrecoverable until
  one of those nodes is restored.
- Mitigation: restore the off-cluster snapshot for whichever
  failed node is easier to recover.
- The gateway logs the truncation at `error` level; alert on this.

### Symptom: `/ready` returns 503

No ring node is responding to `NODE PING` from the gateway.

- `curl http://gateway:8000/health` should still return 200; if
  it doesn't, the gateway itself is down.
- `curl http://gateway:8000/netmap/get` (with bearer) shows the
  gateway's last known view. Nodes appearing as Dead means the
  most recent ping failed.
- `journalctl -u ouroboros-gateway` for the gateway's per-attempt
  errors (connection refused vs auth failure vs read timeout).

### Symptom: `ouroboros_pulls_total` rises but `chunk_bytes_read_total` is flat

Integrity-check failures are triggering size=0 fall-throughs to
backup. The owner's chunks are corrupt on disk. Use the per-node
breakdown in `/metrics` to find which node:

```
ouroboros_pulls_total{node="7001"} 100
ouroboros_chunk_bytes_read_total{node="7001"} 0
```

means node 7001 served 100 corrupt-or-missing PULLs. Restore that
node's storage from snapshot.

### Symptom: disk usage grows steadily

Expected. There is no compaction or garbage collection in v1.0.
Files are immutable once pushed; the only way to free disk is to
not push them in the first place. NEXT_STEPS.md §7.x has the
roadmap for richer lifecycle.

## Version-upgrade procedure

OuroborosFS uses semver from v1.0.0 onward. Upgrade safety
depends on the kind of change:

| Change kind | Procedure |
|---|---|
| **Patch** (e.g., 1.0.1) | Rolling restart. `systemctl restart` one node at a time; the heal flow rebinds it via the predecessor's anti-entropy refill if the storage tree was preserved. |
| **Minor** (e.g., 1.1.0) | Same as patch, but check `CHANGELOG.md` for any new flags or env vars to set. |
| **Major** (e.g., 2.0.0) | Read the changelog. May require: schema migrations (`STORAGE_VERSION` bump), wire-protocol break, full re-push of data. |

The `STORAGE_VERSION` marker (currently `1`) is the canary. If a
new release reads a tree it can't handle, it refuses to start with
a clear error message — it will not silently corrupt your data.

For the v1.0-rc → v1.0 final transition, no migration is required.

### Upgrading from a pre-marker tree

The `STORAGE_VERSION` marker landed in v1.0-rc.1. If you ran an
earlier rc against a tree that was already v1-format (i.e., chunks
already carry the SHA-256 trailer from Series B) but doesn't yet have
a `VERSION` file, `bind` refuses to start with `unversioned storage
tree at <path> contains chunks; …`.

To assert that the tree is genuinely v1 and let `bind` write the
marker over it, set `OUROBOROS_FORCE_V1=1` in the environment for one
boot:

```bash
OUROBOROS_FORCE_V1=1 systemctl restart ouroboros-node@7000
# Verify the marker is now in place:
cat /var/lib/ouroboros/7000/VERSION   # → "1"
# Unset OUROBOROS_FORCE_V1 from the environment going forward.
```

If you're not sure whether the tree is v1, the safer path is to wipe
`<storage_root>` and let the heal flow refill from the cluster's other
copies. Setting `OUROBOROS_FORCE_V1` against a pre-Series-B tree
(chunks without the trailer) will cause every PULL to fall through
to the predecessor's backup forever.

## Log analysis

Production deployments should set `--log-format json` so logs go
straight into Splunk/ELK/Datadog without parsing. Useful queries:

| Field | Useful for |
|---|---|
| `target=ouroboros_fs::server` | All ring-side events. |
| `target=ouroboros_fs::gateway` | Gateway events. |
| `level=ERROR` | Anything we want to alert on. |
| `node=<port>` | Per-node breakdown. |
| `chunk=<name>` | Per-chunk failure tracking. |

Specific events to alert on:

- `Chunk failed integrity check; dropping connection so the puller
  falls through to backup.` — a chunk on disk is corrupt. Repeat
  occurrences from the same node mean the disk is failing.
- `Backup chunk failed integrity check; no further fall-through
  available.` — both copies of a chunk are bad. Data loss.
- `PULL produced short output; emitted truncation trailer` —
  somebody pulled a file that's now only partially recoverable.
- `Refusing connection: max_conns saturated` — load spike or
  someone hammering the gateway.
- `Rejected unauthenticated connection` — possibly a probe;
  possibly a misconfigured client.

## Cluster-level backup

OuroborosFS replicates each chunk **once** within the ring (owner +
predecessor). That protects against the loss of any single node. It does
**not** protect against:

- Simultaneous loss of all N nodes (datacenter fire, ransomware, mass human
  error).
- Logical corruption that's pushed through the ring (a misconfigured
  client deletes everything; the ring faithfully replicates the deletes).

Operators are responsible for backing up the storage tree to an off-cluster
destination on a schedule that matches their RPO.

### What to back up

For a ring of N nodes, every file's bytes live in two places:

- **Content** of chunk *i* lives at
  `<storage_root>/<port_i>/content/<file>.part-<NNN>-of-<MMM>` on the
  chunk owner.
- **Backup** of chunk *i* lives at
  `<storage_root>/<port_(i-1)>/backup/<file>.part-<NNN>-of-<MMM>` on the
  predecessor.

A cluster-wide snapshot must capture **every node's** `<storage_root>` —
copying just one node loses N-1 chunks of every file.

The on-disk layout is `<body bytes> || sha256(body)` per chunk (Series B
trailer). Backup destinations should treat chunks as opaque; the ring
verifies the trailer on every read.

### Suggested approach: per-node rsync

Run, on every host that owns a node, a periodic snapshot of its
`<storage_root>` to off-host storage. Example for an hourly backup to S3:

```bash
# /etc/cron.hourly/ouroboros-backup
HOST="$(hostname)"
SRC="/var/lib/ouroboros/nodes"     # whatever you passed to --storage-root
DEST="s3://my-backups/ouroboros/${HOST}/"
TS="$(date +%Y-%m-%dT%H)"

aws s3 sync "${SRC}/" "${DEST}${TS}/" \
  --exclude '*.partial' \
  --storage-class STANDARD_IA
```

Two important details:

1. **Exclude `*.partial`** — those are mid-write tempfiles; the startup
   janitor (Series B) sweeps them, and they'll fail the SHA-256 check
   anyway.
2. **Snapshot consistency is per-chunk, not per-file**. Since chunk
   writes are atomic (`durably_write_chunk` writes to `.partial` then
   renames), rsync will either see the old `<chunk>` or the new one —
   never a torn write. But two chunks of the same file may snapshot at
   different times, so the restored file may be older-than-RPO if the
   PUSH straddled the snapshot window. This is acceptable for most
   datasets; if it isn't, add a quiesce step (e.g., reject pushes during
   the rsync window).

### Restore

To restore a single node's losses (disk failure, accidental `rm -rf`):

1. Stop the affected node (`pkill -f 'ouroboros_fs run --addr <host>:<port>'`).
2. `rsync` the most recent off-host snapshot back to `<storage_root>`.
3. Restart the node. The ring's heal flow will refresh in-memory NETMAP /
   TOPOLOGY / file_tags; the on-disk content + backup directories are
   already correct.

To restore the **whole cluster** after total loss:

1. Pick the most recent snapshot timestamp that exists for **every** node.
2. For each node, restore that snapshot to its `<storage_root>`.
3. Bring nodes up in any order, then wire the ring with `NODE NEXT` (or
   redeploy the systemd units).
4. Run `NETMAP DISCOVER` and `TOPOLOGY WALK` from any node. `FILE LIST`
   should now reflect the restored state.

### Capacity planning for backups

- On-cluster footprint: every byte of user data is stored 2× (content +
  one predecessor backup).
- Off-cluster footprint: snapshotting all N nodes means another 2× the
  user data, per snapshot generation. A 30-day rolling backup of a 1 TB
  cluster is ~60 TB of off-cluster storage; consider lifecycle rules
  (e.g., S3 Glacier after 7 days).

A more space-efficient strategy is to snapshot **N/2 + 1** nodes that
together hold every chunk + every backup. Concretely, in a 3-node ring
the content of every file lives on every node (since `parts == N`), so
snapshotting any 2 nodes recovers the full file set. This optimization
is fragile against future placement-strategy changes (NEXT_STEPS.md
§7.2); when in doubt, snapshot every node.

## Incident-response template

Paste this into your incident channel and fill in:

```
## Incident: <short description>

- **Detected:** <when, by what — paged on metric? user report?>
- **Symptoms:**
  - <what's broken from the user's POV>
  - <what's broken from the operator's POV>
- **Probable cause:** <best current guess>
- **Affected scope:** <which nodes, which files, what % of traffic>

### Timeline (UTC)

- HH:MM — <event>
- HH:MM — <event>

### Diagnosis

- <log queries we ran>
- <metrics we looked at>
- <hypothesis we ruled out and why>

### Mitigation

- <what we did to stop the bleeding>
- <whether it worked>

### Permanent fix

- <code change / runbook update / monitoring add>
- <PR / issue link>

### Lessons / TODO

- <what should change so this doesn't happen again>
```

Customize to fit your team's conventions (paging policy, comms
channel, incident-tracker). The skeleton above is a starting
point, not a mandate.
