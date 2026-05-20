use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, path::PathBuf};
use tokio::fs;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, copy,
};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::process::Command;
use tokio::time::sleep;
use tracing;

use crate::{
    node::{self, FsyncMode, Node, append_edge, port_str},
    protocol::{self, validate_filename},
};

type AnyErr = Box<dyn Error + Send + Sync>;

/// Bind a node to `bind_addr`, create its on-disk storage tree, and return the
/// pieces a caller needs to wire it (the `Arc<Node>` and the resolved
/// `SocketAddr`) plus the `TcpListener` to feed to [`serve`].
///
/// Splitting the original `run()` lets the test harness:
///   1. bind every node (collecting OS-assigned ports when bound to `:0`),
///   2. wire the ring with `NODE NEXT` *before* any node starts accepting,
///   3. spawn `serve()` per node and abort that handle to "kill" a node.
pub async fn bind(
    bind_addr: &str,
    gossip_interval: Duration,
    file_size: u64,
    storage_root: PathBuf,
    respawn_dead: bool,
    fsync_mode: FsyncMode,
) -> Result<(Arc<Node>, TcpListener, std::net::SocketAddr), AnyErr> {
    let addr: std::net::SocketAddr = bind_addr.parse()?;

    let socket = if addr.is_ipv6() {
        TcpSocket::new_v6()?
    } else {
        TcpSocket::new_v4()?
    };

    socket.set_reuseaddr(true)?;
    #[cfg(unix)]
    socket.set_reuseport(true)?;

    socket.bind(addr)?;
    let listener = socket.listen(1024)?;
    let local = listener.local_addr()?;

    let node = Node::new(
        local.to_string(),
        gossip_interval,
        file_size,
        storage_root,
        respawn_dead,
        fsync_mode,
    );
    tracing::info!(node = %node.port, "Node listening");

    let port_only = port_str(&node.port);
    let content_dir = node.storage_root.join(port_only).join("content");
    let backup_dir = node.storage_root.join(port_only).join("backup");

    if let Err(e) = fs::create_dir_all(&content_dir).await {
        tracing::error!(node = %node.port, dir = %content_dir.display(), error = ?e, "Failed to create node content directory");
        return Err(e.into());
    }
    if let Err(e) = fs::create_dir_all(&backup_dir).await {
        tracing::error!(node = %node.port, dir = %backup_dir.display(), error = ?e, "Failed to create node backup directory");
        return Err(e.into());
    }

    if let Err(e) = sweep_orphan_partials(&content_dir).await {
        tracing::warn!(node = %node.port, dir = %content_dir.display(), error = ?e, "Janitor failed sweeping orphan partials in content/");
    }
    if let Err(e) = sweep_orphan_partials(&backup_dir).await {
        tracing::warn!(node = %node.port, dir = %backup_dir.display(), error = ?e, "Janitor failed sweeping orphan partials in backup/");
    }

    tracing::info!(node = %node.port, content_dir = %content_dir.display(), backup_dir = %backup_dir.display(), "Created node directories");

    Ok((node, listener, local))
}

/// Remove `*.partial` files from a chunk directory. These are leftovers from
/// a crash mid-write: `durably_write_chunk` writes to `<name>.partial` and
/// renames atomically to `<name>` only after a full `sync_all`. Anything that
/// kept its `.partial` suffix is junk by definition.
async fn sweep_orphan_partials(dir: &std::path::Path) -> std::io::Result<()> {
    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("partial") {
            tracing::warn!(file = %path.display(), "Removing orphan partial chunk");
            if let Err(e) = fs::remove_file(&path).await {
                tracing::warn!(file = %path.display(), error = ?e, "Failed to remove orphan partial");
            }
        }
    }
    Ok(())
}

/// Drive a bound node: spawn the gossip loop and run the accept loop forever.
/// Returns when the listener is dropped (e.g. the calling task is aborted).
pub async fn serve(node: Arc<Node>, listener: TcpListener) {
    if node.gossip_interval > Duration::from_millis(0) {
        let gossip_node = Arc::clone(&node);
        tokio::spawn(async move {
            tracing::info!(
                node = %gossip_node.port,
                interval = ?gossip_node.gossip_interval,
                "Gossip loop starting"
            );
            spawn_gossip_loop(gossip_node).await;
        });
    }

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(node = %node.port, error = ?e, "Accept failed; serve loop exiting");
                return;
            }
        };
        let node = Arc::clone(&node);
        let node_port = node.port.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_client(node, stream).await {
                tracing::error!(node = %node_port, peer = %peer, error = ?e, "Client connection error");
            }
        });
    }
}

/// Run a single ring node: bind, then serve forever. Used by the binary;
/// tests use [`bind`] + [`serve`] directly.
pub async fn run(
    bind_addr: &str,
    gossip_interval: Duration,
    file_size: u64,
    fsync_mode: FsyncMode,
) -> Result<(), AnyErr> {
    let (node, listener, _addr) = bind(
        bind_addr,
        gossip_interval,
        file_size,
        PathBuf::from("nodes"),
        true,
        fsync_mode,
    )
    .await?;
    serve(node, listener).await;
    Ok(())
}

async fn handle_client(node: Arc<Node>, stream: TcpStream) -> Result<(), AnyErr> {
    // Set read and write streams
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // The protocol is line delimited, so we just need to read the first line
    // when figuring out how to handle the request
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }

        // Parse the header and match it with a specific command
        match protocol::parse_line(&line) {
            Ok(cmd) => match cmd {
                // NODE
                protocol::Command::NodeNext(addr) => {
                    handle_node_next(&node, &mut writer, addr).await?
                }
                protocol::Command::NodeStatus => handle_node_status(&node, &mut writer).await?,
                protocol::Command::NodePing => handle_node_ping(&mut writer).await?,
                protocol::Command::NodeHeal => {
                    handle_node_heal(Arc::clone(&node), &mut writer).await?
                }
                protocol::Command::NodeHealHop { token, start_addr } => {
                    handle_node_heal_hop(Arc::clone(&node), &mut writer, token, start_addr).await?
                }
                protocol::Command::NodeHealDone { token } => {
                    handle_node_heal_done(&node, &mut writer, token).await?
                }

                // RING
                protocol::Command::RingForward { ttl, msg } => {
                    handle_ring_forward(&node, &mut writer, ttl, msg).await?
                }

                // TOPOLOGY
                protocol::Command::TopologyWalk => handle_topology_walk(&node, &mut writer).await?,
                protocol::Command::TopologyHop {
                    token,
                    start_addr,
                    history,
                } => handle_topology_hop(&node, &mut writer, token, start_addr, history).await?,
                protocol::Command::TopologyDone { token, history } => {
                    // Pass an owned Arc so it can be moved into the new task
                    handle_topology_done(Arc::clone(&node), &mut writer, token, history).await?
                }
                protocol::Command::TopologySet { history } => {
                    handle_topology_set(&node, &mut writer, history).await?
                }

                // NETMAP
                protocol::Command::NetmapDiscover => {
                    handle_netmap_discover(&node, &mut writer).await?
                }
                protocol::Command::NetmapHop {
                    token,
                    start_addr,
                    entries,
                } => handle_netmap_hop(&node, &mut writer, token, start_addr, entries).await?,
                protocol::Command::NetmapDone { token, entries } => {
                    handle_netmap_done(&node, &mut writer, token, entries).await?
                }
                protocol::Command::NetmapSet { entries } => {
                    handle_netmap_set(&node, &mut writer, entries).await?
                }
                protocol::Command::NetmapGet => handle_netmap_get(&node, &mut writer).await?,

                // FILE
                protocol::Command::FilePush { size, name } => {
                    handle_file_push(Arc::clone(&node), &mut reader, &mut writer, size, name)
                        .await?
                }
                protocol::Command::FilePull { name } => {
                    handle_file_pull(&node, &mut writer, name).await?;
                    break;
                }
                protocol::Command::FileList => {
                    handle_file_list_csv(&node, &mut writer).await?;
                    break;
                }
                protocol::Command::FileTagsSet { entries } => {
                    handle_file_tags_set(&node, &mut writer, entries).await?
                }

                // FILE (internal)
                protocol::Command::FilePushChunk {
                    name,
                    chunk_size,
                    file_size,
                    parts,
                    index,
                    start_port,
                } => {
                    handle_file_push_chunk(
                        Arc::clone(&node),
                        &mut reader,
                        &mut writer,
                        name,
                        chunk_size,
                        file_size,
                        parts,
                        index,
                        start_port,
                    )
                    .await?
                }
                protocol::Command::FileGetChunk { name } => {
                    handle_file_get_chunk(&node, &mut writer, name).await?
                }

                // FILE (backup)
                protocol::Command::FileBackupPush { name, size } => {
                    handle_file_backup_push(&node, &mut reader, &mut writer, name, size).await?
                }
                protocol::Command::FileGetBackupChunk { name } => {
                    handle_file_get_backup_chunk(&node, &mut writer, name).await?
                }
            },
            Err(e) => handle_error(&mut writer, e).await?,
        }
    }

    Ok(())
}

