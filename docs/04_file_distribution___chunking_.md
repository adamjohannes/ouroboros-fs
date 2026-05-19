# Chapter 4: File Distribution & Chunking

In the [previous chapter](03_ring_topology___discovery_.md), we learned how OuroborosFS maps out its own network by passing a "sign-in sheet" around the ring. With this complete map, every [Node](02_node_.md) knows how many other nodes are in the network and how they are all connected.

Now, we can finally get to the core purpose of a file system: storing files! But in a distributed system, we can do this in a much smarter way than just dumping the whole file onto one computer.

## What Problem Does This Solve?

Imagine you have a very large file, like a high-resolution video. If you store this entire video on a single [Node](02_node_.md), you create two big problems:

1.  **Single Point of Failure:** If that one [Node](02_node_.md) goes offline, the entire video becomes unavailable.
2.  **Uneven Load:** That one [Node](02_node_.md) is doing all the work and using up all the storage space, while the others sit idle.

OuroborosFS solves this by splitting the file into smaller pieces and spreading them across the network.

**Our main use case: A user wants to upload a 10MB file called `vacation_video.mp4` to a three-node OuroborosFS ring. How does the system store this file without putting it all in one place?**

## The Book and Chapters Analogy

The strategy OuroborosFS uses is called **chunking**.

Instead of storing an entire file on one [Node](02_node_.md), we split it into smaller pieces called **chunks**. These chunks are then distributed sequentially around the ring, with each [Node](02_node_.md) storing one piece.

This is like splitting a large book into chapters and giving one chapter to each person at a round table.
*   **The Book:** The original file (`vacation_video.mp4`).
*   **The Chapters:** The chunks of the file.
*   **The People:** The Nodes in our ring.

This approach spreads the storage load and improves resilience. If one person leaves the table (a [Node](02_node_.md) goes down), we only lose one chapter, not the whole book. We'll see how we can recover from this in the next chapter on [Network Healing & Fault Tolerance](05_network_healing___fault_tolerance_.md).

## Storing a File: The `FILE PUSH` Command

Let's walk through how our `vacation_video.mp4` gets stored. The user will use a simple script to "push" the file to the network. This script connects to any [Node](02_node_.md) in the ring—let's call it Node A—and sends the `FILE PUSH` command.

**File:** `scripts/push_file.sh`
```bash
# Get the file size and name
SIZE_STR=$(wc --bytes < "${LOCAL_FILE}" | xargs)
FILE_NAME=$(basename "${LOCAL_FILE}")

# Send the command and the file data over the network
( printf "FILE PUSH ${SIZE_STR} ${FILE_NAME}\n"; cat "${LOCAL_FILE}" ) | nc ${HOST} ${PORT}
```
This script first creates a header like `FILE PUSH 10485760 vacation_video.mp4`, then immediately sends the raw file data after it. Now, let's see how the nodes handle this.

### A Diagram of the Distribution

Here's how the file data flows through our three-node ring (7001, 7002, 7003). Notice that there's
no chain — the **start node** (the one the client talked to) opens parallel connections to every
other chunk owner and pushes them their slice directly.

```mermaid
sequenceDiagram
    participant Client
    participant NodeA as Node 7001 (start)
    participant NodeB as Node 7002
    participant NodeC as Node 7003

    Client->>NodeA: FILE PUSH (10MB) + [Full File Data]
    Note over NodeA: parts=3. I'll keep chunk 1, fan out 2 and 3.
    par Fan-out
        NodeA->>NodeB: FILE PUSH-CHUNK part-002-of-003 + chunk 2 bytes
    and
        NodeA->>NodeC: FILE PUSH-CHUNK part-003-of-003 + chunk 3 bytes
    end
    NodeA->>NodeA: Save chunk 1 locally
    NodeB-->>NodeA: OK
    NodeC-->>NodeA: OK
    NodeA-->>Client: OK
```

The start node reads the client's bytes once, in order — chunk 1 lands on its own disk, chunk 2
streams straight to NodeB's open connection, chunk 3 to NodeC's. Backends save in parallel. If any
backend fails to ACK, the client sees an `ERR ...` rather than a half-saved file.

## Under the Hood: The Code for Chunking

Let's dive into the code that makes this elegant process work.

### 1. The Start Node: `handle_file_push`

When Node A receives the `FILE PUSH` command, its handler springs into action.

**File:** `src/server.rs`
```rust
async fn handle_file_push(/*...*/) -> Result<(), AnyErr> {
    // 1. Determine how many chunks to create.
    let parts: u32 = node.network_size().await as u32;

    // 2. Walk the topology snapshot to find each chunk's owner.
    //    Chunk 0 stays here; chunks 1..parts-1 go to subsequent nodes.
    let topology = node.topology_map.read().await.clone();
    let mut target_addrs = Vec::with_capacity(parts as usize - 1);
    /* ... walk topology starting from this node's port ... */

    // 3. Open all outbound connections in parallel and send PUSH-CHUNK headers.
    let mut conns = futures::future::try_join_all(
        target_addrs.iter().map(|addr| TcpStream::connect(addr))
    ).await?;
    for (i, s) in conns.iter_mut().enumerate() {
        let index = (i + 1) as u32;
        let chunk_size = fair_chunk_len(index, size, parts);
        let header = format!(
            "FILE PUSH-CHUNK {} {} {} {} {} {}\n",
            chunk_file_name(&name, index, parts),
            chunk_size, size, parts, index, start_port_num,
        );
        s.write_all(header.as_bytes()).await?;
    }

    // 4. Stream chunk 0 to disk locally; forward chunks 1..parts-1 to their conns.
    let len0 = fair_chunk_len(0, size, parts);
    let mut local = tokio::fs::File::create(&chunk0_path).await?;
    tokio::io::copy(&mut reader.take(len0), &mut local).await?;
    for (i, s) in conns.iter_mut().enumerate() {
        let chunk_size = fair_chunk_len((i + 1) as u32, size, parts);
        tokio::io::copy(&mut reader.take(chunk_size), s).await?;
    }

    // 5. Await OK from every backend.
    for s in conns.iter_mut() { /* read_line, check OK, time-bounded */ }
    // ... respond OK to the client ...
}
```
The crucial difference from a chain-relay design: bytes from the client flow through the start node
*once*, but the writes go to multiple destinations in parallel. The start node never buffers a whole
chunk — `tokio::io::copy` uses an 8 KiB internal buffer regardless of chunk size.

