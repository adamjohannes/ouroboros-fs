# OuroborosFS — Claude Code Project Memory

This file is loaded into every Claude Code session opened in this directory. It captures the
architecture, conventions, and known gotchas that a session needs to know before touching
code. **Keep it tight** — every line costs context.

For deeper background:
- `README.md` §2 — How It Works (architecture overview).
- `docs/01-06` — narrative tutorial (AI-generated, recently realigned with code).
- `NEXT_STEPS.md` — production-readiness roadmap.

---

## Architecture in 90 seconds

OuroborosFS is a ring-topology distributed file store. N nodes form a logical ring; each
knows only its successor (`next_port`). A separate Gateway can run alongside as an
HTTP+TCP-proxy entry point.

**File push (fan-out, NOT chain)**:

1. Client sends `FILE PUSH <size> <name>` to a "start node."
2. Start node walks its topology snapshot to find each chunk's owner. Chunk 0 stays local;
   chunks 1..N-1 each go to a different node.
3. Start node opens N-1 outbound TCPs **in parallel** and sends one
   `FILE PUSH-CHUNK <name> <chunk_size> <file_size> <parts> <index> <start_port>` per
   connection.
4. As bytes arrive from the client, the start node streams `fair_chunk_len(0)` bytes to its
   own `content/`, then forwards `fair_chunk_len(i)` bytes to outbound conn `i` for each
   `i in 1..N-1`.
5. Awaits `OK\n` ACK from each backend; ACKs the client.

**File pull (parallel)**: `pull_file_from_ring` snapshots the topology, builds a per-chunk
fetch plan, fires all chunk requests concurrently via `FuturesOrdered` (cap 4), streams
results to the client in chunk-index order. Failed chunks fall back to the predecessor's
`/backup` directory; failed ports are deduped and broadcast at most once per pull.

**Backup is push, not notify+pull**: every save spawns a fire-and-forget
`push_to_predecessor` task that opens one TCP, sends `FILE BACKUP-PUSH <name> <size>` + raw
bytes, closes. Predecessor unreachable → log-and-swallow.

**Fault tolerance**: gossip loop sends `NODE PING` to next neighbor every
`gossip_interval` ms. On failure, healer broadcasts `NETMAP SET` (Dead), respawns the dead
node by exec'ing the same binary, then `share_data_with_new_node` copies NETMAP/TOPOLOGY/
FILE-TAGS to it, and broadcasts Alive.

---

## Layout

```
src/
  bin/main.rs         CLI: `run` (single node) and `set-network` (dev-only multi-spawn).
  lib.rs              Re-exports: Gateway, Node, NodeStatus, Command, parse_line, run, bind, serve.
  server.rs   1.9 kLOC. All wire-protocol handlers, gossip, heal. Bind/serve split for tests.
  protocol.rs   840 LOC. `Command` enum + `parse_line` (line-based, single-space separator).
  node.rs       810 LOC. Node state: next_port, file_tags, network_nodes, topology_map,
                storage_root, respawn_dead, netmap_broadcasts (counter for tests).
  gateway.rs    600 LOC. HTTP REST + transparent TCP-proxy. Sniffs first line:
                GET/POST/OPTIONS → HTTP; else TCP-proxy (which has a known deadlock — see
                gotchas below).
  node_status.rs    7 LOC. enum NodeStatus { Alive, Dead }.

tests/
  common/mod.rs       In-process harness: spin_up, spin_up_with_gateway, push_bytes,
                      pull_bytes, kill_node, http_get/post/options, child_ring (subprocess).
                      KillerGuard for panic-safe child cleanup.
  round_trip.rs       SHA-256 round-trip (small, 5-node, two-files, concurrency stress,
                      fan-out variants, large file #[ignore]'d).
  failover.rs         kill-one + adjacent-double-failure + no-double-broadcast.
  safety.rs           Oversized-PUSH OOM probes + gateway 413.
  server_handlers.rs  20 tests for handlers the round_trip suite doesn't directly hit.
  gateway_http.rs     17 active + 2 ignored (TCP-proxy deadlock pinned).
  heal_subprocess.rs  Single #[ignore]d test that exercises the binary-respawn path.
  no_literal_nodes_path.rs    CI grep gate; fails if any "nodes/" literal appears in
                              src/server.rs outside the binary's run() wrapper.
```

---

## Test counts (current)

166 default tests, 4 ignored. Lib unit: 109 (node/protocol/server). Integration: 57 across
6 binaries. Full default suite: ~3-4 s. `cargo test --release -- --ignored full_heal
large_file_streaming` adds ~3 s.

The PR-only CI job runs the unstuck ignored tests. The two `gateway_tcp_proxy_*` tests stay
ignored (pinned bug — see gotchas).

---

## Build & test commands

```bash
cargo build --release                                # production build
cargo test                                           # default suite (~3-4 s)
cargo test --release -- --ignored full_heal large_file_streaming    # all unstuck tests
cargo run --release -- set-network --nodes 5 --base-port 7000 --dns-port 8000   # dev ring
cargo llvm-cov --html                                # coverage report (rustup-managed
                                                     # toolchain only; see gotchas)
```