// --- Command handlers

async fn handle_node_next<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    addr: String,
) -> Result<(), AnyErr> {
    node.set_next(addr.clone()).await;
    writer
        .write_all(format!("OK next={}\n", addr).as_bytes())
        .await?;
    Ok(())
}

async fn handle_node_status<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
) -> Result<(), AnyErr> {
    let next = node
        .get_next()
        .await
        .unwrap_or_else(|| "<unset>".to_string());
    writer
        .write_all(format!("PORT {}\nNEXT {}\nOK\n", node.port, next).as_bytes())
        .await?;
    Ok(())
}

async fn handle_node_ping<W: AsyncWrite + Unpin>(writer: &mut W) -> Result<(), AnyErr> {
    writer.write_all(b"PONG\n").await?;
    Ok(())
}

/// Handles "NODE HEAL"
/// Starts a walk that forces every node to check and heal its neighbor.
async fn handle_node_heal<W: AsyncWrite + Unpin>(
    node: Arc<Node>,
    writer: &mut W,
) -> Result<(), AnyErr> {
    let token = node.make_walk_token();
    let rx = node.register_heal_walk(&token).await;

    // Spawn a task to do the first check and start the walk
    let start_addr = node.port.clone();
    let node_clone = Arc::clone(&node);
    tokio::spawn(async move {
        if let Err(e) = check_and_heal_neighbor(node_clone, &token, &start_addr).await {
            tracing::error!(
                node = %start_addr,
                token = %token,
                error = ?e,
                "Heal walk: First check failed"
            );
        }
    });

    // Wait for the walk to complete (or time out)
    let walk_timeout = Duration::from_secs(60);
    match tokio::time::timeout(walk_timeout, rx).await {
        Ok(Ok(())) => {
            writer.write_all(b"OK network healed\n").await?;
        }
        Ok(Err(_)) => {
            writer.write_all(b"ERR heal walk canceled\n").await?;
        }
        Err(_) => {
            writer.write_all(b"ERR heal walk timed out\n").await?;
        }
    }

    Ok(())
}

/// Handles "NODE HEAL-HOP <token> <start_addr>"
/// This is received by a node, which then checks its neighbor.
async fn handle_node_heal_hop<W: AsyncWrite + Unpin>(
    node: Arc<Node>,
    writer: &mut W,
    token: String,
    start_addr: String,
) -> Result<(), AnyErr> {
    // 1. ACK the hop request immediately
    writer.write_all(b"OK\n").await?;

    // 2. Spawn a task to do the actual work
    tokio::spawn(async move {
        let node_port = node.port.clone();
        if let Err(e) = check_and_heal_neighbor(node, &token, &start_addr).await {
            tracing::error!(
                node = %node_port,
                token = %token,
                error = ?e,
                "Heal walk: Check/forward failed"
            );
        }
    });

    Ok(())
}

/// Handles "NODE HEAL-DONE <token>"
/// This is received by the start node when the walk is complete.
async fn handle_node_heal_done<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    token: String,
) -> Result<(), AnyErr> {
    // Signal the original "handle_node_heal" waiter
    node.finish_heal_walk(&token).await;
    writer.write_all(b"OK\n").await?;
    Ok(())
}

/// Logic for one step of the heal walk.
/// 1. Get neighbor.
/// 2. Check if neighbor is start. If so, send HEAL-DONE.
/// 3. If not, ping neighbor.
/// 4. If ping OK, forward HEAL-HOP.
/// 5. If ping FAIL, run `handle_node_death`, then forward HEAL-HOP.
async fn check_and_heal_neighbor(
    node: Arc<Node>,
    token: &str,
    start_addr: &str,
) -> Result<(), AnyErr> {
    let Some(next_addr) = node.get_next().await else {
        tracing::warn!(node = %node.port, "Heal walk: No next node set, stopping walk.");
        return Ok(()); // Stop the walk
    };

    // 1. Check if the ring was completed
    if port_str(&next_addr) == port_str(start_addr) {
        tracing::info!(node = %node.port, token = %token, "Heal walk: Completed ring, sending DONE.");
        let mut s = TcpStream::connect(start_addr).await?;
        s.write_all(format!("NODE HEAL-DONE {}\n", token).as_bytes())
            .await?;
        return Ok(());
    }

    // 2. Node is not the start, so check its health
    match check_node_health(node.clone(), &next_addr).await {
        Ok(_) => {
            // 3. Node is ALIVE -> Forward the HEAL-HOP request
            tracing::debug!(node = %node.port, target = %next_addr, "Heal walk: Node is alive, forwarding hop.");
            let mut s = TcpStream::connect(&next_addr).await?;
            s.write_all(format!("NODE HEAL-HOP {} {}\n", token, start_addr).as_bytes())
                .await?;
        }
        Err(e) => {
            // 3. Node is DEAD -> Heal it, then forward
            tracing::warn!(
                node = %node.port,
                target = %next_addr,
                error = ?e,
                "Heal walk: Node is dead, starting healing process."
            );

            // This blocks until the node is respawned and synced
            if let Err(heal_err) = handle_node_death(node.clone(), next_addr.clone()).await {
                tracing::error!(
                    node = %node.port,
                    target = %next_addr,
                    error = ?heal_err,
                    "Heal walk: `handle_node_death` failed. Stopping walk."
                );
                return Err(heal_err); // Stop the walk
            }

            // 4. Forward the HEAL-HOP to the newly respawned node
            tracing::info!(
                node = %node.port,
                target = %next_addr,
                "Heal walk: Node healed, forwarding hop."
            );
            let mut s = TcpStream::connect(&next_addr).await?;
            s.write_all(format!("NODE HEAL-HOP {} {}\n", token, start_addr).as_bytes())
                .await?;
        }
    }

    Ok(())
}

async fn handle_ring_forward<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    mut ttl: u32,
    msg: String,
) -> Result<(), AnyErr> {
    tracing::debug!(node = %node.port, ttl, msg = %msg, "RING FORWARD");

    if ttl > 0 {
        ttl -= 1;
        if let Some(next_addr) = node.get_next().await {
            if let Err(e) = node.forward_ring_forward(ttl, &msg).await {
                tracing::warn!(node = %node.port, target = %next_addr, error = ?e, "RING FORWARD failed");
            }
        } else {
            tracing::warn!(node = %node.port, "No next node set, dropping RING FORWARD");
        }
    }

    writer.write_all(b"OK\n").await?;
    Ok(())
}

/// Handle "TOPOLOGY WALK" from the client on the start node.
async fn handle_topology_walk<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
) -> Result<(), AnyErr> {
    let token = node.make_walk_token();
    let rx = node.register_walk(token.as_str()).await;

    let Some(history) = node.first_walk_history().await else {
        writer.write_all(b"ERR no next hop set\n").await?;
        return Ok(());
    };

    if let Err(e) = node
        .forward_topology_hop(&token, &node.port, &history)
        .await
    {
        writer
            .write_all(format!("ERR forward failed: {e}\n").as_bytes())
            .await?;
        return Ok(());
    }

    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(final_history)) => {
            for seg in final_history.split(';').filter(|s| !s.is_empty()) {
                writer.write_all(format!("{seg}\n").as_bytes()).await?;
            }
            writer.write_all(b"OK\n").await?;
        }
        Ok(Err(_)) => {
            writer.write_all(b"ERR walk canceled\n").await?;
        }
        Err(_) => {
            writer.write_all(b"ERR walk timeout\n").await?;
        }
    }

    Ok(())
}

