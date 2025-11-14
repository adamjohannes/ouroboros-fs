use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};
use std::{env, path::PathBuf, sync::Arc};
use tokio::fs;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, copy,
};
use tokio::net::{TcpSocket, TcpStream};
use tokio::process::Command;
use tokio::time::sleep;
use tracing;

use crate::{
    node::{self, Node, append_edge, port_str},
    protocol,
};

type AnyErr = Box<dyn Error + Send + Sync>;

/// Run the TCP server and handle connections.
pub async fn run(bind_addr: &str, gossip_interval: Duration, file_size: u64) -> Result<(), AnyErr> {
    // 1. Parse the address with an explicit type annotation
    let addr: std::net::SocketAddr = bind_addr.parse()?;

    // 2. Create a socket based on IP version
    let socket = if addr.is_ipv6() {
        TcpSocket::new_v6()?
    } else {
        TcpSocket::new_v4()?
    };

    // 3. Set the SO_REUSEADDR option
    socket.set_reuseaddr(true)?;

    // 4. Set the SO_REUSEPORT option (required on macOS/BSD to bypass TIME_WAIT)
    #[cfg(unix)]
    socket.set_reuseport(true)?;

    // 5. Bind the socket to the address
    socket.bind(addr)?;

    // 6. Listen for incoming connections
    let listener = socket.listen(1024)?;

    // 7. Get the local address
    let local = listener.local_addr()?;

    // Initialize Node structure
    let node = Node::new(local.to_string(), gossip_interval, file_size);
    tracing::info!(node = %node.port, "Node listening");

    // Create nodes/<port>/content and nodes/<port>/backup directories
    let port_only = port_str(&node.port);
    let content_dir = format!("nodes/{}/content", port_only);
    let backup_dir = format!("nodes/{}/backup", port_only);

    if let Err(e) = fs::create_dir_all(&content_dir).await {
        tracing::error!(node = %node.port, dir = %content_dir, error = ?e, "Failed to create node content directory");
        return Err(e.into());
    }
    if let Err(e) = fs::create_dir_all(&backup_dir).await {
        tracing::error!(node = %node.port, dir = %backup_dir, error = ?e, "Failed to create node backup directory");
        return Err(e.into());
    }

    tracing::info!(node = %node.port, content_dir = %content_dir, backup_dir = %backup_dir, "Created node directories");

    // Spawn the gossip loop
    if gossip_interval > Duration::from_millis(0) {
        let gossip_node = Arc::clone(&node);
        tokio::spawn(async move {
            tracing::info!(
                node = %gossip_node.port,
                interval = ?gossip_interval,
                "Gossip loop starting"
            );
            spawn_gossip_loop(gossip_node).await;
        });
    }

    // Accept connections
    loop {
        let (stream, peer) = listener.accept().await?;
        let node = Arc::clone(&node);

        // Clone the port for logging before moving `node`
        let node_port = node.port.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_client(node, stream).await {
                tracing::error!(node = %node_port, peer = %peer, error = ?e, "Client connection error");
            }
        });
    }
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
                protocol::Command::FileRelayBlob {
                    token,
                    start_addr,
                    size,
                    name,
                } => {
                    handle_file_relay_blob(
                        Arc::clone(&node),
                        &mut reader,
                        &mut writer,
                        token,
                        start_addr,
                        size,
                        name,
                    )
                    .await?
                }
                protocol::Command::FileRelayStream {
                    token,
                    start_addr,
                    file_size,
                    parts,
                    index,
                    name,
                } => {
                    handle_file_relay_stream(
                        Arc::clone(&node),
                        &mut reader,
                        &mut writer,
                        token,
                        start_addr,
                        file_size,
                        parts,
                        index,
                        name,
                    )
                    .await?
                }
                protocol::Command::FileGetChunk { name } => {
                    handle_file_get_chunk(&node, &mut writer, name).await?
                }

                // FILE (backup)
                protocol::Command::FileNotifyChunkSaved { name } => {
                    handle_file_notify_chunk_saved(Arc::clone(&node), &mut writer, name).await?
                }
                protocol::Command::FileGetChunkForBackup { name } => {
                    handle_file_get_chunk_for_backup(&node, &mut writer, name).await?
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

/* --- Command handlers --- */

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

/* -------- NETMAP -------- */

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

/* -------- FILE CHUNKING helpers -------- */

fn fair_chunk_len(index: u32, total_size: u64, parts: u32) -> u64 {
    // Distribute remainder to the first (total_size % parts) chunks
    let base = total_size / parts as u64;
    let rem = total_size % parts as u64;
    if (index as u64) < rem { base + 1 } else { base }
}

fn sum_len_up_to_inclusive(index: u32, total_size: u64, parts: u32) -> u64 {
    (0..=index)
        .map(|i| fair_chunk_len(i, total_size, parts))
        .sum()
}

fn chunk_file_name(name: &str, index: u32, parts: u32) -> String {
    let safe = sanitize_filename(name);
    format!("{}.part-{:03}-of-{:03}", safe, index + 1, parts)
}

/* -------- FILE: PUSH / HOP handlers -------- */

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
    // Handle files larger than the node supports
    if size > node.file_size {
        tracing::error!(node = %node.port, file_name = %name, file_size = size, max_file_size = %node.file_size, "File size is too large");

        let msg = format!(
            "ERR File size is too large ({} > {})\n",
            size, node.file_size
        );
        writer.write_all(msg.as_bytes()).await?;

        // Drain the stream to consume the file body the client is sending
        let mut sink = vec![0u8; size as usize];
        reader.read_exact(&mut sink).await?;

        return Ok(());
    }

    let name = Path::new(&name)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Determine how many parts to split into: number of known nodes (fallback to 1)
    let parts: u32 = node.network_size().await as u32;

    // Update local file_tags (start, size, parts)
    let start_port_num: u16 = port_str(&node.port).parse().unwrap_or(0);
    node.set_file_tag(&name, start_port_num, size, parts).await;

    if parts == 1 {
        // Single node: read everything and store locally
        let mut buf = vec![0u8; size as usize];
        reader.read_exact(&mut buf).await?;
        let _ = save_into_node_dir(&node, &name, &buf, "content").await?;

        // Notify predecessor
        let node_clone = Arc::clone(&node);
        let name_clone = name.clone();
        tokio::spawn(async move {
            notify_predecessor(node_clone, name_clone).await;
        });

        writer
            .write_all(format!("FILE {} bytes '{}' stored locally\nOK", size, name).as_bytes())
            .await?;
        return Ok(());
    }

    // We need a next hop
    let Some(next) = node.get_next().await else {
        writer.write_all(b"ERR no next hop set\n").await?;
        // Drain the stream to keep protocol in sync
        let mut sink = vec![0u8; size as usize];
        reader.read_exact(&mut sink).await?;
        return Ok(());
    };

    let first_len = fair_chunk_len(0, size, parts);
    // Read and save this node's first chunk
    let mut first = vec![0u8; first_len as usize];
    reader.read_exact(&mut first).await?;
    let chunk_name = chunk_file_name(&name, 0, parts);
    let saved_as = save_into_node_dir(&node, &chunk_name, &first, "content").await?;

    // Notify predecessor
    let node_clone = Arc::clone(&node);
    let chunk_name_clone = chunk_name.clone();
    tokio::spawn(async move {
        notify_predecessor(node_clone, chunk_name_clone).await;
    });

    tracing::info!(
        node = %node.port,
        chunk = 1,
        parts,
        file = %saved_as.display(),
        bytes = first_len,
        "Saved file chunk"
    );

    // Open connection to next and stream the remaining bytes
    let mut s = TcpStream::connect(&next).await?;
    let token = node.make_file_token();
    let header = format!(
        "FILE RELAY-STREAM {} {} {} {} {} {}\n",
        token, &node.port, size, parts, 1, name
    );
    s.write_all(header.as_bytes()).await?;

    // Forward exactly the remaining bytes (size - first_len) from client -> next
    let mut limited = reader.take(size - first_len);
    copy(&mut limited, &mut s).await?;

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

async fn handle_file_relay_blob<R, W>(
    node: Arc<Node>,
    reader: &mut R,
    writer: &mut W,
    token: String,
    start_addr: String,
    size: u64,
    name: String,
) -> Result<(), AnyErr>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Read the exact file body first
    let mut buf = vec![0u8; size as usize];
    reader.read_exact(&mut buf).await?;

    // If this hop delivered back to the start node, just finish & ACK.
    if port_str(&node.port) == port_str(&start_addr) {
        let _ = node.finish_file(&token).await;
        let _ = writer.write_all(b"OK\n").await;
        return Ok(());
    }

    // Save locally
    if let Err(e) = save_into_node_dir(&node, &name, &buf, "content").await {
        tracing::error!(node = %node.port, file_name = %name, error = ?e, "Failed to save relayed file blob");
    } else {
        // Notify predecessor
        let node_clone = Arc::clone(&node);
        let name_clone = name.clone();
        tokio::spawn(async move {
            notify_predecessor(node_clone, name_clone).await;
        });
    }

    // Forward to next
    if let Some(_) = node.get_next().await {
        if let Err(e) = node
            .forward_file_relay_blob(&token, &start_addr, size, &name, &buf)
            .await
        {
            tracing::warn!(node = %node.port, error = ?e, "FILE RELAY-BLOB forward failed");
        }
    }

    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

async fn handle_file_relay_stream<R, W>(
    node: Arc<Node>,
    reader: &mut R,
    writer: &mut W,
    token: String,
    start_addr: String,
    file_size: u64,
    parts: u32,
    index: u32,
    name: String,
) -> Result<(), AnyErr>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if index >= parts {
        writer
            .write_all(b"ERR bad FILE RELAY-STREAM index\n")
            .await?;
        return Ok(());
    }

    // Compute my chunk length and read exactly those bytes
    let my_len = fair_chunk_len(index, file_size, parts);
    let mut buf = vec![0u8; my_len as usize];
    reader.read_exact(&mut buf).await?;

    // Tag the file on this node too
    let start_port_num: u16 = port_str(&start_addr).parse().unwrap_or(0);
    node.set_file_tag(&name, start_port_num, file_size, parts)
        .await;

    // Save my chunk locally
    let chunk_name = chunk_file_name(&name, index, parts);
    let saved_as = save_into_node_dir(&node, &chunk_name, &buf, "content").await?;

    // Notify predecessor
    let node_clone = Arc::clone(&node);
    tokio::spawn(async move {
        notify_predecessor(node_clone, chunk_name).await;
    });

    tracing::info!(
        node = %node.port,
        chunk = index + 1,
        parts,
        file = %saved_as.display(),
        bytes = my_len,
        "Saved file chunk"
    );

    // If not the last chunk, forward remaining bytes to next with index+1
    let consumed = sum_len_up_to_inclusive(index, file_size, parts);
    let remaining = file_size - consumed;
    if remaining > 0 {
        if let Some(next) = node.get_next().await {
            let mut s = TcpStream::connect(&next).await?;
            let header = format!(
                "FILE RELAY-STREAM {} {} {} {} {} {}\n",
                token,
                start_addr,
                file_size,
                parts,
                index + 1,
                name
            );
            s.write_all(header.as_bytes()).await?;
            let mut limited = reader.take(remaining);
            copy(&mut limited, &mut s).await?;
        }
    } else {
        // nothing left to do
        let _ = node.finish_file(&token).await;
    }

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

/* -------- FILE RETRIEVAL (PULL / GET-CHUNK) -------- */

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

    // Assemble full file by walking the ring starting at start_addr
    let bytes = pull_file_from_ring(node, &name, &start_addr, parts, file_size).await?;

    // IMPORTANT: return *pure bytes*, no textual header or trailer.
    writer.write_all(&bytes).await?;
    Ok(())
}