Project slash commands abbreviate the common patterns:

- `/run-stress` — runs `cargo test` N times with bounded timeouts; prints flake summary.
- `/lint-rust` — `cargo fmt --check` + `cargo clippy -- -D warnings` + `cargo doc --no-deps`.
- `/coverage` — wraps `cargo llvm-cov --html` with the toolchain workaround.
- `/protocol-grep <COMMAND>` — finds the variant, parser branch, dispatch arm, handler, and
  test references for a given protocol command name.

---

## Conventions

### Code style (see `~/.claude/CLAUDE.md` for the project-level base)

- No comments unless the *why* is non-obvious. Well-named identifiers explain *what*.
- Prefer early returns over nested conditionals.
- No docstrings on private functions.
- Don't add error handling for impossible cases. Trust internal invariants.
- Bug fixes don't need refactors. Three similar lines beat a premature abstraction.

### Test patterns

- Every async integration test: `#[tokio::test(flavor = "multi_thread")]`. Single-threaded
  flavor serializes accept loops and creates artificial deadlocks.
- Each `Ring` owns a `TempDir` via `RingOpts.storage_root` (set by harness). Concurrent
  test runs never collide on disk. Bound to `127.0.0.1:0` so OS picks free ports.
- `push_bytes` has a **300 ms settle window** baked in. The start node ACKs the client
  before every downstream chunk has been saved; without the settle, fast follow-up PULLs
  see missing chunks. **Don't remove this**; the proper fix is a v2 protocol change.
- Gateway tests need a **5 s window** for the gateway accept loop under heavy parallel test
  load. The HTTP client retries connect on `ECONNREFUSED` for 1 s.
- Tests that flip a node Dead use **gossip_interval = 200 ms** + sleep(800-1500 ms) before
  asserting on netmap state. Gossip detection has a 500 ms timeout per ping.
- Pinning a known bug? Use `#[ignore]` with a comment block explaining the contract for the
  fix. See `tests/gateway_http.rs::gateway_tcp_proxy_*` for the pattern.

### Tracing in tests

`tracing-subscriber::fmt::try_init()` quietly fails on second call (the global subscriber
is set-once). Tests don't init their own subscriber; cargo's `--nocapture` shows the binary's
default. To assert log content: use `Node.netmap_broadcasts: AtomicU64` (already wired) or
similar atomic counters added to `Node` rather than scraping tracing output.

### Storage paths

Every chunk path **must** go through `node.storage_root.join(...)`. Literal `"nodes/..."` is
banned in `src/server.rs` outside the binary's `run()` wrapper. The
`tests/no_literal_nodes_path.rs` grep gate enforces this. If you see a literal `"nodes/"`
slip in, that's how the harness tests collide on disk under parallel runs.

### Wire protocol changes

The protocol is line-delimited ASCII, single-space separator, optional binary payload after
the header line. **`parse_line` splits on literal `' '`, not whitespace** — tabs and
multi-space inputs don't work. Adding a new `Command` variant means: enum + parser branch +
`server.rs` dispatch arm + handler + `protocol.rs` unit test + integration test in the
right `tests/*.rs` file. The PR pattern is documented in old commits (search `git log
--grep "feat: add"` for examples).

---

## Known gotchas (read these before touching the relevant code)

### 1. `handle_tcp_proxy` deadlocks for most clients

`gateway.rs::handle_tcp_proxy` does `try_join!(client→server, server→client)` of two
`tokio::io::copy` halves. Both halves only return on EOF of their reader. The **ring's**
`handle_client` loop never closes proactively; it only closes when the client's write half
closes — which the gateway only does after `try_join!` finishes. Net result: deadlock until
OS-level TCP keepalive eventually tears it down.

Two ignored tests pin this: `gateway_tcp_proxy_passes_through_node_status` and
`gateway_tcp_proxy_passes_through_file_push_pull`. Run them with
`cargo test gateway_tcp_proxy -- --ignored` after a fix.

The fix is to switch from `try_join!` to a select-loop with idle detection, or to
short-circuit single-shot text-protocol commands by reading the line, forwarding it,
reading the response, and closing both halves manually.

### 2. `sanitize_filename` doesn't block `..`