async fn handle_topology_hop<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    token: String,
    start_addr: String,
    history: String,
) -> Result<(), AnyErr> {
    let Some(next_addr) = node.get_next().await else {
        let _ = writer.write_all(b"OK\n").await;
        return Ok(());
    };

    let new_history = append_edge(history, &node.port, &next_addr);

    if port_str(&next_addr) == port_str(&start_addr) {
        if let Err(e) = node
            .send_topology_done(&start_addr, &token, &new_history)
            .await
        {
            tracing::warn!(
                node = %node.port,
                target = %start_addr,
                error = ?e,
                "TOPOLOGY DONE send failed"
            );
        }
    } else {
        if let Err(e) = node
            .forward_topology_hop(&token, &start_addr, &new_history)
            .await
        {
            tracing::warn!(
                node = %node.port,
                target = %next_addr,
                error = ?e,
                "TOPOLOGY HOP forward failed"
            );
        }
    }

    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

async fn handle_topology_done<W: AsyncWrite + Unpin>(
    node: Arc<Node>,
    writer: &mut W,
    token: String,
    history: String,
) -> Result<(), AnyErr> {
    // Finish the client walk if we are the start node
    let _ = node.finish_walk(&token, history.clone()).await;

    // Persist and broadcast the completed topology
    node.set_topology_from_history(&history).await;

    let node_clone = Arc::clone(&node);
    tokio::spawn(async move {
        node_clone.broadcast_topology_set().await;
    });

    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

async fn handle_topology_set<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    history: String,
) -> Result<(), AnyErr> {
    node.set_topology_from_history(&history).await;
    writer.write_all(b"OK\n").await?;
    Ok(())
}

// --- NETMAP

async fn handle_netmap_discover<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
) -> Result<(), AnyErr> {
    let token = node.make_invest_token();

    let Some(_next) = node.get_next().await else {
        writer.write_all(b"ERR no next hop set\n").await?;
        return Ok(());
    };

    // entries begins with "<node_port>=Alive"
    let entries = format!("{}=Alive", port_str(&node.port));
    if let Err(e) = node.forward_netmap_hop(&token, &node.port, &entries).await {
        writer
            .write_all(format!("ERR forward failed: {e}\n").as_bytes())
            .await?;
        return Ok(());
    }

    // We don't need to wait here; it's a background ring discovery.
    writer.write_all(b"OK\n").await?;
    Ok(())
}

async fn handle_netmap_hop<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    token: String,
    start_addr: String,
    entries: String,
) -> Result<(), AnyErr> {
    let Some(next_addr) = node.get_next().await else {
        let _ = writer.write_all(b"OK\n").await;
        return Ok(());
    };

    let new_entries = node.entries_with_self(&entries);

    if port_str(&next_addr) == port_str(&start_addr) {
        if let Err(e) = node
            .send_netmap_done(&start_addr, &token, &new_entries)
            .await
        {
            tracing::warn!(
                node = %node.port,
                target = %start_addr,
                error = ?e,
                "NETMAP DONE send failed"
            );
        }
    } else {
        if let Err(e) = node
            .forward_netmap_hop(&token, &start_addr, &new_entries)
            .await
        {
            tracing::warn!(
                node = %node.port,
                target = %next_addr,
                error = ?e,
                "NETMAP HOP forward failed"
            );
        }
    }

    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

async fn handle_netmap_done<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    _token: String,
    entries: String,
) -> Result<(), AnyErr> {
    // Persist locally, then broadcast to all nodes
    node.set_network_nodes_from_entries(&entries).await;
    node.broadcast_netmap(&entries).await;

    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

async fn handle_netmap_set<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    entries: String,
) -> Result<(), AnyErr> {
    node.set_network_nodes_from_entries(&entries).await;
    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

async fn handle_netmap_get<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
) -> Result<(), AnyErr> {
    let lines = node.get_network_nodes_lines().await;
    if lines.is_empty() {
        writer.write_all(b"(empty)\n").await?;
    } else {
        for l in lines {
            writer.write_all(format!("{l}\n").as_bytes()).await?;
        }
    }
    writer.write_all(b"OK\n").await?;
    Ok(())
}

// --- FILE CHUNKING helpers

/// Length of the SHA-256 trailer appended to every saved chunk.
const CHUNK_TRAILER_LEN: u64 = 32;

/// Write a chunk's bytes to `<dir>/<final_name>` with crash safety and
/// per-chunk integrity checking.
///
/// On-disk layout: `<body bytes> || sha256(body)` (32 trailing bytes).
/// The body length is `metadata.len() - CHUNK_TRAILER_LEN`; readers split
/// the trailer back off and verify before serving (see `open_chunk_verified`).
///
/// Crash safety:
/// 1. Copy exactly `size` bytes from `reader` into `<dir>/<final_name>.partial`,
///    hashing in-stream.
/// 2. Append the 32-byte hash trailer.
/// 3. `sync_all()` the file if `mode >= Data`.
/// 4. `rename(2)` to `<dir>/<final_name>`. POSIX rename is atomic within a
///    directory, so a crash either leaves the `.partial` (cleaned by the
///    startup janitor) or leaves the final file intact.
/// 5. `sync_all()` the directory file descriptor if `mode == Full`, so the
///    directory entry survives a power loss between rename and OS writeback.
///
/// Returns the body length actually copied (matches `size` on success).
async fn durably_write_chunk<R: AsyncRead + Unpin>(
    dir: &std::path::Path,
    final_name: &str,
    reader: &mut R,
    size: u64,
    mode: FsyncMode,
) -> std::io::Result<u64> {
    use sha2::{Digest, Sha256};

    let final_path = dir.join(final_name);
    let partial_path = dir.join(format!("{final_name}.partial"));

    let mut written = 0u64;
    {
        let mut f = tokio::fs::File::create(&partial_path).await?;
        let mut hasher = Sha256::new();
        if size > 0 {
            // Stream in 64 KiB blocks; hash + write each block in lockstep so
            // we never hold more than the block size in RAM.
            let mut buf = vec![0u8; 64 * 1024];
            let mut remaining = size;
            while remaining > 0 {
                let want = remaining.min(buf.len() as u64) as usize;
                let n = reader.read(&mut buf[..want]).await?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "short read on chunk body",
                    ));
                }
                hasher.update(&buf[..n]);
                f.write_all(&buf[..n]).await?;
                remaining -= n as u64;
                written += n as u64;
            }
        }
        let digest = hasher.finalize();
        f.write_all(&digest).await?;
        f.flush().await?;
        if mode.syncs_file() {
            f.sync_all().await?;
        }
    }
    fs::rename(&partial_path, &final_path).await?;
    if mode.syncs_dir() {
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            // Open the directory and fsync it. The std handle's `sync_all`
            // works on directories on Unix; on non-Unix it's a no-op,
            // which matches the documented platform support.
            let f = std::fs::File::open(&dir)?;
            f.sync_all()
        })
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))??;
    }
    Ok(written)
}

/// Open a chunk, read it in full, verify its SHA-256 trailer, and return the
/// body bytes (without the trailer).
///
/// Returns `Err` on missing-file (`NotFound`), short file (anything smaller
/// than the trailer length is corrupt by definition), or hash mismatch
/// (`InvalidData`). Callers decide whether to fall through to a backup.
async fn open_chunk_verified(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use sha2::{Digest, Sha256};

    let bytes = tokio::fs::read(path).await?;
    if (bytes.len() as u64) < CHUNK_TRAILER_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "chunk shorter than trailer",
        ));
    }
    let split = bytes.len() - CHUNK_TRAILER_LEN as usize;
    let (body, trailer) = bytes.split_at(split);
    let mut hasher = Sha256::new();
    hasher.update(body);
    let actual = hasher.finalize();
    if actual.as_slice() != trailer {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "chunk hash mismatch",
        ));
    }
    Ok(body.to_vec())
}

fn fair_chunk_len(index: u32, total_size: u64, parts: u32) -> u64 {
    // Distribute remainder to the first (total_size % parts) chunks
    let base = total_size / parts as u64;
    let rem = total_size % parts as u64;
    if (index as u64) < rem { base + 1 } else { base }
}

fn chunk_file_name(name: &str, index: u32, parts: u32) -> String {
    debug_assert!(validate_filename(name).is_ok(), "unvalidated name {name:?}");
    format!("{}.part-{:03}-of-{:03}", name, index + 1, parts)
}

// --- FILE: PUSH (fan-out) handlers