#### What is `fair_chunk_len`?

This helper function ensures the chunks are as evenly sized as possible. If a 10-byte file is split among 3 nodes, it can't be perfect. This function gives the first node 4 bytes, and the other two get 3 bytes each.

**File:** `src/server.rs`
```rust
fn fair_chunk_len(index: u32, total_size: u64, parts: u32) -> u64 {
    let base = total_size / parts as u64; // e.g., 10 / 3 = 3
    let rem = total_size % parts as u64;  // e.g., 10 % 3 = 1
    
    // The first `rem` chunks get one extra byte.
    if (index as u64) < rem { base + 1 } else { base }
}
```

### 2. Receiving a Chunk: `handle_file_push_chunk`

When Node B (or C, or any non-start node) receives a `FILE PUSH-CHUNK` command, its job is much
simpler than the start node's: it just streams the bytes to disk and acknowledges. There's no
forwarding — every chunk is sent directly by the start node.

**File:** `src/server.rs`
```rust
async fn handle_file_push_chunk(/*...*/) -> Result<(), AnyErr> {
    // 1. Tag the file locally so this node knows about it for FILE LIST and FILE PULL.
    let parent_name = name
        .rsplit_once(".part-")
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| name.clone());
    node.set_file_tag(&parent_name, start_port, file_size, parts).await;

    // 2. Stream exactly chunk_size bytes from the connection straight to disk.
    let path = node.storage_root.join(port_str(&node.port)).join("content").join(&name);
    let mut file = tokio::fs::File::create(&path).await?;
    tokio::io::copy(&mut reader.take(chunk_size), &mut file).await?;
    file.flush().await?;

    // 3. Spawn a fire-and-forget backup push to our predecessor (see Chapter 5).
    tokio::spawn(push_to_predecessor(Arc::clone(&node), name.clone()));

    // 4. ACK the start node.
    writer.write_all(b"OK\n").await?;
    Ok(())
}
```
The receiver never holds a chunk in a `Vec<u8>` — `tokio::io::copy` uses a fixed 8 KiB internal
buffer, so per-node memory during a push is bounded by that buffer plus whatever filesystem
write-back the OS holds. This matters when files are large: a 1 GB push across 5 nodes used to peak
at hundreds of MB resident on each relay; now relay nodes idle around 4 MB.

## Keeping Track: The `FileTag`

Storing the chunks is only half the story. The system also needs to remember how the file was split up so it can be reassembled later. For this, it creates a `FileTag`.

When Node A first receives the file, it creates a small record with three key pieces of information:

**File:** `src/node.rs`
```rust
pub struct FileTag {
    /// The port of the node where the first chunk is stored.
    pub start: u16,
    /// The total size of the original file.
    pub size: u64,
    /// The total number of chunks.
    pub parts: u32,
}
```
This `FileTag` is like a library card for the file. It tells any [Node](02_node_.md) everything it needs to know to find and reassemble `vacation_video.mp4`: "It starts at Node 7001, it's 10MB in total, and it's in 3 parts."

Each chunk receiver tags the file *locally* as part of `PUSH-CHUNK` reception (see step 1 of
`handle_file_push_chunk` above). There's no broadcast — fan-out reaches every chunk owner directly,
and each one writes its own `file_tags` entry. The tags are also re-pushed to a respawned node
during heal via `FILE TAGS-SET` (see [Chapter 5](05_network_healing___fault_tolerance_.md)).

## Conclusion

You've just learned the core magic behind OuroborosFS's storage strategy!

*   Instead of storing whole files, OuroborosFS breaks them into smaller **chunks**.
*   The number of chunks is determined by the number of **known nodes in the ring**.
*   The chunks are **pushed in parallel** from the start node directly to each chunk's owner — no chain.
*   A **`FileTag`** is created to serve as an index, recording where the file starts and how it was split.

This system is efficient and distributes the storage load perfectly. But what happens if a [Node](02_node_.md) holding a chunk suddenly crashes? How does the system handle that failure and prevent data loss?

In the next chapter, we'll explore the exciting mechanisms OuroborosFS uses to heal itself and tolerate faults.

➡️ **Next Chapter: [Network Healing & Fault Tolerance](05_network_healing___fault_tolerance_.md)**

---

Generated by [AI Codebase Knowledge Builder](https://github.com/The-Pocket/Tutorial-Codebase-Knowledge)