# Operations

Operator-facing notes for running OuroborosFS in production. This file covers
**cluster-level backup** (NEXT_STEPS.md §4.8); other operational topics
(deploy, capacity, runbooks) are tracked in §6.1 and not yet written.

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