/// Start node for a `FILE PUSH`. Splits the upload across the ring by
/// fanning out concurrent `FILE PUSH-CHUNK` connections — one per non-start
/// chunk owner — instead of relaying through a serial chain.
///
/// The client byte-stream is inherently sequential (one TCP) so we read
/// chunk i, save/forward, then read chunk i+1. The parallelism is in the
/// per-target write-out + ACK-await: each target chunk has its own open
/// TCP we write to in turn, and we await all `OK`s concurrently at the
/// end. This means latency drops from O(N × hop) to O(longest hop) and
/// failures are observed by the start node directly. The fan-out refactor
/// also retired the dead `pending_files`/`finish_file` machinery on `Node`;
/// ACK tracking now happens via the open outbound TCP connections instead.
async fn handle_file_push<R, W>(
    node: Arc<Node>,
    reader: &mut R,
    writer: &mut W,
    size: u64,
    name: String,
) -> Result<(), AnyErr>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Reject oversized files; bounded drain so a malicious client can't OOM
    // us by declaring `u64::MAX`.
    if size > node.file_size {
        tracing::error!(node = %node.port, file_name = %name, file_size = size, max_file_size = %node.file_size, "File size is too large");
        let msg = format!(
            "ERR File size is too large ({} > {})\n",
            size, node.file_size
        );
        writer.write_all(msg.as_bytes()).await?;
        let mut limited = (&mut *reader).take(size);
        tokio::io::copy(&mut limited, &mut tokio::io::sink()).await?;
        return Ok(());
    }

    debug_assert!(validate_filename(&name).is_ok(), "unvalidated name {name:?}");

    let parts: u32 = node.network_size().await as u32;
    let start_port_num: u16 = port_str(&node.port).parse().unwrap_or(0);
    node.set_file_tag(&name, start_port_num, size, parts).await;

    // Single-node ring: save chunk 0 locally and we're done. No fan-out
    // needed; this also handles the parts==1 short-circuit the test
    // `fanout_push_parts_eq_one` pins.
    if parts == 1 {
        let chunk_name = chunk_file_name(&name, 0, parts);
        let dir = node
            .storage_root
            .join(port_str(&node.port))
            .join("content");
        durably_write_chunk(&dir, &chunk_name, &mut *reader, size, node.fsync_mode).await?;

        let node_clone = Arc::clone(&node);
        let cn = chunk_name.clone();
        tokio::spawn(async move {
            push_to_predecessor(node_clone, cn).await;
        });

        writer
            .write_all(format!("FILE {} bytes '{}' stored locally\nOK", size, name).as_bytes())
            .await?;
        return Ok(());
    }

    // Walk the topology snapshot to find chunk i's owner address. Chunk 0
    // is us; chunks 1..parts-1 each go to the next hop in turn. The
    // topology map stores `port -> next_port` already keyed on bare ports.
    let topology: std::collections::HashMap<String, String> =
        node.topology_map.read().await.clone();
    let host = host_of(&node.port).to_string();
    let mut target_addrs: Vec<String> = Vec::with_capacity(parts as usize - 1);
    {
        let mut current = port_str(&node.port).to_string();
        for _ in 1..parts {
            let Some(next) = topology.get(&current).cloned() else {
                writer.write_all(b"ERR topology incomplete; cannot fan out\n").await?;
                let mut limited = (&mut *reader).take(size);
                tokio::io::copy(&mut limited, &mut tokio::io::sink()).await?;
                return Ok(());
            };
            target_addrs.push(format!("{}:{}", host, next));
            current = next;
        }
    }

    // Open all outbound connections in parallel (one per non-start chunk).
    // If any connect fails we abort the whole push: the client gets ERR
    // and drains. Future work could fall back to a relay; not in scope.
    let connect_futures = target_addrs.iter().map(|addr| {
        let addr = addr.clone();
        async move { TcpStream::connect(&addr).await.map(|s| (addr, s)) }
    });
    let mut conns: Vec<(String, TcpStream)> = match futures::future::try_join_all(connect_futures).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(node = %node.port, error = ?e, "Fan-out connect failed");
            writer.write_all(b"ERR fan-out connect failed\n").await?;
            let mut limited = (&mut *reader).take(size);
            tokio::io::copy(&mut limited, &mut tokio::io::sink()).await?;
            return Ok(());
        }
    };

    // Send a PUSH-CHUNK header per outbound connection up front so the
    // receivers know how many bytes to expect. Chunk lengths come from
    // fair_chunk_len; index i+1 lives on conns[i].
    for (i, (_addr, s)) in conns.iter_mut().enumerate() {
        let index = (i + 1) as u32;
        let chunk_size = fair_chunk_len(index, size, parts);
        let chunk_name = chunk_file_name(&name, index, parts);
        let header = format!(
            "FILE PUSH-CHUNK {} {} {} {} {} {}\n",
            chunk_name, chunk_size, size, parts, index, start_port_num
        );
        if let Err(e) = s.write_all(header.as_bytes()).await {
            tracing::error!(node = %node.port, error = ?e, "Failed to send PUSH-CHUNK header");
            writer.write_all(b"ERR fan-out header write failed\n").await?;
            let mut limited = (&mut *reader).take(size);
            tokio::io::copy(&mut limited, &mut tokio::io::sink()).await?;
            return Ok(());
        }
    }

    // Stream the client byte stream chunk by chunk. Chunk 0 lands on disk
    // here; chunks 1..parts-1 stream straight to the corresponding
    // outbound conn (zero buffering on the start node).
    let chunk0_name = chunk_file_name(&name, 0, parts);
    let content_dir = node
        .storage_root
        .join(port_str(&node.port))
        .join("content");
    let len0 = fair_chunk_len(0, size, parts);
    durably_write_chunk(&content_dir, &chunk0_name, &mut *reader, len0, node.fsync_mode).await?;

    // Backup chunk 0 to predecessor.
    let node_clone = Arc::clone(&node);
    let cn0 = chunk0_name.clone();
    tokio::spawn(async move {
        push_to_predecessor(node_clone, cn0).await;
    });

    for (i, (_addr, s)) in conns.iter_mut().enumerate() {
        let index = (i + 1) as u32;
        let chunk_size = fair_chunk_len(index, size, parts);
        if chunk_size == 0 {
            continue;
        }
        let mut limited = (&mut *reader).take(chunk_size);
        if let Err(e) = copy(&mut limited, s).await {
            tracing::error!(node = %node.port, error = ?e, "Failed streaming chunk to fan-out target");
            writer.write_all(b"ERR fan-out stream failed\n").await?;
            return Ok(());
        }
    }

    // Now wait for OK from every backend, with a per-push deadline.
    use tokio::io::AsyncBufReadExt;
    let ack_timeout = std::time::Duration::from_secs(30);
    for (addr, s) in conns.iter_mut() {
        let mut br = tokio::io::BufReader::new(s);
        let mut line = String::new();
        match tokio::time::timeout(ack_timeout, br.read_line(&mut line)).await {
            Ok(Ok(0)) | Err(_) => {
                tracing::error!(node = %node.port, target = %addr, "PUSH-CHUNK ACK timed out or EOF");
                writer.write_all(b"ERR fan-out ack timeout\n").await?;
                return Ok(());
            }
            Ok(Err(e)) => {
                tracing::error!(node = %node.port, target = %addr, error = ?e, "PUSH-CHUNK ACK io error");
                writer.write_all(b"ERR fan-out ack io\n").await?;
                return Ok(());
            }
            Ok(Ok(_)) => {
                if !line.trim_start().starts_with("OK") {
                    tracing::error!(node = %node.port, target = %addr, response = %line.trim(), "PUSH-CHUNK target returned non-OK");
                    writer.write_all(b"ERR fan-out non-ok\n").await?;
                    return Ok(());
                }
            }
        }
    }

    writer
        .write_all(
            format!(
                "FILE {} bytes split into {} chunks and distributed\nOK\n",
                size, parts
            )
            .as_bytes(),
        )
        .await?;
    Ok(())
}

/// Receiver side of fan-out PUSH. Streams `chunk_size` bytes from the
/// connection straight to disk, tags the file, kicks off backup-push,
/// replies `OK\n`. Replaces the older RELAY-STREAM hop handler.
async fn handle_file_push_chunk<R, W>(
    node: Arc<Node>,
    reader: &mut R,
    writer: &mut W,
    name: String,
    chunk_size: u64,
    file_size: u64,
    parts: u32,
    index: u32,
    start_port: u16,
) -> Result<(), AnyErr>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if index >= parts {
        writer.write_all(b"ERR bad FILE PUSH-CHUNK index\n").await?;
        return Ok(());
    }

    // The chunk name embeds index/parts; reconstruct the parent file name
    // from the chunk name so the file_tags entry uses the right key.
    // Convention: chunk_file_name(name, index, parts) =
    // "<name>.part-XXX-of-YYY". The receiver's `name` argument is already
    // the full chunk name; recover the parent.
    let parent_name = name
        .rsplit_once(".part-")
        .map(|(p, _)| p.to_string())
        .unwrap_or_else(|| name.clone());
    node.set_file_tag(&parent_name, start_port, file_size, parts)
        .await;

    let dir = node
        .storage_root
        .join(port_str(&node.port))
        .join("content");
    durably_write_chunk(&dir, &name, reader, chunk_size, node.fsync_mode).await?;

    tracing::info!(
        node = %node.port,
        chunk = index + 1,
        parts,
        file = %dir.join(&name).display(),
        bytes = chunk_size,
        "Stored fan-out chunk"
    );

    // Backup push to predecessor (push-based replication).
    let node_clone = Arc::clone(&node);
    let cn = name.clone();
    tokio::spawn(async move {
        push_to_predecessor(node_clone, cn).await;
    });

    writer.write_all(b"OK\n").await?;
    Ok(())
}

