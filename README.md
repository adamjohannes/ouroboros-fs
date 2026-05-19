# OuroborosFS

![Build Dev](https://github.com/adamjohannes/ouroboros-fs/actions/workflows/build_dev.yml/badge.svg)
![Build and Test Release](https://github.com/adamjohannes/ouroboros-fs/actions/workflows/build_and_test_release_release.yml/badge.svg)

---

| ![OuroborosFS Logo](docs/assets/ouroboros_fs_logo.png) |
|:------------------------------------------------------:|

---

This project is a distributed, fault-tolerant, ring-based network for file storage, written in Rust. It also includes an
optional **web-based dashboard** built with Vue.js for visualizing the network and managing files.

It allows you to spawn multiple server nodes that automatically wire themselves into a ring topology. Files pushed to
the network are **sharded** (split) and distributed across all nodes. The network is **self-healing**: it detects node
failures, automatically respawns them, and reintegrates them into the ring by syncing the network state.

It also includes an optional **gateway service** that acts as a single entry point, automatically proxying client
requests (both raw TCP and HTTP) to any healthy node in the ring.

---

## Index

1. [Core features](#1-core-features)
2. [How It Works](#2-how-it-works)
    1. [File Storage](#21-file-storage)
    2. [Data Replication](#22-data-replication)
    3. [Fault Tolerance](#23-fault-tolerance)
    4. [Gateway Service (TCP Proxy & HTTP API)](#24-gateway-service-tcp-proxy--http-api)
3. [Getting Started](#3-getting-started)
   1. [Tutorial](#31-tutorial)
   2. [Build the Backend](#32-build-the-backend)
   3. [Run a Network](#33-run-a-network)
   4. [Run the Web Dashboard (Optional)](#34-run-the-web-dashboard-optional)
   5. [Running the Tests](#35-running-the-tests)
4. [Protocol Overview](#4-protocol-overview)
   1. [Client Commands](#41-client-commands)
   2. [Internal (Node-to-Node) Commands](#42-internal-node-to-node-commands)
5. [Test Coverage](#5-test-coverage)

## 1. Core Features

- **Distributed File Storage:** Files are automatically sharded (split) and stored in chunks across all nodes in the
  ring.
- **Data Replication:** Implements a single-neighbor backup model. Each node automatically stores a backup copy of the
  data chunks from its successor node.
- **Self-Healing Ring:** Nodes constantly check their neighbors. If a node crashes, its neighbor detects the failure,
  respawns the dead node, and syncs the network state (topology, file locations) to the new process.
- **Optional Gateway Service:** Run a single-entry-point gateway that discovers healthy nodes and proxies requests. It
  supports a raw TCP proxy for `netcat` clients and an **HTTP REST API** for the web dashboard.
- **Web-based Dashboard:** Includes a Vue.js frontend to visualize the network status in real-time, list all distributed
  files, and upload new content via the gateway's HTTP API.
- **Manual Healing:** A `NODE HEAL` command allows a client to trigger a ring-wide health check, forcing every node to
  check its neighbor and initiate the healing process for any dead nodes it finds.
- **Automatic Discovery:** Includes protocols for mapping the ring's topology (`TOPOLOGY WALK`) and discovering the
  status of all nodes (`NETMAP DISCOVER`).
- **Simple Text Protocol:** Interaction is done via a simple, line-based text protocol, easily accessible with tools
  like `netcat`.

---

## 2. How It Works

### 2.1. File Storage

The system shards files across the network for distributed storage. Each node stores its chunks in a
`nodes/<port>/content/` directory.

* **File Push (fan-out):**

    1. A client sends a `FILE PUSH <size> <name>` command to any node — the *start node*.
    2. The start node computes the chunk count from its known network size (`N`) and walks its topology
       snapshot to identify each chunk's owner. Chunk `0` stays local; chunks `1..N-1` go to the next
       `N-1` nodes around the ring.
    3. It opens `N-1` outbound TCP connections **in parallel** and sends one
       `FILE PUSH-CHUNK <chunk_name> <chunk_size> <file_size> <parts> <index> <start_port>` header per
       connection up front.
    4. As bytes arrive from the client it streams `fair_chunk_len(0)` bytes to its own
       `content/` directory, then forwards `fair_chunk_len(1)` bytes to the conn for chunk 1,
       `fair_chunk_len(2)` bytes to the conn for chunk 2, and so on.
    5. After all bytes are forwarded, the start node awaits an `OK\n` ACK from each backend (with a
       per-push deadline). Any failed connect, write, or ACK turns into an `ERR ...` response to the
       client — the system favors visible, fast failure over partial saves.

* **File Pull (parallel):**

    1. A client sends a `FILE PULL <name>` command to any node.
    2. The node consults its internal `file_tags` map to find the file's size, its "start node"
       (holding chunk 1), and the total number of `parts`.
    3. It builds a fetch plan by walking the topology snapshot, then issues all chunk fetches
       concurrently via `FuturesOrdered` with an in-flight cap of `4` (so a slow client can't
       accumulate the whole file in RAM).
    4. **Happy path:** Each fetch sends a `FILE GET-CHUNK` command to the chunk's owner; results are
       streamed to the client in chunk-index order as they complete.
    5. **Failure path:** If a chunk fetch fails, the originating node:
       a. Records the dead port (deduped — at most one entry per dead host per pull).
       b. Looks up the dead node's **predecessor** in a pre-built reverse map and sends
          `FILE GET-BACKUP-CHUNK` to it.
    6. After the loop completes, **one** netmap broadcast announces every dead host that surfaced
       during the pull (pre-refactor, the same dead host could trigger N broadcasts).

### 2.2. Data Replication

In addition to sharding, the network automatically replicates data for extra resilience. It uses a single-neighbor
backup model: **each node is responsible for backing up the data of its successor (neighbor).**

For example, in a `7000 -> 7001 -> 7002` ring:

- Node `7000` will back up data from Node `7001`.
- Node `7001` will back up data from Node `7002`.
- Node `7002` will back up data from Node `7000`.

This is achieved with a single push per chunk:

1. **Store Content:** When a node receives a chunk (e.g., `a.txt.part-002-of-003`), it streams the bytes
   straight to its local `nodes/<port>/content/` directory.
2. **Push to Predecessor:** Immediately after the local save flushes, the node spawns a fire-and-forget
   task. The task opens a single TCP connection to its predecessor, sends
   `FILE BACKUP-PUSH <chunk_name> <size>` followed by `<size>` raw bytes, and closes.
3. **Store Backup:** The predecessor's `handle_file_backup_push` streams those bytes straight to its
   own `nodes/<port>/backup/` directory and replies `OK\n`.

If the predecessor is unreachable, the failure is logged and the local content save still stands —
the chunk just isn't backed up until the next push for that chunk lands.

### 2.3. Fault Tolerance

The network actively monitors and heals itself.

1. **Gossip:** Each node runs a "gossip loop" to send a `NODE PING` command to its next neighbor.
2. **Detection:** If the neighbor doesn't respond with `PONG`, it's assumed to be dead.
3. **Healing:** The detecting node immediately:
    - Marks the neighbor as `Dead` in its local network map.
    - Broadcasts this updated map to all other nodes (`NETMAP SET`).
    - **Respawns** the dead node by executing a new process.
    - Waits for the new node to boot up.
    - Shares all critical state (`NETMAP SET`, `TOPOLOGY SET`, `FILE TAGS-SET`) with the new node to bring it up to
      speed.
    - Marks the node as `Alive` and broadcasts the final update.
4. **Proactive Detection:** The `FILE PULL` operation also actively detects failures. If it fails to retrieve a chunk
   from a node, it will immediately mark that node as `Dead` and broadcast the update, often detecting failures faster
   than the gossip loop.

### 2.4. Gateway Service (TCP Proxy & HTTP API)

You can optionally run a gateway service using the `--dns-port` flag when running `set-network`. This service acts as a
simple, stateless proxy and single entry point for the network.

When a client connects, the gateway "sniffs" the first line of the request to determine its type:

* **HTTP API:** If the request starts with `GET`, `POST`, or `OPTIONS`, the gateway handles it as an HTTP request. This
  serves a REST API used by the web dashboard, providing endpoints like:
    - `GET /netmap/get`: Returns a JSON map of all nodes and their `Alive`/`Dead` status.
    - `GET /file/list`: Returns a JSON list of all known files.
    - `GET /file/pull/<name>`: Streams the raw file bytes for download.
    - `POST /file/push`: Accepts raw file bytes (as `application/octet-stream`) to push a new file to the network.
    - `POST /network/heal`: Triggers a manual, ring-wide network heal.
    - `POST /node/<port>/kill`: Sends a kill signal to a specific node process.
* **TCP Proxy:** If the request is not HTTP, the gateway assumes it's a text-based protocol command (like
  `FILE PUSH ...`). It checks its internal, cached list of healthy nodes, finds one that is `Alive`, and transparently
  proxies the entire TCP connection to that node.

This provides a single, stable entry point for the network, so clients don't need to know the address of any specific
node.

---

## 3. Getting Started

### 3.1. Tutorial

There's a learning material that was generated with the help of
the [AI Codebase Knowledge Builder](https://github.com/The-Pocket/PocketFlow-Tutorial-Codebase-Knowledge) project. You
can access it [here](./docs/index.md).

### 3.2. Build the Backend

You'll need the Rust toolchain installed.

```bash
cargo build --release
```

### 3.3. Run a Network

The easiest way to start is using the `set-network` subcommand, which spawns and wires up a ring for you. The
`--dns-port` flag starts the gateway, which provides both the TCP proxy and the HTTP API.

```bash
# This will start 5 nodes (7000-7004) AND a gateway service on port 8000
# The gateway will provide the HTTP API on http://127.0.0.1:8000
# and also act as a TCP proxy on the same port.
cargo run --release -- set-network \
    --nodes 5 \
    --base-port 7000 \
    --dns-port 8000
```

This command will block, holding the network open.

`set-network` also accepts `--file-size <bytes>` (default `1_000_000_000`, i.e., 1 GB) which caps the
maximum size of a single accepted file per node. Pass `0` to disable the cap. The same flag exists on
the `run` subcommand if you start nodes individually.

Each node persists its chunks under `nodes/<port>/content/` and backups under `nodes/<port>/backup/`,
rooted at the binary's working directory. The `Node` struct internally takes a `storage_root` parameter
so the in-process test harness can redirect this to a tempdir; the binary always uses `nodes/`.

### 3.4. Run the Web Dashboard (Optional)

The web dashboard is a separate Vue.js application. You'll need Node.js and `npm` installed.

```bash
# In a new terminal, navigate to the GUI directory
cd src-gui/ouroborosfs-vue-gui

# Install dependencies
npm install

# Run the frontend development server
npm run dev
```

This will typically make the dashboard available at `http://localhost:5173`. It is pre-configured to communicate with
the gateway API running on `http://127.0.0.1:8000`. Here's a preview of how it looks like:

| ![OuroborosFS Dashboard](docs/assets/ouroboros_fs_dashboard.png) |
|:----------------------------------------------------------------:|

### 4. Interact with the Network

You now have two ways to interact with the network:

#### Option A: Command Line (via Gateway)

The [`scripts/`](./scripts) directory contains helpers for interacting with the ring using `netcat`. If you are running
the **gateway service** (e.g., on port 8000), you can point all scripts to that single port.

```bash
# Push this project's Cargo.toml file (via the gateway on port 8000)
./scripts/push_file.sh -p 8000 -f Cargo.toml

# List all distributed files (via the gateway)
./scripts/list_files.sh -p 8000

# Pull the file back (via the gateway and save it as 'downloaded_file')
./scripts/pull_file.sh -p 8000 -f Cargo.toml > downloaded_file

# Get the status of all nodes (via the gateway)
./scripts/get_nodes.sh -p 8000

# Trigger a manual ring-wide heal walk
./scripts/heal_network.sh -p 8000

# Kill a specific node by port
./scripts/kill_node.sh 7002

# Kill every node spawned by set-network
./scripts/kill_all_nodes.sh

# List node processes (lsof against the configured ports)
./scripts/get_processes.sh
```

#### Option B: Web Dashboard

If you completed Step 3, open the URL from the `npm run dev` output in your browser.

You can use the dashboard to:

- See a live-updating graph of all nodes and their status.
- See a list of all files stored in the network.
- Upload new files using the "Share File" button.

### 3.5. Running the Tests

The repository ships with a unit + integration test suite that runs in-process — no need to spin up
a binary network. The integration harness spawns nodes inside the test runtime, wires them through
ephemeral ports, and tears them down via `tempfile::TempDir`.

```bash
# Default suite (~3 s wall-clock)
cargo test

# Full suite including the 100 MB streaming round-trip
cargo test --release -- --ignored
```

CI runs `cargo test --verbose` on every push and pull request — see
`.github/workflows/build_and_test_release_release.yml`.

---

## 4. Protocol Overview

The server's *internal* node-to-node and client-to-node communication uses a simple, line-based ASCII text protocol.
Commands follow a `<NOUN> <VERB> [params...]` structure.

> [!NOTE]
> This is separate from the HTTP API provided by the gateway for the web dashboard.

### 4.1. Client Commands

These are the primary commands you would send to a node (or the gateway) via `netcat`.

- **`NODE NEXT <addr>`**: Sets the next hop for a node to form the ring.
- **`NODE STATUS`**: Asks a node for its port and configured next hop.
- **`NODE HEAL`**: (Client -\> any node) Initiates a manual, ring-wide heal walk.
- **`NETMAP GET`**: Asks a node for its current view of the network map (all nodes and their `Alive`/`Dead` status).
- **`NETMAP DISCOVER`**: (Client -\> any node) Initiates a ring walk to discover all nodes.
- **`TOPOLOGY WALK`**: Initiates a ring walk to map the connections (e.g., `7000->7001;7001->7002`).
- **`FILE PUSH <size> <name>`**: Initiates a file upload. The client must send this header line, followed by *exactly*
  `<size>` bytes of binary data.
- **`FILE PULL <name>`**: Requests a file. The node responds with the *raw* binary file data, with no headers or
  trailers.
- **`FILE LIST`**: Asks a node for a CSV-formatted list of all known files and their metadata.

### 4.2. Internal (Node-to-Node) Commands

These commands are used by the nodes to communicate with each other.

- **`NODE PING`**: Health check. Expects a `PONG` response.
- **`NODE HEAL-HOP <token> <start_addr>`**: Continues a heal walk to the next node.
- **`NODE HEAL-DONE <token>`**: Sent by the last node back to the start to complete the heal walk.
- **`NETMAP SET <entries>`**: Broadcasts an updated network map (e.g., `7000=Alive,7001=Dead`) to another node.
- **`TOPOLOGY SET <history>`**: Broadcasts a complete topology map to another node.
- **`FILE TAGS-SET <entries>`**: Broadcasts the map of known files to another node (used during heal).
- **`FILE PUSH-CHUNK <name> <chunk_size> <file_size> <parts> <index> <start_port>`**: Sent by the start
  node to each chunk owner during a `FILE PUSH`. Followed by exactly `<chunk_size>` raw bytes;
  receiver replies `OK\n`.
- **`FILE GET-CHUNK <name>`**: Requests a specific file chunk from another node during a `FILE PULL` operation.
- **`FILE BACKUP-PUSH <name> <size>`**: Saving node pushes a just-saved chunk to its predecessor for
  backup. Followed by exactly `<size>` raw bytes; receiver replies `OK\n`.
- **`FILE GET-BACKUP-CHUNK <name>`**: (Node i -\> Node i-1) Requests a specific file chunk from the predecessor's
  `/backup` directory. Used by `FILE PULL` as a failover.

---

## 5. Test Coverage

OuroborosFS measures coverage with [`cargo-llvm-cov`]. The default suite (`cargo test`) is what the
coverage job in CI measures; the `#[ignore]`d `heal_subprocess` test exercises the binary-respawn
path under real child processes and is excluded from the headline numbers.

### Local

```bash
cargo install cargo-llvm-cov
rustup component add llvm-tools-preview

cargo llvm-cov                           # console summary
cargo llvm-cov --html                    # writes target/llvm-cov/html/index.html
cargo llvm-cov --lcov --output-path lcov.info
```

To exercise the gaps below (non-default — measured separately):

```bash
cargo test --release -- --ignored heal_subprocess
```

### CI

`.github/workflows/build_and_test_release_release.yml` defines a `coverage` job that runs on every
push and pull-request to `main` and uploads `lcov.info` as a build artifact named `lcov`. A floor
will be added in a follow-up once a measured baseline exists.

A separate `pr_full_tests` job runs **only on pull-requests targeting `main`**. It runs the default
suite plus the unstuck `#[ignore]`d tests (`heal_subprocess::full_heal_*` and
`large_file_streaming_100mb`) so PR authors get a regression signal on the slow paths the
default-on-push suite skips. The two `gateway_tcp_proxy_*` ignored tests are deliberately not
included — they're pinned to a known deadlock and would hang the job.

### Gaps reflected in the metric

- `src/server.rs::handle_node_death` lines 1644-1685 (binary-respawn + `share_data_with_new_node`)
  are reached only by the ignored `heal_subprocess::full_heal_respawns_dead_child_and_broadcasts`
  test. The in-process integration harness sets `respawn_dead=false` so killing a node doesn't
  exec a real binary that survives the test runtime.
- `src/gateway.rs::trigger_node_kill` is Unix-specific (`lsof`/`kill`). The
  `gateway_post_kill_unknown_port_returns_500` test covers the failure branch only; the success
  branch isn't hit by the in-process suite.
- `src/gateway.rs::handle_tcp_proxy` has a known deadlock with the current `try_join!` strategy
  (see `tests/gateway_http.rs::gateway_tcp_proxy_*` — `#[ignore]`d). The two passthrough tests
  are pinned for after the proxy fix.

[`cargo-llvm-cov`]: https://github.com/taiki-e/cargo-llvm-cov
