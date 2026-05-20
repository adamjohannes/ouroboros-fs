# OuroborosFS architecture

A single-page contributor's overview. For the narrative tutorial, see
`docs/01-06`. For deployment, see `docs/operations.md`.

## Topology

OuroborosFS is a logical ring of N nodes. Each node knows only its
**successor** (`next_port`); the successor's successor, transitively,
is the rest of the ring. A separate **gateway** can run in front,
exposing HTTP and a TCP-proxy for clients that don't speak the wire
protocol natively.

```
                    ┌────────────┐
                    │  gateway   │  HTTP/TCP entry point
                    │ :8000      │  Bearer auth
                    └──────┬─────┘
                           │  AUTH + ring protocol
              ┌────────────┼────────────┐
              ▼            ▼            ▼
        ┌─────────┐  ┌─────────┐  ┌─────────┐
        │ node 0  │─▶│ node 1  │─▶│ node 2  │─┐
        │ :7000   │  │ :7001   │  │ :7002   │ │
        └─────────┘  └─────────┘  └─────────┘ │
              ▲                               │
              └───────────────────────────────┘
```

Each node's storage tree:

```
<storage_root>/
└── <port>/
    ├── VERSION              storage-format marker (currently "1")
    ├── content/             chunks this node owns
    │   ├── foo.bin.part-001-of-003   body || sha256(body)
    │   ├── foo.bin.part-001-of-003.partial   (transient mid-write)
    │   └── …
    └── backup/              chunks the *successor* owns
        ├── bar.bin.part-002-of-003
        └── …
```

Two invariants:

1. Every chunk on disk has a 32-byte SHA-256 trailer; reads verify
   before serving.
2. Every chunk in `content/` of node N has a copy in `backup/` of
   node N's predecessor. (Predecessor-of in the ring sense — for a
   3-node ring, predecessor of 0 is 2.)

## Wire protocol

Line-delimited ASCII, single-space separator, optional binary payload
after the header line. The full grammar is in `src/protocol.rs`.

Highlights:

| Verb | Direction | Purpose |
|---|---|---|
| `AUTH <hmac> <nonce>` | client → node, gateway → node | First line on every connection. Skipped when auth is disabled. |
| `FILE PUSH <size> <name>` | client → start node | Top-level upload. Start node fans the body out across the ring. |
| `FILE PULL <name>` | client → any node | Top-level download. Pulling node fetches each chunk in parallel. |
| `FILE PUSH-CHUNK …` | start → owner | Internal fan-out leg of PUSH. |
| `FILE BACKUP-PUSH …` | owner → predecessor | Push-based replication after a successful save. |
| `FILE CONTENT-PUSH …` | predecessor → respawned successor | Anti-entropy refill (§1.5b). |
| `NODE PING` | gossip & gateway probe | Liveness check; expects `PONG`. |
| `NODE METRICS` | gateway → node | `<key>=<value>` lines + `OK`. |

## Sequence: FILE PUSH

```
client       node 0 (start)    node 1            node 2
  │              │                │                │
  │── PUSH ─────▶│                │                │
  │  size N      │                │                │
  │              │── connect ────▶│                │
  │              │── connect ───────────────────  ▶│
  │              │── PUSH-CHUNK ▶│                │
  │              │── PUSH-CHUNK ────────────────  ▶│
  │              │                │                │
  │── body bytes ▶ stream chunk 0 to its own disk  │
  │              │── stream chunk 1 ▶│             │
  │              │── stream chunk 2 ────────────  ▶│
  │              │                │                │
  │              │            BACKUP-PUSH chunk 1 to node 0
  │              │            BACKUP-PUSH chunk 2 to node 1
  │              │      (chunk 0 backup-pushed to node 2)
  │              │                │                │
  │              │◀── OK ─────────│                │
  │              │◀── OK ───────────────────────  ─│
  │◀── OK ───────│                │                │
```

Key points:

- The start node opens N-1 outbound TCPs **in parallel** (one per
  non-start chunk). Each receiver writes its chunk durably (fsync
  + atomic rename) and ACKs.
