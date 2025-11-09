# OuroborosFS

[![Build Dev](https://github.com/hazardous-sun/rust-socket-server/actions/workflows/build_dev.yml/badge.svg)](https://github.com/hazardous-sun/rust-socket-server/actions/workflows/build_dev.yml)
[![Build and Test Release](https://github.com/hazardous-sun/rust-socket-server/actions/workflows/build_and_test_release_release.yml/badge.svg)](https://github.com/hazardous-sun/rust-socket-server/actions/workflows/build_and_test_release_release.yml)

---

This project is a distributed, fault-tolerant, ring-based network for file storage, written in Rust.

It allows you to spawn multiple server nodes that automatically wire themselves into a ring topology. Files pushed to
the network are **sharded** (split) and distributed across all nodes. The network is **self-healing**: it detects node
failures, automatically respawns them, and reintegrates them into the ring by syncing the network state.

-----

## Core Features

* **Distributed File Storage:** Files are automatically sharded (split) and stored in chunks across all nodes in the
  ring.
* **Self-Healing Ring:** Nodes constantly check their neighbors. If a node crashes, its neighbor detects the failure,
  respawns the dead node, and syncs the network state (topology, file locations) to the new process.
* **Automatic Discovery:** Includes protocols for mapping the ring's topology (`TOPOLOGY WALK`) and discovering the
  status of all nodes (`NETMAP DISCOVER`).
* **Simple Text Protocol:** Interaction is done via a simple, line-based text protocol, easily accessible with tools
  like `netcat`.

-----

## How It Works

### File Storage

The system shards files across the network for distributed storage.

* **File Push:**

    1. A client sends a `FILE PUSH <size> <name>` command to any node.
    2. That node determines the network size (N) from its known "netmap".
    3. It reads the first chunk (1/N) of the file, saves it locally, and forwards the *rest* of the file's binary stream
       to its neighbor using a `FILE RELAY-STREAM` command.
    4. This process repeats: the next node saves chunk 2/N and forwards the rest. This continues until all N chunks are
       stored on N different nodes.

* **File Pull:**

    1. A client sends a `FILE PULL <name>` command to any node.
    2. The node consults its internal `file_tags` map to find the file's size and its "start node" (the node holding
       chunk 1).
    3. It then "walks" the ring, starting from the start node, sending `FILE GET-CHUNK` to each node in sequence.
    4. Each node returns its local chunk of the file.
    5. The originating node reassembles the chunks in order and streams the complete file back to the client.

### Fault Tolerance

The network actively monitors and heals itself.

1. **Gossip:** Each node runs a "gossip loop" to send a `NODE PING` command to its next
   neighbor.
2. **Detection:** If the neighbor doesn't respond with `PONG`, it's assumed to be dead.
3. **Healing:** The detecting node immediately:
    * Marks the neighbor as `Dead` in its local network map.
    * Broadcasts this updated map to all other nodes (`NETMAP SET`).
    * **Respawns** the dead node by executing a new process.
    * Waits for the new node to boot up.
    * Shares all critical state (`NETMAP SET`, `TOPOLOGY SET`, `FILE TAGS-SET`) with the new node to bring it up to
      speed.
    * Marks the node as `Alive` and broadcasts the final update.

-----

## Getting Started

### 1. Build

You'll need the Rust toolchain installed.

```bash
cargo build --release
```

### 2. Run a Network

The easiest way to start is using the `set-network` subcommand, which spawns and wires up a ring for you.

```bash
# This will start 5 nodes on ports 7000, 7001, 7002, 7003, and 7004
# It will then wire them together (7000 -> 7001 -> ... -> 7004 -> 7000)
cargo run --release -- set-network --nodes 5 --base-port 7000
```

This command will block, holding the network open. Press `Ctrl-C` to shut down all child node processes.

### 3. Interact with the Network

The [`scripts/`](./scripts) directory contains helpers for interacting with the ring (via port 7000 by default) using
`netcat`.

```bash
# Push this project's Cargo.toml file to the ring
./scripts/push_file.sh Cargo.toml

# List all distributed files (output is CSV)
./scripts/list_files.sh

# Pull the file back (and save it as 'downloaded_file')
./scripts/pull_file.sh Cargo.toml > downloaded_file

# Get the status of all nodes in the network
./scripts/get_nodes.sh
```

-----

## Protocol Overview

The server communicates using a simple, line-based ASCII text protocol. Commands follow a `<NOUN> <VERB> [params...]`
structure.

### Client Commands

These are the primary commands you would send to a node.

* **`NODE NEXT <addr>`**: Sets the next hop for a node to form the ring.
* **`NODE STATUS`**: Asks a node for its port and configured next hop.
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
* **`... HOP` / `... DONE`**: Various commands like `NETMAP HOP` and `TOPOLOGY DONE` are used to pass discovery messages
  around the ring until they return to their origin.