async fn handle_file_tags_set<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    entries: String,
) -> Result<(), AnyErr> {
    node.set_file_tags_from_entries(&entries).await;
    writer.write_all(b"OK\n").await?;
    Ok(())
}

// --- FILE RETRIEVAL (PULL / GET-CHUNK)

async fn handle_file_pull<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    name: String,
) -> Result<(), AnyErr> {
    let tags = node.file_tags.read().await;
    let Some(tag) = tags.get(&name) else {
        writer.write_all(b"ERR file not found\n").await?;
        return Ok(());
    };
    let start_port = tag.start;
    let parts = tag.parts;
    let file_size = tag.size;
    let start_addr = format!("{}:{}", host_of(&node.port), start_port);
    drop(tags);

    // Stream each chunk straight to the client (no full-file Vec).
    pull_file_from_ring(node, &name, &start_addr, parts, file_size, writer).await
}

async fn handle_file_get_chunk<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    name: String,
) -> Result<(), AnyErr> {
    let next = node.get_next().await.unwrap_or_else(|| node.port.clone());

    let chunk_path = node
        .storage_root
        .join(port_str(&node.port))
        .join("content")
        .join(&name);

    // Read the chunk in full and verify the SHA-256 trailer before announcing
    // any bytes to the puller.
    //
    // Missing file → announce size=0 (the long-standing convention; some
    // PULL paths legitimately receive a missing-chunk response, e.g. during
    // a heal window).
    //
    // Corrupt file → return Err so `handle_client` drops the connection.
    // The puller's `request_chunk_from` returns Err on the unexpected EOF,
    // which triggers the predecessor-backup fall-through path. We can't use
    // size=0 here: that's ambiguous with "this chunk is legitimately empty,"
    // which happens for small files where `fair_chunk_len(i) == 0`, and
    // the puller treats size=0 as success (no fall-through).
    let body = match open_chunk_verified(&chunk_path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            writer
                .write_all(format!("FILE RESP-CHUNK {} 0 {}\n", next, name).as_bytes())
                .await?;
            return Ok(());
        }
        Err(e) => {
            tracing::error!(node = %node.port, chunk = %name, error = ?e, "Chunk failed integrity check; dropping connection so the puller falls through to backup.");
            return Err(format!("chunk {name} failed integrity check: {e}").into());
        }
    };
    writer
        .write_all(format!("FILE RESP-CHUNK {} {} {}\n", next, body.len(), name).as_bytes())
        .await?;
    if !body.is_empty() {
        writer.write_all(&body).await?;
    }
    Ok(())
}

// --- BACKUP HANDLERS

/// Handles "FILE BACKUP-PUSH <name> <size>" + raw bytes.
///
/// Replaces the older notify-then-pull dance. The saving node opens
/// one connection to its predecessor, ships the chunk in a single round
/// trip, gets `OK`. Bytes stream straight from the wire to disk — no
/// `Vec<u8>` of size = chunk_size.
async fn handle_file_backup_push<R, W>(
    node: &Node,
    reader: &mut R,
    writer: &mut W,
    name: String,
    size: u64,
) -> Result<(), AnyErr>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let dir = node
        .storage_root
        .join(port_str(&node.port))
        .join("backup");
    fs::create_dir_all(&dir).await.ok();
    durably_write_chunk(&dir, &name, reader, size, node.fsync_mode).await?;

    tracing::info!(
        node = %node.port,
        chunk = %name,
        bytes = size,
        path = %dir.join(&name).display(),
        "Backup chunk received and stored (push)."
    );

    writer.write_all(b"OK\n").await?;
    Ok(())
}

/// Handles "FILE GET-BACKUP-CHUNK <name>"
/// This is used by the PULL failover process. It reads from the "/backup" dir
/// and returns a standard FILE RESP-CHUNK.
async fn handle_file_get_backup_chunk<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    name: String,
) -> Result<(), AnyErr> {
    let next = node.get_next().await.unwrap_or_else(|| node.port.clone());

    let chunk_path = node
        .storage_root
        .join(port_str(&node.port))
        .join("backup")
        .join(&name);

    // Same read-and-verify pattern as GET-CHUNK, against /backup. Missing
    // → size=0; corrupt → return Err so `handle_client` drops the
    // connection (no further fall-through is available — the file is
    // permanently lost, but at least we don't return body bytes whose
    // trailer didn't verify).
    let body = match open_chunk_verified(&chunk_path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            writer
                .write_all(format!("FILE RESP-CHUNK {} 0 {}\n", next, name).as_bytes())
                .await?;
            return Ok(());
        }
        Err(e) => {
            tracing::error!(node = %node.port, chunk = %name, error = ?e, "Backup chunk failed integrity check; no further fall-through available.");
            return Err(format!("backup chunk {name} failed integrity check: {e}").into());
        }
    };
    writer
        .write_all(format!("FILE RESP-CHUNK {} {} {}\n", next, body.len(), name).as_bytes())
        .await?;
    if !body.is_empty() {
        writer.write_all(&body).await?;
    }
    Ok(())
}

// --- PULL helpers

