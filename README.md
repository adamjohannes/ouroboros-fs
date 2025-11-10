# OuroborosFS

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

## Core Features

* **Distributed File Storage:** Files are automatically sharded (split) and stored in chunks across all nodes in the
  ring.
* **Data Replication:** Implements a single-neighbor backup model. Each node automatically stores a backup copy of the
  data chunks from its successor node.
* **Self-Healing Ring:** Nodes constantly check their neighbors. If a node crashes, its neighbor detects the failure,
  respawns the dead node, and syncs the network state (topology, file locations) to the new process.
* **Optional Gateway Service:** Run a single-entry-point gateway that discovers healthy nodes and proxies requests. It
  supports a raw TCP proxy for `netcat` clients and an **HTTP REST API** for the web dashboard.
* **Web-based Dashboard:** Includes a Vue.js frontend to visualize the network status in real-time, list all distributed
  files, and upload new content via the gateway's HTTP API.
* **Manual Healing:** A `NODE HEAL` command allows a client to trigger a ring-wide health check, forcing every node to
  check its neighbor and initiate the healing process for any dead nodes it finds.
* **Automatic Discovery:** Includes protocols for mapping the ring's topology (`TOPOLOGY WALK`) and discovering the
  status of all nodes (`NETMAP DISCOVER`).
* **Simple Text Protocol:** Interaction is done via a simple, line-based text protocol, easily accessible with tools
  like `netcat`.

---

## How It Works

### File Storage

The system shards files across the network for distributed storage. Each node stores its chunks in a
`nodes/<port>/content/` directory.

* **File Push:**

    1. A client sends a `FILE PUSH <size> <name>` command to any node.
    2. That node determines the network size (N) from its known "netmap".
    3. It reads the first chunk (1/N) of the file, saves it locally to its `content/` directory, and forwards the *rest*
       of the file's binary stream to its neighbor using a `FILE RELAY-STREAM` command.
    4. This process repeats: the next node saves chunk 2/N to its `content/` directory and forwards the rest. This
       continues until all N chunks are stored on N different nodes.

* **File Pull:**

    1. A client sends a `FILE PULL <name>` command to any node.
    2. The node consults its internal `file_tags` map to find the file's size, its "start node" (holding chunk 1), and
       the total number of `parts`.
    3. It then iterates from chunk `1` to `N`, calculating which node in the ring *should* hold that specific chunk (
       e.g., `a.txt.part-002-of-003`).
    4. **Happy Path:** It sends a `FILE GET-CHUNK` command to the target node, which reads the chunk from its `content/`
       directory and returns it.
    5. **Failure Path:** If the target node is dead (request fails), the originating node:
       a. Marks the target node as `Dead` in its local netmap and broadcasts this update to the ring.
       b. Finds the dead node's **predecessor** (which holds the backup).
       c. Sends a `FILE GET-BACKUP-CHUNK` command to the predecessor, which reads the chunk from its `backup/` directory
       and returns it.
    6. The originating node reassembles all chunks in order and streams the complete file back to the client.

### Data Replication (Backup)

In addition to sharding, the network automatically replicates data for extra resilience. It uses a single-neighbor
backup model: **each node is responsible for backing up the data of its successor (neighbor).**

For example, in a `7000 -> 7001 -> 7002` ring:

* Node `7000` will back up data from Node `7001`.
* Node `7001` will back up data from Node `7002`.
* Node `7002` will back up data from Node `7000`.

This is achieved using an active notification workflow:

1. **Store Content:** When Node `7001` receives a file chunk (e.g., `a.txt.part-002-of-003`), it saves it to its local
   `nodes/7001/content/` directory.
2. **Notify Predecessor:** Immediately after saving, Node `7001` sends a `FILE NOTIFY-CHUNK-SAVED` command to its
   predecessor (Node `7000`), telling it the chunk name.
3. **Fetch for Backup:** Node `7000` receives this notification, connects back to Node `7001`, and requests the full
   chunk data using `FILE GET-CHUNK-FOR-BACKUP`.
4. **Store Backup:** Node `7000` receives the data and saves it to its local `nodes/7000/backup/` directory.

### Fault Tolerance

The network actively monitors and heals itself.

1. **Gossip:** Each node runs a "gossip loop" to send a `NODE PING` command to its next neighbor.
2. **Detection:** If the neighbor doesn't respond with `PONG`, it's assumed to be dead.
3. **Healing:** The detecting node immediately:
    * Marks the neighbor as `Dead` in its local network map.
    * Broadcasts this updated map to all other nodes (`NETMAP SET`).
    * **Respawns** the dead node by executing a new process.
    * Waits for the new node to boot up.
    * Shares all critical state (`NETMAP SET`, `TOPOLOGY SET`, `FILE TAGS-SET`) with the new node to bring it up to
      speed.
    * Marks the node as `Alive` and broadcasts the final update.