- Each receiver fires a `BACKUP-PUSH` to its own predecessor as a
  detached task. The start node's ACK to the client doesn't wait
  for backups — they're eventually consistent.
- Failure of any non-start chunk fails the whole PUSH; the client
  drains the body and sees `ERR fan-out …`.

## Sequence: FILE PULL

```
client       node 0 (puller)   node 1            node 2
  │              │                │                │
  │── PULL ─────▶│                │                │
  │              │── GET-CHUNK ─▶ (chunk 1 owner)   │
  │              │── GET-CHUNK ────────────────  ▶ (chunk 2 owner)
  │              │                │                │
  │              │◀── chunk 1 ────│                │
  │              │◀── chunk 2 ───────────────────  │
  │              │                │                │
  │              │  (chunk 0 served from local content/)
  │              │                │                │
  │              │  if a chunk request fails → fall through
  │              │  to predecessor's backup/ via GET-BACKUP-CHUNK
  │              │                │                │
  │◀── chunk 0 ──│                │                │
  │◀── chunk 1 ──│                │                │
  │◀── chunk 2 ──│                │                │
```

Key points:

- The puller fetches all chunks in parallel (`FuturesOrdered`,
  cap 4) and writes them to the client in chunk-index order.
- A failed `GET-CHUNK` (TCP error, integrity-check fail) triggers
  fall-through to the predecessor's `GET-BACKUP-CHUNK`.
- If both owner and predecessor are dead, the puller emits the
  bytes it has plus `\nERR truncated expected=<E> got=<G>\n` so
  aware clients can detect the loss.

## Sequence: heal flow

```
detector       dead node       respawned       predecessor of dead
  │                               │
  │── PING ──▶ ECONNREFUSED       │
  │   (after gossip_interval)     │
  │                               │
  │── exec --addr <dead_addr> ─────▶ (fresh process)
  │                                  ├ writes VERSION marker
  │                                  └ sweeps *.partial orphans
  │                               │
  │── share NETMAP/TOPOLOGY/  ────▶│
  │   FILE TAGS/NODE NEXT          │
  │                               │
  │── walk our backup/ ────────────────────▶ each chunk
  │   (anti-entropy)                          via FILE CONTENT-PUSH
  │                               │
  │── broadcast NETMAP SET (Alive)
```

The detector is the **predecessor** of the dead node (the one
gossipping `NODE PING` at it). After respawn, that same predecessor
runs the anti-entropy refill — its own `backup/` directory is, by the
ring's replication invariant, exactly the set of chunks the
respawned node should hold in `content/`.

## Storage durability

Per-chunk write path (`durably_write_chunk` in `src/server.rs`):

1. Open `<dir>/<chunk>.partial`.
2. Stream body bytes through a SHA-256 hasher to disk.
3. Append the 32-byte hash trailer.
4. `flush()` and (if `mode >= Data`) `sync_all()` the file.
5. `rename(2)` to `<dir>/<chunk>` (POSIX-atomic within a directory).
6. (If `mode == Full`) open the directory and `sync_all()` it.

Read path (`open_chunk_verified`):

1. Read the whole file into memory.
2. Split off the trailing 32 bytes as the expected hash.
3. Recompute SHA-256 over the body.
4. Compare; on mismatch return `InvalidData` so the caller falls
   through to backup.

Crash safety: a crash mid-write leaves a `*.partial` file. The
janitor on the next `bind()` sweeps these. The final filename never
appears until the rename completes.

## Module layout

```
src/
├── auth.rs         AuthToken (HMAC-SHA256 PSK)
├── bin/main.rs     CLI: run, gateway, dev-network
├── gateway.rs      HTTP API + TCP-proxy
├── lib.rs          Re-exports
├── node.rs         Node state + helpers (gossip, topology, file_tags)
├── node_status.rs  enum NodeStatus { Alive, Dead }
├── protocol.rs     Command enum + parse_line
└── server.rs       Wire-protocol handlers, accept loop, heal flow
```

Tests live under `tests/`; the in-process harness is `tests/common/`.
The subprocess heal tests (`tests/heal_subprocess.rs`) exercise the
full exec-respawn path that the in-process harness can't reach.