async fn pull_file_from_ring<W: AsyncWrite + Unpin>(
    node: &Node,
    name: &str,
    start_addr: &str,
    parts: u32,
    _file_size: u64,
    writer: &mut W,
) -> Result<(), AnyErr> {
    use futures::stream::{FuturesOrdered, StreamExt};
    use std::collections::{HashMap, HashSet};
    use tokio::sync::Semaphore;

    let host = host_of(start_addr).to_string();

    // Snapshot the topology once. Holding the read guard across the per-chunk
    // network I/O blocked any heal/discover broadcast that wanted to write
    // (deadlocking concurrent NETMAP DISCOVER while a long pull was in
    // flight). The snapshot can become stale if a node dies mid-pull, but
    // that already fell through to the backup path; same outcome, fewer
    // contention surprises.
    let topology: HashMap<String, String> = node.topology_map.read().await.clone();

    // Reverse map: dead_port -> predecessor_port. O(1) lookup replaces the
    // O(N) scan that ran per failed chunk.
    let predecessor_of: HashMap<String, String> = topology
        .iter()
        .map(|(from, to)| (port_str(to).to_string(), from.clone()))
        .collect();

    // 1. Build a fetch plan by walking the topology snapshot once.
    //    Chunk i lives on the node at hop i from the start.
    let mut plan: Vec<(u32, String, String, String)> = Vec::with_capacity(parts as usize);
    {
        let mut current_addr = start_addr.to_string();
        let mut current_port = port_str(start_addr).to_string();
        for i in 0..parts {
            plan.push((
                i,
                chunk_file_name(name, i, parts),
                current_addr.clone(),
                current_port.clone(),
            ));
            // Advance to the next node, even on the last iteration we don't
            // use it. If the topology is broken mid-walk we stop early —
            // missing entries become "no chunks" (short output; see the
            // streaming-pull truncation contract pinned in tests).
            let Some(next) = topology.get(&current_port).cloned() else {
                tracing::error!(
                    node = %node.port,
                    from_node = %current_port,
                    "Topology snapshot incomplete; pull plan truncated."
                );
                break;
            };
            current_port = next.clone();
            current_addr = format!("{}:{}", host, next);
        }
    }

    // 2. Spawn fetches concurrently with a small in-flight cap. Throttling
    //    keeps queued-but-not-yet-written chunks bounded if the client
    //    reads slowly: at most CAP * largest_chunk bytes in RAM.
    const FETCH_CONCURRENCY: usize = 4;
    let sem = Arc::new(Semaphore::new(FETCH_CONCURRENCY));

    let mut tasks: FuturesOrdered<_> = plan
        .into_iter()
        .map(|(i, chunk_name, owner_addr, owner_port)| {
            let sem = Arc::clone(&sem);
            async move {
                // Acquire-on-spawn would defeat ordered pipelining; acquire
                // inside the task so FuturesOrdered can buffer up to CAP
                // pending tasks while the writer drains in order.
                let _permit = sem.acquire_owned().await.expect("semaphore closed");
                let r = request_chunk_from(&owner_addr, &chunk_name).await;
                (i, chunk_name, owner_addr, owner_port, r)
            }
        })
        .collect();

    // 3. Drain in chunk-index order, streaming each result to the client.
    //    Failed chunks are recovered synchronously from the predecessor's
    //    backup; failed ports are batched and broadcast once at the end so
    //    we don't emit N "Broadcasting node status" lines for one dead host.
    let mut failed_ports: HashSet<String> = HashSet::new();

    while let Some((_i, chunk_name, owner_addr, owner_port, r)) = tasks.next().await {
        let chunk: Vec<u8> = match r {
            Ok((chunk_data, _next_addr_ignored)) => {
                tracing::debug!(
                    node = %node.port,
                    from = %owner_addr,
                    chunk_name = %chunk_name,
                    "Got chunk successfully."
                );
                chunk_data
            }
            Err(e) => {
                tracing::warn!(
                    node = %node.port,
                    target_node = %owner_addr,
                    chunk_name = %chunk_name,
                    error = ?e,
                    "Failed to get chunk from node. Attempting to use backup."
                );
                failed_ports.insert(owner_port.clone());

                // Find predecessor (backup holder) and try the backup path.
                let Some(pred_port) = predecessor_of.get(&owner_port).cloned() else {
                    tracing::error!(
                        node = %node.port,
                        dead_node = %owner_addr,
                        chunk_name = %chunk_name,
                        "No predecessor found in topology for dead node. \
                         Output will be SHORT (chunk omitted, no zero-padding)."
                    );
                    continue;
                };
                let pred_addr = format!("{}:{}", host, pred_port);
                match request_backup_chunk_from(&pred_addr, &chunk_name).await {
                    Ok((chunk_data, _)) => {
                        tracing::info!(
                            node = %node.port,
                            from_backup_node = %pred_addr,
                            chunk_name = %chunk_name,
                            "Successfully retrieved chunk from backup."
                        );
                        chunk_data
                    }
                    Err(e_backup) => {
                        tracing::error!(
                            node = %node.port,
                            backup_node = %pred_addr,
                            chunk_name = %chunk_name,
                            error = ?e_backup,
                            "Failed to get chunk from backup node. \
                             Output will be SHORT (chunk omitted, no zero-padding)."
                        );
                        Vec::new()
                    }
                }
            }
        };

        if !chunk.is_empty() {
            writer.write_all(&chunk).await?;
        }
    }

    // 4. Single batch netmap update + broadcast for the dead set, deferred
    //    until after the pull completes so we emit at most one
    //    "Broadcasting node status" line per dead host (not per failed
    //    chunk). This is the regression that `failover_no_double_broadcast`
    //    pins.
    if !failed_ports.is_empty() {
        for port in &failed_ports {
            tracing::info!(
                node = %node.port,
                dead_node = %port,
                "Marking node as Dead and broadcasting netmap update."
            );
            node.update_node_status(port.clone(), crate::NodeStatus::Dead)
                .await;
        }
        node.broadcast_netmap_update().await;
    }

    Ok(())
}

async fn request_chunk_from(addr: &str, chunk_name: &str) -> Result<(Vec<u8>, String), AnyErr> {
    let mut s = TcpStream::connect(addr).await?;
    s.write_all(format!("FILE GET-CHUNK {}\n", chunk_name).as_bytes())
        .await?;

    let (r, mut w) = s.into_split();
    let mut reader = BufReader::new(r);

    // Parse FILE RESP-CHUNK <next_addr> <size> <name>
    let mut header = String::new();
    reader.read_line(&mut header).await?;
    let header = header.trim_end_matches(['\r', '\n']);

    let rest = header
        .strip_prefix("FILE RESP-CHUNK ")
        .ok_or_else(|| "malformed FILE RESP-CHUNK".to_string())?;
    let mut parts = rest.splitn(3, ' ');
    let next_addr = parts.next().unwrap_or("").to_string();
    let size_str = parts.next().unwrap_or("");
    let _name_echo = parts.next().unwrap_or("").to_string();

    let size: usize = size_str
        .parse()
        .map_err(|_| "invalid chunk size".to_string())?;
    let mut buf = vec![0u8; size];
    reader.read_exact(&mut buf).await?;

    // Ensure the is writer not dropped too early
    let _ = (&mut w).shutdown().await;

    Ok((buf, next_addr))
}

async fn request_backup_chunk_from(
    addr: &str,
    chunk_name: &str,
) -> Result<(Vec<u8>, String), AnyErr> {
    let mut s = TcpStream::connect(addr).await?;
    // Send the new command
    s.write_all(format!("FILE GET-BACKUP-CHUNK {}\n", chunk_name).as_bytes())
        .await?;

    let (r, mut w) = s.into_split();
    let mut reader = BufReader::new(r);

    // Parse FILE RESP-CHUNK <next_addr> <size> <name>
    let mut header = String::new();
    reader.read_line(&mut header).await?;
    let header = header.trim_end_matches(['\r', '\n']);

    let rest = header
        .strip_prefix("FILE RESP-CHUNK ")
        .ok_or_else(|| "malformed FILE RESP-CHUNK".to_string())?;
    let mut parts = rest.splitn(3, ' ');
    let next_addr = parts.next().unwrap_or("").to_string();
    let size_str = parts.next().unwrap_or("");
    let _name_echo = parts.next().unwrap_or("").to_string();

    let size: usize = size_str
        .parse()
        .map_err(|_| "invalid chunk size".to_string())?;
    let mut buf = vec![0u8; size];
    reader.read_exact(&mut buf).await?;

    // ensure writer not dropped too early
    let _ = (&mut w).shutdown().await;

    Ok((buf, next_addr))
}

// --- FILE LIST

async fn handle_file_list_csv<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
) -> Result<(), AnyErr> {
    // Pure CSV output (header + rows)
    writer.write_all(b"name,start,size\n").await?;

    let tags = node.file_tags.read().await;
    let mut items: Vec<(&String, &node::FileTag)> = tags.iter().collect();
    items.sort_by(|a, b| a.0.cmp(b.0));

    for (name, tag) in items {
        let name_escaped = csv_escape(name);
        writer
            .write_all(format!("{},{},{}\n", name_escaped, tag.start, tag.size).as_bytes())
            .await?;
    }

    Ok(())
}

// --- Helpers

async fn handle_error<W: AsyncWrite + Unpin>(writer: &mut W, err: String) -> Result<(), AnyErr> {
    writer
        .write_all(format!("ERR {}\n", err).as_bytes())
        .await?;
    Ok(())
}

/// Minimal CSV escaping for names containing commas, quotes, or newlines.
fn csv_escape(s: &str) -> String {
    let needs_quotes = s.chars().any(|c| matches!(c, ',' | '"' | '\n' | '\r'));
    if !needs_quotes {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        if ch == '"' {
            out.push('"'); // escape by doubling
        }
        out.push(ch);
    }
    out.push('"');
    out
}

fn host_of(addr: &str) -> &str {
    if addr.contains(':') {
        addr.split(':').next().unwrap_or("127.0.0.1")
    } else {
        "127.0.0.1" // Assume localhost if no host is given
    }
}

// --- Backup Helpers

/// Helper to find the predecessor node from the topology map
async fn get_predecessor_addr(node: &Node) -> Option<String> {
    let my_port = port_str(&node.port);
    let topology = node.topology_map.read().await;
    if topology.is_empty() {
        return None;
    }

    // Find the key whose value is `my_port`
    let predecessor_port = topology
        .iter()
        .find(|(_from, to)| port_str(to) == my_port)
        .map(|(from, _to)| from.clone());

    predecessor_port.map(|port| format!("{}:{}", host_of(&node.port), port))
}