4. **Proactive Detection:** The `FILE PULL` operation also actively detects failures. If it fails to retrieve a chunk
   from a node, it will immediately mark that node as `Dead` and broadcast the update, often detecting failures faster
   than the gossip loop.

### Gateway Service (TCP Proxy & HTTP API)

You can optionally run a gateway service using the `--dns-port` flag when running `set-network`. This service acts as a
simple, stateless proxy and single entry point for the network.

When a client connects, the gateway "sniffs" the first line of the request to determine its type:

* **HTTP API:** If the request starts with `GET`, `POST`, or `OPTIONS`, the gateway handles it as an HTTP request. This
  serves a REST API used by the web dashboard, providing endpoints like:
    * `GET /api/nodes`: Returns a JSON map of all nodes and their `Alive`/`Dead` status.
    * `GET /api/files`: Returns a JSON list of all known files.
    * `POST /api/upload`: Accepts raw file bytes to push a new file to the network.
* **TCP Proxy:** If the request is not HTTP, the gateway assumes it's a text-based protocol command (like
  `FILE PUSH ...`). It checks its internal, cached list of healthy nodes, finds one that is `Alive`, and transparently
  proxies the entire TCP connection to that node.

This provides a single, stable entry point for the network, so clients don't need to know the address of any specific
node.

---

## Getting Started

### 1. Build the Backend

You'll need the Rust toolchain installed.

```bash
cargo build --release
```

### 2. Run a Network

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

### 3. Run the Web Dashboard (Optional)

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
the gateway API running on `http://127.0.0.1:8000`.

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
```

#### Option B: Web Dashboard

If you completed Step 3, open the URL from the `npm run dev` output in your browser.

You can use the dashboard to:

* See a live-updating graph of all nodes and their status.
* See a list of all files stored in the network.
* Upload new files using the "Share File" button.

---

## Protocol Overview

The server's *internal* node-to-node and client-to-node communication uses a simple, line-based ASCII text protocol.
Commands follow a `<NOUN> <VERB> [params...]` structure.

> [!NOTE]
> This is separate from the HTTP API provided by the gateway for the web dashboard.

### Client Commands

These are the primary commands you would send to a node (or the gateway) via `netcat`.

* **`NODE NEXT <addr>`**: Sets the next hop for a node to form the ring.
* **`NODE STATUS`**: Asks a node for its port and configured next hop.
* **`NODE HEAL`**: (Client -\> any node) Initiates a manual, ring-wide heal walk.
* **`NETMAP GET`**: Asks a node for its current view of the network map (all nodes and their `Alive`/`Dead` status).
* **`TOPOLOGY WALK`**: Initiates a ring walk to map the connections (e.g., `7000->7001;7001->7002`).
* **`FILE PUSH <size> <name>`**: Initiates a file upload. The client must send this header line, followed by *exactly*
  `<size>` bytes of binary data.
* **`FILE PULL <name>`**: Requests a file. The node responds with the *raw* binary file data, with no headers or
  trailers.
* **`FILE LIST`**: Asks a node for a CSV-formatted list of all known files and their metadata.

### Internal (Node-to-Node) Commands

These commands are used by the nodes to communicate with each other.

* **`NODE PING`**: Health check. Expects a `PONG` response.
* **`NETMAP SET <entries>`**: Broadcasts an updated network map (e.g., `7000=Alive,7001=Dead`) to another node.
* **`TOPOLOGY SET <history>`**: Broadcasts a complete topology map to another node.
* **`FILE RELAY-STREAM ...`**: Forwards a file chunk (and the remaining stream) to the next node during a `FILE PUSH`
  operation.
* **`FILE GET-CHUNK <name>`**: Requests a specific file chunk from another node during a `FILE PULL` operation.
* **`FILE NOTIFY-CHUNK-SAVED <name>`**: (Node i+1 -\> Node i) Notifies the predecessor node that a new chunk is
  available for backup.
* **`FILE GET-CHUNK-FOR-BACKUP <name>`**: (Node i -\> Node i+1) Requests the raw bytes of a specific chunk for backup.
* **`FILE GET-BACKUP-CHUNK <name>`**: (Node i -\> Node i-1) Requests a specific file chunk from the predecessor's
  `/backup` directory. Used by `FILE PULL` as a failover.