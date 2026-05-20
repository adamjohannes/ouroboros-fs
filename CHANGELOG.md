# Changelog

All notable changes to OuroborosFS are documented in this file. Format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The pre-1.0 history is summarized; per-PR detail is preserved in
`git log` and the section headings below correspond to the PR series
that landed each chunk of work (P0 sweep = A–D, P1 sweep = E–I,
P2 sweep = J–M).

## [Unreleased]

## [1.0.0-rc.1] — 2026-05-20

First release candidate. Closes every P0 and P1 item from
`NEXT_STEPS.md`, plus most P2 items.

### Added

- **Wire-protocol AUTH handshake** with HMAC-SHA256 over a 16-byte
  nonce. Pre-shared key (32 bytes / 64-char hex) via `--auth-token`
  or the `OUROBOROS_AUTH_TOKEN` env var. Disabled-mode is supported
  for development and logs a warning on startup. (Series C)
- **HTTP gateway bearer auth** on every non-OPTIONS request.
  `/health` and `/ready` are exempt so Kubernetes/Nomad probes work
  without credentials. (Series C, F)
- **Per-chunk durability**: SHA-256 trailer per stored chunk,
  atomic write-then-rename, optional `fsync`/dir-`fsync`
  (`--fsync-mode {none,data,full}`, default `full`). Startup
  janitor sweeps `*.partial` orphans from a prior crash. (Series B)
- **Storage version marker** (`<storage_root>/<port>/VERSION`).
  Refuses to start on incompatible trees. `OUROBOROS_FORCE_V1=1`
  escape hatch for already-v1 unmarked trees. (Series K)
- **Anti-entropy refill** on respawn. When a respawned node's disk
  was destroyed, the predecessor walks its own `backup/` tree and
  pushes every chunk back to the respawned node's `content/` via
  the new `FILE CONTENT-PUSH` command. (Series I)
- **PULL truncation signal**. When chunks are unrecoverable
  (adjacent owner+predecessor failure), the body is followed by a
  `\nERR truncated expected=<E> got=<G>\n` trailer. The gateway
  strips it from HTTP bodies and logs the failure. (Series D)
- **Strict filename allowlist** at the protocol parse boundary
  (`[A-Za-z0-9._-]`, length 1–255, no all-dot names). Closes the
  path-traversal gap pinned by the old `sanitize_filename` test.
  (Series A)
- **Connection limits**: `--idle-timeout <seconds>` (default 60),
  `--max-conns <n>` (default 1024). Saturated clients get
  `ERR server busy\n` and a prompt close. (Series E)
- **Graceful shutdown** on SIGTERM/SIGINT. Stop accepting,
  drain in-flight handlers via `JoinSet` up to
  `--shutdown-timeout` (default 30 s), then `abort_all`. (Series G)
- **Standalone `gateway` subcommand** for production deploys. New
  systemd sample units in `samples/systemd/` (templated node unit,
  gateway unit, shared environment file). Renamed `set-network` to
  `dev-network` (alias preserved for back-compat). (Series H)
- **Prometheus `/metrics`** endpoint on the gateway. Per-node
  atomic counters (`pushes_total`, `pulls_total`, `chunk_bytes_*`,
  `dead_nodes`, etc.) aggregated via a new `NODE METRICS` internal
  command. Bearer-protected. (Series H)
- **TOML `--config` file** support on `run` and `gateway`. CLI
  flags > config-file > built-in defaults. Sample configs in
  `samples/config/`. (Series K)
- **JSON log format** via `--log-format json` (default `text`).
  Structured fields go straight to Splunk/ELK/Datadog. (Series F)
- **`/health` and `/ready` endpoints** on the gateway. (Series F)
- **Operator documentation**: `docs/operations.md` (cluster backup,
  capacity planning, runbooks), `docs/ARCHITECTURE.md`
  (topology + sequence overview), `docs/SECURITY.md` (threat
  model). (Series M, Series D)
- **Lint CI gate**: `cargo clippy --all-targets -- -D warnings`,
  `cargo fmt --check`, and `cargo doc --no-deps` with
  `RUSTDOCFLAGS=-D warnings`. (Series L)

### Changed

- **`Node` `Debug` impl is now manual.** Sensitive fields
  (`storage_root`, `file_tags`, `network_nodes`, `topology_map`,
  `pending_walks`, `pending_heals`) are *omitted* via
  `finish_non_exhaustive()`. `auth_token` is still emitted, but its
  own `Debug` impl prints `AuthToken(<redacted>)` so the secret
  doesn't leak. Prevents future `tracing::error!(?node, ...)` from
  dumping the full state. (Series J)
- **`set-network` renamed to `dev-network`** with deprecated alias.
  (Series H)
- **`POST /node/<port>/kill` removed** entirely from the gateway —
  it was a remote-RCE primitive. Operators have SSH + `pkill`.
  (Series C)
- **CORS `Access-Control-Allow-Origin: *` removed** from gateway
  responses. Internal-only deployment doesn't advertise CORS;
  re-add with a specific allowed origin if you need a dashboard.
  (Series C)

### Fixed

- **TCP-proxy deadlock** in the gateway. The two
  `gateway_tcp_proxy_*` tests are no longer `#[ignore]`d. (Series I)
- **Gateway `/file/pull/` empty filename** returns proper 404
  (was 200 + `ERR file not found` body). (Series F)
- **Gateway `/file/pull/<missing>`** returns 404 (was 200 +
  ring's `ERR` line). (Series F)
- **Storage root persisted on respawn**: `handle_node_death` now
  passes `--storage-root` explicitly, so on-disk content survives
  a node restart. (Series D)
- **Walk-token uniqueness**: confirmed `next_token` already
  prefixes the port; new regression test pins the contract.
  (Series J)

### Security

- **Path traversal closed**. The previous `sanitize_filename`
  rewriter allowed `..` to leak through; the new strict allowlist
  rejects it at parse. (Series A)
- **Filename-via-PUSH crash closed**. `Path::new(&name).file_name().
  unwrap()` is gone; bad names can't reach a handler. (Series A)
- **Wire AUTH** on every internal connect (gossip, fan-out PUSH,
  GET-CHUNK, BACKUP-PUSH, heal share, gateway → ring). (Series C)
- **`env_clear()` on respawn** with explicit PATH, RUST_LOG,
  OUROBOROS_AUTH_TOKEN passthrough. (Series C)

### Public API

The library crate is intentionally small. The stable surface for
v1.0 is:

- `ouroboros_fs::run` — single-node entry point.
- `ouroboros_fs::Gateway::new` / `with_auth` and `run_server`.
- `ouroboros_fs::AuthToken`, `FsyncMode`, `NodeStatus`.
- The wire protocol (`docs/01-06`, `docs/ARCHITECTURE.md`).

`bind`, `serve`, and `serve_with_shutdown` are re-exported under
`#[doc(hidden)]` (see `src/lib.rs:15`) for the in-process test
harness; they're not stable across patch releases. `Node` is
re-exported normally (it's part of the public surface so users can
read its fields), but its layout is not stable across major versions
— treat it as opaque except for documented accessors.

## [0.1.0] — Pre-1.0 history

Pre-1.0 development is summarized; see `git log` for per-commit
detail. Highlights:

- Ring-topology distributed file store with fan-out PUSH and
  parallel-fetch PULL.
- Predecessor-backup replication.
- Gossip-driven failover with binary respawn.
- Test harness (in-process and subprocess flavors).
- Coverage CI job.