/// Helper to send the notification
/// Push a just-saved chunk to this node's predecessor for backup.
///
/// Replaces the older `notify_predecessor` + `fetch_chunk_for_backup_to`
/// dance. Two TCP setups + two round trips became one. Bytes stream straight
/// from `/content` on the saving node to `/backup` on the predecessor — no
/// `Vec<u8>` of size = chunk_size on either side.
///
/// Fire-and-forget: every error path is logged and swallowed. Predecessor
/// unreachable is the documented degraded-mode behavior; the local save
/// already committed so the chunk is available via PULL even without a
/// backup until the next push for the same chunk happens.
async fn push_to_predecessor(node: Arc<Node>, chunk_name: String) {
    let Some(pred_addr) = get_predecessor_addr(&node).await else {
        tracing::warn!(node = %node.port, chunk = %chunk_name, "No predecessor in topology; skipping backup push.");
        return;
    };

    let src_path = node
        .storage_root
        .join(port_str(&node.port))
        .join("content")
        .join(&chunk_name);

    // Open + verify our local copy, then ship the body (without our own
    // trailer) to the predecessor. The receiver re-hashes and writes its
    // own trailer; the two copies are independent integrity-checked stores.
    let body = match open_chunk_verified(&src_path).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(node = %node.port, chunk = %chunk_name, error = ?e, "Source chunk missing or corrupt; cannot push backup.");
            return;
        }
    };
    let size = body.len() as u64;

    let mut s = match TcpStream::connect(&pred_addr).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(node = %node.port, predecessor = %pred_addr, chunk = %chunk_name, error = ?e, "Predecessor unreachable; skipping backup push.");
            return;
        }
    };

    let header = format!("FILE BACKUP-PUSH {} {}\n", chunk_name, size);
    if let Err(e) = s.write_all(header.as_bytes()).await {
        tracing::warn!(node = %node.port, predecessor = %pred_addr, chunk = %chunk_name, error = ?e, "Failed to send BACKUP-PUSH header.");
        return;
    }
    if size > 0
        && let Err(e) = s.write_all(&body).await
    {
        tracing::warn!(node = %node.port, predecessor = %pred_addr, chunk = %chunk_name, error = ?e, "Failed to stream backup chunk body.");
        return;
    }
    let _ = s.shutdown().await;

    tracing::info!(
        node = %node.port,
        predecessor = %pred_addr,
        chunk = %chunk_name,
        bytes = size,
        "Pushed chunk to predecessor for backup."
    );
}

// --- Gossip and Healing Functions

/// The main gossip loop task
async fn spawn_gossip_loop(node: Arc<Node>) {
    loop {
        // Wait for the gossip interval
        tokio::time::sleep(node.gossip_interval).await;

        // Find out who to ping
        let Some(next_addr) = node.get_next().await else {
            tracing::debug!(
                node = %node.port,
                "Gossip: No next node set, skipping health check."
            );
            continue;
        };

        tracing::debug!(node = %node.port, target = %next_addr, "Gossip: Sending PING");
        match check_node_health(node.clone(), &next_addr).await {
            Ok(_) => {
                tracing::debug!(node = %node.port, from = %next_addr, "Gossip: Received PONG");
            }
            Err(e) => {
                // Health check failed, start the healing process
                tracing::error!(
                    node = %node.port,
                    target = %next_addr,
                    error = ?e,
                    "Gossip: Health check failed"
                );

                // Start healing in a new task to not block the gossip loop
                let heal_node = node.clone();
                tokio::spawn(async move {
                    let node_port = heal_node.port.clone();
                    if let Err(e) = handle_node_death(heal_node, next_addr).await {
                        tracing::error!(node = %node_port, error = ?e, "Gossip: Node healing process failed");
                    }
                });
            }
        }
    }
}

/// Tries to send "NODE PING" and expects "PONG"
async fn check_node_health(_node: Arc<Node>, addr: &str) -> Result<(), AnyErr> {
    let timeout = Duration::from_secs(2);

    // Connect with timeout
    let mut stream = tokio::time::timeout(timeout, TcpStream::connect(addr)).await??;
    stream.write_all(b"NODE PING\n").await?;

    // Read response with timeout
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    tokio::time::timeout(timeout, reader.read_line(&mut buf)).await??;

    if buf.trim().eq_ignore_ascii_case("PONG") {
        Ok(())
    } else {
        Err("invalid PONG response".into())
    }
}

/// The healing process workflow
async fn handle_node_death(node: Arc<Node>, dead_addr: String) -> Result<(), AnyErr> {
    tracing::info!(
        node = %node.port,
        dead_node = %dead_addr,
        "Starting healing process"
    );
    let dead_port = port_str(&dead_addr).to_string();
    let dead_host = host_of(&dead_addr);
    let full_dead_addr = format!("{}:{}", dead_host, dead_port);

    // 1. Update local map to Dead
    node.update_node_status(dead_port.clone(), crate::NodeStatus::Dead)
        .await;

    // 2. Broadcast change
    tracing::info!(
        node = %node.port,
        target_node = %dead_port,
        status = "Dead",
        "Broadcasting node status"
    );
    node.broadcast_netmap_update().await;

    // Tests disable respawn so a killed node stays dead — the rest of the
    // ring's failover paths still get exercised.
    if !node
        .respawn_dead
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        tracing::info!(node = %node.port, dead_node = %full_dead_addr, "Respawn disabled; leaving node dead");
        return Ok(());
    }

    // 3. Start a new process
    tracing::info!(node = %node.port, respawn_addr = %full_dead_addr, "Respawning node");
    let exe = current_exe()?;

    let mut cmd = Command::new(exe);
    cmd.arg("run")
        .arg("--addr")
        .arg(&full_dead_addr)
        .arg("--wait-time")
        .arg(node.gossip_interval.as_millis().to_string());

    // Spawn the child and detach it
    let _ = cmd.spawn()?;

    // Wait for it to be up
    tracing::info!(
        node = %node.port,
        respawn_addr = %full_dead_addr,
        "Waiting for respawned node to listen..."
    );
    wait_until_listening(dead_host, dead_port.parse()?, Duration::from_secs(10)).await?;
    tracing::info!(node = %node.port, respawn_addr = %full_dead_addr, "Respawned node is up.");

    // 4. Update map to Alive
    node.update_node_status(dead_port.clone(), crate::NodeStatus::Alive)
        .await;

    // 5. Share shared data
    tracing::info!(
        node = %node.port,
        target_node = %full_dead_addr,
        "Sharing network data with new node"
    );
    share_data_with_new_node(&node, &full_dead_addr).await?;

    // 6. Broadcast change (Alive)
    tracing::info!(
        node = %node.port,
        target_node = %dead_port,
        status = "Alive",
        "Broadcasting node status"
    );
    node.broadcast_netmap_update().await;

    tracing::info!(
        node = %node.port, healed_node = %full_dead_addr, "Healing process complete."
    );
    Ok(())
}

/// Sends all shared state to a newly spawned node
async fn share_data_with_new_node(node: &Node, new_node_addr: &str) -> Result<(), AnyErr> {
    let timeout = Duration::from_millis(500);

    // Share NETMAP
    let entries = node.get_network_nodes_entries().await;
    let mut s_netmap = tokio::time::timeout(timeout, TcpStream::connect(new_node_addr)).await??;
    s_netmap
        .write_all(format!("NETMAP SET {}\n", entries).as_bytes())
        .await?;
    s_netmap.shutdown().await?;

    // Share TOPOLOGY
    let history = node.get_topology_history().await;
    if !history.is_empty() {
        let mut s_topo = tokio::time::timeout(timeout, TcpStream::connect(new_node_addr)).await??;
        s_topo
            .write_all(format!("TOPOLOGY SET {}\n", history).as_bytes())
            .await?;
        s_topo.shutdown().await?;
    }

    // Share FILE TAGS
    let tags_entries = node.get_file_tags_entries().await;
    if !tags_entries.is_empty() {
        let mut s_tags = tokio::time::timeout(timeout, TcpStream::connect(new_node_addr)).await??;
        s_tags
            .write_all(format!("FILE TAGS-SET {}\n", tags_entries).as_bytes())
            .await?;
        s_tags.shutdown().await?;
    }

    // Share its NEXT hop
    let next_hop_port = node.get_next_for_node(port_str(new_node_addr)).await;
    if let Some(port) = next_hop_port {
        // Reconstruct the full address from the healing node's host and the port
        let host = host_of(&node.port);
        let next_addr = format!("{}:{}", host, port);
        let mut s_next = tokio::time::timeout(timeout, TcpStream::connect(new_node_addr)).await??;
        s_next
            .write_all(format!("NODE NEXT {}\n", next_addr).as_bytes())
            .await?;
        s_next.shutdown().await?;
    }

    Ok(())
}

fn current_exe() -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    Ok(env::current_exe()?)
}