async fn handle_file_get_chunk<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    name: String,
) -> Result<(), AnyErr> {
    let next = node.get_next().await.unwrap_or_else(|| node.port.clone());

    // Read the specific chunk from the "content" directory
    let chunk_path = PathBuf::from(format!(
        "nodes/{}/content/{}",
        port_str(&node.port),
        sanitize_filename(&name)
    ));

    let chunk = fs::read(&chunk_path).await.unwrap_or_default();

    // Header + exact bytes for node-to-node transfer
    writer
        .write_all(format!("FILE RESP-CHUNK {} {} {}\n", next, chunk.len(), name).as_bytes())
        .await?;
    writer.write_all(&chunk).await?;
    Ok(())
}

/* -------- BACKUP HANDLERS -------- */

/// Handles "FILE NOTIFY-CHUNK-SAVED <name>"
/// This node is the predecessor (i). It is being notified by its successor (i+1).
/// It must now fetch the chunk from (i+1) and save it to its /backup dir.
async fn handle_file_notify_chunk_saved<W: AsyncWrite + Unpin>(
    node: Arc<Node>,
    writer: &mut W,
    chunk_name: String,
) -> Result<(), AnyErr> {
    // Find the successor (i+1) from whom it received the notification
    let Some(next_addr) = node.get_next().await else {
        tracing::warn!(node = %node.port, chunk = %chunk_name, "Got NOTIFY-CHUNK-SAVED but have no next_port to fetch from.");
        writer.write_all(b"OK (but no next_port)\n").await?;
        return Ok(());
    };

    tracing::info!(
        node = %node.port,
        target = %next_addr,
        chunk = %chunk_name,
        "Backup process: Requesting chunk from successor."
    );

    // Spawn a new task to do the backup and ACK the notification immediately
    tokio::spawn(async move {
        match request_chunk_for_backup(&next_addr, &chunk_name).await {
            Ok(chunk_data) => {
                if chunk_data.is_empty() {
                    tracing::warn!(
                        node = %node.port,
                        from = %next_addr,
                        chunk = %chunk_name,
                        "Backup fetch: Got 0 bytes."
                    );
                    return;
                }

                // Save to "/backup" directory
                match save_into_node_dir(&node, &chunk_name, &chunk_data, "backup").await {
                    Ok(path) => {
                        tracing::info!(
                            node = %node.port,
                            from = %next_addr,
                            chunk = %chunk_name,
                            path = %path.display(),
                            bytes = chunk_data.len(),
                            "Backup chunk saved successfully."
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            node = %node.port,
                            chunk = %chunk_name,
                            error = ?e,
                            "Failed to save backup chunk."
                        );
                    }
                }
            }
            Err(e) => {
                tracing::error!(
                    node = %node.port,
                    target = %next_addr,
                    chunk = %chunk_name,
                    error = ?e,
                    "Failed to request chunk for backup."
                );
            }
        }
    });

    // ACK the notification immediately
    writer.write_all(b"OK\n").await?;
    Ok(())
}