`src/server.rs::sanitize_filename` replaces `/`, `\`, `\0`, `:`, `|`, `;`, `\n`, `\r` with
`_`. It does **not** filter `.` or `..`. Pinned by
`server::tests::sanitize_filename_known_traversal_gap`. Combined with the lack of auth on
the wire protocol, this is a path-traversal vector in production. See `NEXT_STEPS.md` §2.3.

The fix is a stricter allowlist (ASCII alphanumerics + `.`, `-`, `_`, plus the
`part-NNN-of-MMM` suffix). Reject non-conforming names with `ERR` rather than rewriting.

### 3. `host_str` and `host_of` mishandle IPv6

Both functions split on the first colon. `host_of("[::1]:7000")` returns `"["`. Pinned by
`node::tests::host_str_ipv6_brackets_known_gap` and the `host_of_ipv6_bracket_pin` test in
`server.rs`. Fine for IPv4-only deployments; not fine for v2.

### 4. Async-relay race on PUSH

The start node (in `handle_file_push`) ACKs the client before downstream `PUSH-CHUNK`
receivers have all reported `OK`. Wait — actually that's wrong, **post-PR7** the start
node *does* await every backend's `OK` before acking the client. But the **backup-push**
that each receiver fires is fire-and-forget. So a fast pull from a non-start node may see
chunks where backups haven't landed yet.

The 300 ms `push_bytes` settle window in the harness papers over this for tests. Production
deployment: backups are eventually consistent; the system tolerates this because the
content path on each chunk owner is durable (modulo `NEXT_STEPS.md` §1.1's fsync gap).

### 5. Adjacent two-node failure → silent file truncation

If a chunk's owner *and* its predecessor (the backup holder) both die between PUSH and
PULL, `pull_file_from_ring` emits a short file with no error signal. Pinned by
`failover::adjacent_double_failure_corruption_pin`. The fix is a length signal in the PULL
response shape — wire-protocol break, deferred to v2. See `NEXT_STEPS.md` §3.1.

### 6. `cargo llvm-cov` mismatch on this machine

The user's `rustc` is from Homebrew (`/opt/homebrew/Cellar/rust/`); `cargo-llvm-cov` looks
in the rustup-managed toolchain (`~/.rustup/toolchains/`). To run coverage locally:

```bash
rustup default stable                    # ensure rustup toolchain is active
rustup component add llvm-tools-preview
cargo llvm-cov --html
```

CI doesn't have this problem (uses `dtolnay/rust-toolchain@stable`).

### 7. `Path::new(&name).file_name().unwrap().to_str().unwrap()` panics on bad inputs

`server.rs:744` — a `FILE PUSH 0 ..` panics the start node. Single-client DoS via empty
file name. Tied to gotcha #2; fixed by the same allowlist work.

### 8. `respawn_dead = true` only in the binary

The in-process harness sets `respawn_dead = false` when calling `Node::new`. This means
tests can `kill_node()` without the rest of the ring exec'ing the binary to bring it back
(which would then survive the test runtime as an orphan). **The `handle_node_death`
exec-respawn path is therefore only exercised by the `#[ignore]`d `heal_subprocess` test.**

If you're debugging a heal issue and your in-process test isn't reproducing it, switch to
the subprocess test pattern.

---

## Don't do these

1. **Don't add `assert_cmd`, `reqwest`, `scopeguard`, or other "convenience" dev-deps.** The
   harness hand-rolls everything (HTTP client, RAII guards) on purpose. New deps cost
   compile time, supply-chain surface, and onboarding friction.
2. **Don't change `parse_line`'s single-space separator.** Lots of test inputs depend on it
   exactly. Multi-space tolerance would mask malformed-input bugs.
3. **Don't write to `nodes/` from a test.** Always go through `node.storage_root`. The grep
   gate fails the build otherwise.
4. **Don't introduce a new `unwrap()` in production code paths.** They exist (audited in
   `NEXT_STEPS.md` §5.1) but new ones should be `expect("explanation")` or proper error
   handling.
5. **Don't bump dependencies casually.** `Cargo.lock` is committed; bumps need a reason.
6. **Don't run `cargo test -- --test-threads=1` to fix flakes.** The flakes were real;
   serializing hides them. Either fix the timing (settle window, retry budget) or `#[ignore]`
   with a comment block.
7. **Don't add `#[derive(Debug)]` on types that contain user content** (file names, chunk
   bytes) without thinking about log leaks. See `NEXT_STEPS.md` §2.5.
8. **Don't commit `nodes/` or `scripts/nodes/`.** They're gitignored as `nodes` in the
   `.gitignore` line; leftover dev runs go there.

---

## Operational notes

- The binary's default listen address is `127.0.0.1:9000`. Override with `--addr`,
  `--port`, or `PORT` env var.
- Per-node max file size is `--file-size <bytes>` (default 1 GB; `0` disables). The cap
  exists to prevent OOM from oversized PUSH headers; see `NEXT_STEPS.md` §2.4.
- Each node persists chunks under `<storage_root>/<port>/{content,backup}/`. The binary
  uses `nodes/` rooted at cwd; tests use a tempdir.
- Gossip default is 5 s. Tests use 200 ms when they need fast failure detection.
- `tests/no_literal_nodes_path.rs` is the storage-root-threading regression net. Don't
  silence it; fix the offending literal.

---

## Recent activity

This session and the few before it landed:

- The fan-out PUSH refactor (PR-T0 through PR7 in commit history).
- The docs realignment (`docs/04` rewrite, README §2/§4 rewrite, inline rustdoc cleanup).
- 110 new tests across unit + integration + subprocess heal.
- CI coverage job + PR-only "all tests" job.
- `NEXT_STEPS.md` — the production-readiness roadmap. Read this before tackling any
  P0 item.

The 8-PR refactor's plan was at `~/.claude/plans/merry-wiggling-puddle.md` (now overwritten
by later plans). For historical context, the merge sequence is in `git log --oneline`.