async fn wait_until_listening(
    host: &str,
    port: u16,
    deadline: Duration,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let start = Instant::now();
    let addr = format!("{}:{}", host, port);
    loop {
        match TcpStream::connect(&addr).await {
            Ok(_) => return Ok(()),
            Err(_) => {
                if start.elapsed() > deadline {
                    return Err(format!("timed out while waiting for {}", addr).into());
                }
                sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fair_chunk_len_no_remainder() {
        // 9 / 3 = 3, 3, 3
        assert_eq!(fair_chunk_len(0, 9, 3), 3);
        assert_eq!(fair_chunk_len(1, 9, 3), 3);
        assert_eq!(fair_chunk_len(2, 9, 3), 3);
    }

    #[test]
    fn fair_chunk_len_remainder_distributed_to_first() {
        // 10 / 3 = 4, 3, 3
        assert_eq!(fair_chunk_len(0, 10, 3), 4);
        assert_eq!(fair_chunk_len(1, 10, 3), 3);
        assert_eq!(fair_chunk_len(2, 10, 3), 3);
    }

    #[test]
    fn fair_chunk_len_single_part() {
        assert_eq!(fair_chunk_len(0, 100, 1), 100);
    }

    #[test]
    fn fair_chunk_len_zero_size() {
        for i in 0..5 {
            assert_eq!(fair_chunk_len(i, 0, 5), 0);
        }
    }

    #[test]
    fn fair_chunk_len_one_byte_five_parts() {
        assert_eq!(fair_chunk_len(0, 1, 5), 1);
        for i in 1..5 {
            assert_eq!(fair_chunk_len(i, 1, 5), 0);
        }
    }

    #[test]
    fn fair_chunk_len_sum_invariant() {
        for &(parts, size) in &[(1u32, 1u64), (1, 1 << 20), (3, 7), (5, 1 << 20), (7, 123_456)] {
            let total: u64 = (0..parts).map(|i| fair_chunk_len(i, size, parts)).sum();
            assert_eq!(total, size, "parts={parts} size={size}");
        }
    }

    #[test]
    fn chunk_file_name_zero_padded() {
        assert_eq!(
            chunk_file_name("hello.txt", 0, 3),
            "hello.txt.part-001-of-003"
        );
        assert_eq!(
            chunk_file_name("hello.txt", 9, 10),
            "hello.txt.part-010-of-010"
        );
    }

    #[test]
    #[should_panic]
    fn chunk_file_name_panics_on_unvalidated_name() {
        // The validator (parse layer) rejects '/'; chunk_file_name's
        // debug_assert! pins this invariant so a regression in the dispatch
        // path that bypasses validation triggers loudly in tests.
        let _ = chunk_file_name("a/b", 0, 1);
    }

    #[test]
    fn validate_filename_accepts_allowlist() {
        assert!(validate_filename("normal.txt").is_ok());
        assert!(validate_filename("already-safe.bin").is_ok());
        assert!(validate_filename("under_score").is_ok());
        assert!(validate_filename("foo.bin.part-001-of-003").is_ok());
    }

    #[test]
    fn validate_filename_rejects_path_separators() {
        assert!(validate_filename("/abs/path").is_err());
        assert!(validate_filename("a/b").is_err());
        assert!(validate_filename("a\\b").is_err());
    }

    #[test]
    fn validate_filename_rejects_traversal() {
        // The whole point of replacing sanitize_filename: `..` doesn't pass.
        assert!(validate_filename("..").is_err());
        assert!(validate_filename(".").is_err());
        assert!(validate_filename("...").is_err());
        // Path separators inside a name with `..` are also blocked (twice over).
        assert!(validate_filename("../etc/passwd").is_err());
        // A name that *contains* dots but has at least one other char is fine.
        assert!(validate_filename(".hidden").is_ok());
        assert!(validate_filename("a.b").is_ok());
    }

    #[test]
    fn validate_filename_rejects_control_and_punctuation() {
        assert!(validate_filename("a\0b").is_err());
        assert!(validate_filename("a\nb").is_err());
        assert!(validate_filename("a:b").is_err());
        assert!(validate_filename("a;b").is_err());
        assert!(validate_filename("a|b").is_err());
        assert!(validate_filename("a b").is_err());
        assert!(validate_filename("a,b").is_err());
        assert!(validate_filename("a\"b").is_err());
    }

    #[test]
    fn validate_filename_rejects_empty_and_oversize() {
        assert!(validate_filename("").is_err());
        let huge = "a".repeat(256);
        assert!(validate_filename(&huge).is_err());
        let max = "a".repeat(255);
        assert!(validate_filename(&max).is_ok());
    }

    #[test]
    fn validate_filename_rejects_non_ascii() {
        // The allowlist is ASCII-only by design — disk encoding, CSV, and
        // wire formatting all assume single-byte name handling.
        assert!(validate_filename("héllo.txt").is_err());
    }

    #[test]
    fn csv_escape_basics() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_escape("a\nb"), "\"a\nb\"");
        assert_eq!(csv_escape(""), "");
    }

    #[test]
    fn host_of_extracts() {
        assert_eq!(host_of("127.0.0.1:7000"), "127.0.0.1");
        assert_eq!(host_of("localhost:7000"), "localhost");
        assert_eq!(host_of("7000"), "127.0.0.1");
    }

    #[test]
    fn host_of_ipv6_bracket_pin() {
        // Current `split(':')` does not handle IPv6 brackets correctly;
        // it stops at the first colon. Pin the behavior so we know if a
        // future fix changes it.
        assert_eq!(host_of("[::1]:7000"), "[");
    }

    // --- Additional edge-case anchors

    #[test]
    fn fair_chunk_len_size_lt_parts_distributes_one_per_first_size_chunks() {
        // 3 bytes across 5 parts: first 3 chunks get 1 byte each, last 2
        // get 0 (the empty-chunk path that the relay handler must skip).
        let parts = 5u32;
        let size = 3u64;
        let lens: Vec<u64> = (0..parts).map(|i| fair_chunk_len(i, size, parts)).collect();
        assert_eq!(lens, vec![1, 1, 1, 0, 0]);
    }

    #[test]
    #[should_panic]
    fn fair_chunk_len_parts_zero_panics() {
        // parts=0 triggers division-by-zero (u64 / 0). Pinning the
        // precondition: callers must enforce parts >= 1 (handle_file_push
        // does, via `network_size().await as u32`).
        let _ = fair_chunk_len(0, 100, 0);
    }

    #[test]
    fn chunk_file_name_zero_pads_3_digits_at_boundary() {
        // 100 chunks: index 99 maps to part 100 of 100, three-digit field.
        assert_eq!(
            chunk_file_name("x", 99, 100),
            "x.part-100-of-100"
        );
    }

    #[test]
    fn chunk_file_name_separator_format_pinned() {
        // The exact format `<safe>.part-NNN-of-MMM` is the contract that
        // handle_file_push_chunk relies on when recovering the parent name
        // via rsplit_once(".part-"). A change here breaks PULL.
        let n = chunk_file_name("foo.bin", 0, 3);
        assert_eq!(n, "foo.bin.part-001-of-003");
    }

    #[test]
    fn chunk_name_roundtrip_via_rsplit_once_part() {
        // The mirror invariant: parent recovery from chunk name.
        let chunk = chunk_file_name("foo.bin", 1, 3);
        let (parent, _) = chunk.rsplit_once(".part-").unwrap();
        assert_eq!(parent, "foo.bin");
    }

    #[test]
    fn chunk_file_name_appended_to_validated_input_still_validates() {
        // Server-generated chunk names satisfy the allowlist by construction:
        // the suffix `.part-NNN-of-MMM` is alphanum + '.' + '-'.
        let n = chunk_file_name("foo.bin", 0, 3);
        assert!(validate_filename(&n).is_ok(), "got {n}");
    }

    #[test]
    fn csv_escape_quote_doubling() {
        // The doubled-quote contract used by handle_file_list_csv.
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn csv_escape_carriage_return_quoted() {
        // The `\r` predicate branch in csv_escape.
        assert_eq!(csv_escape("a\rb"), "\"a\rb\"");
    }

    #[test]
    fn csv_escape_no_special_chars_returns_input_string() {
        assert_eq!(csv_escape("plain-name.bin"), "plain-name.bin");
    }

    #[test]
    fn host_of_empty_string_falls_back() {
        // Empty input has no colon → fallback branch.
        assert_eq!(host_of(""), "127.0.0.1");
    }

    #[test]
    fn host_of_localhost_with_port() {
        // Named anchor for the literal-host-name case.
        assert_eq!(host_of("localhost:7000"), "localhost");
    }
}