/// Handles "FILE GET-CHUNK-FOR-BACKUP <name>"
/// This is used by the backup process. It reads from the "/content" dir
/// and returns <8-byte-size><raw-bytes>.
async fn handle_file_get_chunk_for_backup<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    name: String, // This is the full chunk name
) -> Result<(), AnyErr> {
    // Sanitize the name, although it should already be safe
    let fname = sanitize_filename(&name);
    let path = PathBuf::from(format!(
        "nodes/{}/content/{}", // Read from "/content"
        port_str(&node.port),
        fname
    ));

    let (chunk, size) = match fs::read(&path).await {
        Ok(data) => {
            let size = data.len() as u64;
            (data, size)
        }
        Err(e) => {
            tracing::warn!(
                node = %node.port,
                path = %path.display(),
                error = ?e,
                "GET-CHUNK-FOR-BACKUP: File not found or unreadable."
            );
            (Vec::new(), 0u64) // Send 0 bytes on error
        }
    };

    // 1. Send the 8-byte size (u64, big-endian)
    writer.write_all(&size.to_be_bytes()).await?;

    // 2. Send the raw file bytes
    if size > 0 {
        writer.write_all(&chunk).await?;
    }
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

    // Read from "backup" directory
    let chunk_path = PathBuf::from(format!(
        "nodes/{}/backup/{}",
        port_str(&node.port),
        sanitize_filename(&name)
    ));

    let chunk = fs::read(&chunk_path).await.unwrap_or_default();

    // Respond with the same protocol message as GET-CHUNK
    writer
        .write_all(format!("FILE RESP-CHUNK {} {} {}\n", next, chunk.len(), name).as_bytes())
        .await?;
    writer.write_all(&chunk).await?;
    Ok(())
}

/* --- PULL helpers --- */

async fn pull_file_from_ring(
    node: &Node,
    name: &str,
    start_addr: &str,
    parts: u32,
    _file_size: u64,
) -> Result<Vec<u8>, AnyErr> {
    let mut out = Vec::new();
    let mut current_addr = start_addr.to_string();
    let mut current_port = port_str(start_addr).to_string();
    let host = host_of(start_addr);
    let topology = node.topology_map.read().await;

    for i in 0..parts {
        let chunk_name = chunk_file_name(name, i, parts);
        let chunk: Vec<u8>;

        // 1. Try to get chunk from the current node
        match request_chunk_from(&current_addr, &chunk_name).await {
            Ok((chunk_data, _next_addr_ignored)) => {
                // Node is alive.
                tracing::debug!(
                    node = %node.port,
                    from = %current_addr,
                    chunk_name = %chunk_name,
                    "Got chunk successfully."
                );
                chunk = chunk_data;
            }
            Err(e) => {
                // 1.2. Node is likely dead
                tracing::warn!(
                    node = %node.port,
                    target_node = %current_addr,
                    chunk_name = %chunk_name,
                    error = ?e,
                    "Failed to get chunk from node. Attempting to use backup."
                );

                // Mark node as Dead and broadcast this change
                tracing::info!(
                    node = %node.port,
                    dead_node = %current_port,
                    "Marking node as Dead and broadcasting netmap update."
                );
                node.update_node_status(current_port.clone(), crate::NodeStatus::Dead)
                    .await;

                // Await the broadcast to ensure state is sent before we continue
                node.broadcast_netmap_update().await;

                // 1.3. Find the predecessor of the dead node (the one holding the backup)
                let pred_port = topology
                    .iter()
                    .find(|(_from, to)| port_str(to) == current_port)
                    .map(|(from, _to)| from.clone());

                let Some(pred_port) = pred_port else {
                    tracing::error!(
                        node = %node.port,
                        dead_node = %current_addr,
                        "No predecessor found in topology for dead node. Cannot fetch backup."
                    );
                    chunk = Vec::new();
                    out.extend_from_slice(&chunk);

                    // Manually advance to the next node to avoid getting stuck
                    let next_port = topology.get(&current_port).cloned();
                    if let Some(port) = next_port {
                        current_port = port.clone();
                        current_addr = format!("{}:{}", host, port);
                    } else {
                        tracing::error!(node=%node.port, dead_node=%current_port, "Topology broken. Cannot find next hop.");
                        break;
                    }
                    continue;
                };

                let pred_addr = format!("{}:{}", host, pred_port);

                // 1.4. Request the backup chunk from the predecessor
                match request_backup_chunk_from(&pred_addr, &chunk_name).await {
                    Ok((chunk_data, _)) => {
                        tracing::info!(
                            node = %node.port,
                            from_backup_node = %pred_addr,
                            chunk_name = %chunk_name,
                            "Successfully retrieved chunk from backup."
                        );
                        chunk = chunk_data;
                    }
                    Err(e_backup) => {
                        tracing::error!(
                            node = %node.port,
                            backup_node = %pred_addr,
                            chunk_name = %chunk_name,
                            error = ?e_backup,
                            "Failed to get chunk from backup node. File will be corrupt."
                        );
                        chunk = Vec::new();
                    }
                }
            }
        }

        out.extend_from_slice(&chunk);

        // 2. Find the next node in the chain to query
        let next_port = topology.get(&current_port).cloned();

        if let Some(port) = next_port {
            current_port = port.clone();
            current_addr = format!("{}:{}", host, port);
        } else {
            // This should only happen if the topology is broken
            tracing::error!(
                node = %node.port,
                from_node = %current_port,
                "Topology map is broken. Cannot find next hop. Stopping pull."
            );
            break;
        }
    }

    Ok(out)
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

/* -------- FILE LIST -------- */

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

/* --- Helpers and Errors --- */

async fn handle_error<W: AsyncWrite + Unpin>(writer: &mut W, err: String) -> Result<(), AnyErr> {
    writer
        .write_all(format!("ERR {}\n", err).as_bytes())
        .await?;
    Ok(())
}

fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        let bad = ch == '/'
            || ch == '\\'
            || ch == '\0'
            || ch == ':'
            || ch == '|'
            || ch == ';'
            || ch == '\n'
            || ch == '\r';
        if bad {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() { "_".into() } else { out }
}

async fn save_into_node_dir(
    node: &Node,
    name: &str,
    data: &[u8],
    subdir: &str,
) -> Result<PathBuf, AnyErr> {
    let fname = sanitize_filename(name);
    let path = PathBuf::from(format!(
        "nodes/{}/{}/{}",
        port_str(&node.port),
        subdir,
        fname
    ));
    fs::write(&path, data).await?;
    Ok(path)
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

/* --- BACKUP HELPERS --- */

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
async fn notify_predecessor(node: Arc<Node>, chunk_name: String) {
    if let Some(pred_addr) = get_predecessor_addr(&node).await {
        tracing::info!(
            node = %node.port,
            predecessor = %pred_addr,
            chunk = %chunk_name,
            "Notifying predecessor of new chunk."
        );

        // Try to send the notification
        match TcpStream::connect(&pred_addr).await {
            Ok(mut stream) => {
                let line = format!("FILE NOTIFY-CHUNK-SAVED {}\n", chunk_name);
                if let Err(e) = stream.write_all(line.as_bytes()).await {
                    tracing::warn!(node = %node.port, target = %pred_addr, error = ?e, "Failed to send chunk notification.");
                }
                // No need to wait for an ACK
                let _ = stream.shutdown().await;
            }
            Err(e) => {
                tracing::warn!(node = %node.port, target = %pred_addr, error = ?e, "Failed to connect to predecessor for notification.");
            }
        }
    } else {
        tracing::warn!(node = %node.port, chunk = %chunk_name, "No predecessor found in topology map. Cannot send backup notification.");
    }
}

/// Helper function for the backup process
async fn request_chunk_for_backup(addr: &str, name: &str) -> Result<Vec<u8>, AnyErr> {
    let mut s = TcpStream::connect(addr).await?;

    // 1. Send the request
    s.write_all(format!("FILE GET-CHUNK-FOR-BACKUP {}\n", name).as_bytes())
        .await?;

    // 2. Read the 8-byte size prefix
    let mut size_buf = [0u8; 8];
    s.read_exact(&mut size_buf).await?;
    let size = u64::from_be_bytes(size_buf);

    if size == 0 {
        // This means the file was empty or not found
        return Ok(Vec::new());
    }

    // 3. Read exactly 'size' bytes of data
    let mut buf = vec![0u8; size as usize];
    s.read_exact(&mut buf).await?;

    // The TcpStream 's' is dropped here when the task ends,
    // closing the connection from the client side
    Ok(buf)
}

/* --- Gossip and Healing Functions --- */

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
