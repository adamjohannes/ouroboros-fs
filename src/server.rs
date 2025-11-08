use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant};
use std::{env, error, path::PathBuf, sync::Arc};
use tokio::fs;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, copy,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::time::sleep;

use crate::{
    node::{self, Node, append_edge, port_str},
    protocol,
};

type AnyErr = Box<dyn error::Error + Send + Sync>;

/// Run the TCP server and handle connections.
pub async fn run(bind_addr: &str, gossip_interval: Duration) -> Result<(), AnyErr> {
    // Initialize TCP listener and collect its address
    let listener = TcpListener::bind(bind_addr).await?;
    let local = listener.local_addr()?;

    // Initialize Node structure
    let node = Node::new(local.to_string(), gossip_interval);
    println!("[{}] node listening on {}", node.port, node.port);

    // Create nodes/<port> directory once the node is up
    let port_only = port_str(&node.port);
    let node_dir = format!("nodes/{}", port_only);
    if let Err(e) = fs::create_dir_all(&node_dir).await {
        eprintln!("[{}] failed to create {}: {}", node.port, node_dir, e);
    } else {
        println!("[{}] created {}", node.port, node_dir);
    }

    // Spawn the gossip loop
    if gossip_interval > Duration::from_millis(0) {
        let gossip_node = Arc::clone(&node);
        tokio::spawn(async move {
            println!(
                "[{}] Gossip loop starting (interval: {:?})",
                gossip_node.port, gossip_interval
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
                eprintln!("[{node_port}] client {peer}: error: {e}");
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
                    handle_file_push(&node, &mut reader, &mut writer, size, name).await?
                }
                protocol::Command::FilePull { name } => {
                    handle_file_pull(&node, &mut writer, name).await?
                }
                protocol::Command::FileList => handle_file_list_csv(&node, &mut writer).await?,
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
                        &node,
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
                        &node,
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

async fn handle_ring_forward<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    mut ttl: u32,
    msg: String,
) -> Result<(), AnyErr> {
    println!("[{}] RING FORWARD(ttl={}) msg: {}", node.port, ttl, msg);

    if ttl > 0 {
        ttl -= 1;
        if let Some(next_addr) = node.get_next().await {
            if let Err(e) = node.forward_ring_forward(ttl, &msg).await {
                eprintln!("[{}] forward error to {}: {}", node.port, next_addr, e);
            }
        } else {
            eprintln!("[{}] no next node set; dropping", node.port);
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
            eprintln!(
                "[{}] TOPOLOGY DONE send failed to {}: {}",
                node.port, start_addr, e
            );
        }
    } else {
        if let Err(e) = node
            .forward_topology_hop(&token, &start_addr, &new_history)
            .await
        {
            eprintln!(
                "[{}] TOPOLOGY HOP forward failed to {}: {}",
                node.port, next_addr, e
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
            eprintln!(
                "[{}] NETMAP DONE send failed to {}: {}",
                node.port, start_addr, e
            );
        }
    } else {
        if let Err(e) = node
            .forward_netmap_hop(&token, &start_addr, &new_entries)
            .await
        {
            eprintln!(
                "[{}] NETMAP HOP forward failed to {}: {}",
                node.port, next_addr, e
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
    node: &Node,
    reader: &mut R,
    writer: &mut W,
    size: u64,
    name: String,
) -> Result<(), AnyErr>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let name = Path::new(&name)
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Determine how many parts to split into: number of known nodes (fallback to 1)
    let parts: u32 = node.network_size().await as u32;

    // Update local file_tags (start, size)
    let start_port_num: u16 = port_str(&node.port).parse().unwrap_or(0);
    node.set_file_tag(&name, start_port_num, size).await;

    if parts == 1 {
        // Single node: read everything and store locally
        let mut buf = vec![0u8; size as usize];
        reader.read_exact(&mut buf).await?;
        let _ = save_into_node_dir(node, &name, &buf).await?;
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
    let saved_as = save_into_node_dir(node, &chunk_file_name(&name, 0, parts), &first).await?;
    println!(
        "[{}] saved chunk 1/{parts}: {} ({} bytes)",
        node.port,
        saved_as.display(),
        first_len
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
    node: &Node,
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
    if let Err(e) = save_into_node_dir(node, &name, &buf).await {
        eprintln!("[{}] failed to save file '{}': {}", node.port, name, e);
    }

    // Forward to next
    if let Some(_) = node.get_next().await {
        if let Err(e) = node
            .forward_file_relay_blob(&token, &start_addr, size, &name, &buf)
            .await
        {
            eprintln!("[{}] FILE RELAY-BLOB forward failed: {}", node.port, e);
        }
    }

    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

async fn handle_file_relay_stream<R, W>(
    node: &Node,
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
    node.set_file_tag(&name, start_port_num, file_size).await;

    // Save my chunk locally
    let saved_as = save_into_node_dir(node, &chunk_file_name(&name, index, parts), &buf).await?;
    println!(
        "[{}] saved chunk {}/{}: {} ({} bytes)",
        node.port,
        index + 1,
        parts,
        saved_as.display(),
        my_len
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
    let start_addr = format!("{}:{}", host_of(&node.port), start_port);
    drop(tags);

    // Assemble full file by walking the ring starting at start_addr
    let bytes = pull_file_from_ring(node, &name, &start_addr).await?;

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
    let chunk = read_local_chunk_bytes(node, &name)
        .await
        .unwrap_or_default();

    // Header + exact bytes for node-to-node transfer
    writer
        .write_all(format!("FILE RESP-CHUNK {} {} {}\n", next, chunk.len(), name).as_bytes())
        .await?;
    writer.write_all(&chunk).await?;
    Ok(())
}

/* --- PULL helpers --- */

async fn pull_file_from_ring(node: &Node, name: &str, start_addr: &str) -> Result<Vec<u8>, AnyErr> {
    let start_port = port_str(start_addr).to_string();
    let mut out = Vec::new();

    let mut current = start_addr.to_string();

    loop {
        let curr_port = port_str(&current).to_string();

        // Fetch chunk from `current` (local fast-path)
        let (chunk, next_addr) = if curr_port == port_str(&node.port) {
            let bytes = read_local_chunk_bytes(node, name).await.unwrap_or_default();
            let next = node.get_next().await.unwrap_or_else(|| current.clone());
            (bytes, next)
        } else {
            request_chunk_from(&current, name).await?
        };

        out.extend_from_slice(&chunk);

        // Decide whether to continue
        if port_str(&next_addr) == start_port {
            break;
        }
        current = next_addr;
    }

    Ok(out)
}

async fn request_chunk_from(addr: &str, name: &str) -> Result<(Vec<u8>, String), AnyErr> {
    let mut s = TcpStream::connect(addr).await?;
    s.write_all(format!("FILE GET-CHUNK {}\n", name).as_bytes())
        .await?;

    let (r, mut w) = s.into_split();
    let mut reader = BufReader::new(r);

    // Parse: FILE RESP-CHUNK <next_addr> <size> <name>

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

async fn read_local_chunk_bytes(node: &Node, name: &str) -> Result<Vec<u8>, AnyErr> {
    // Look for "<name>.part-*-of-*" inside nodes/<port>/
    let dir = format!("nodes/{}", port_str(&node.port));
    let safe_prefix = format!("{}.", sanitize_filename(name));

    let mut rd = fs::read_dir(&dir).await?;
    while let Some(ent) = rd.next_entry().await? {
        let ft = ent.file_type().await?;
        if !ft.is_file() {
            continue;
        }
        let fname = ent.file_name();
        let fname = fname.to_string_lossy();
        if fname.starts_with(&safe_prefix) && fname.contains(".part-") && fname.contains("-of-") {
            let path = ent.path();
            return Ok(fs::read(path).await?);
        }
    }
    Ok(Vec::new())
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

async fn save_into_node_dir(node: &Node, name: &str, data: &[u8]) -> Result<PathBuf, AnyErr> {
    let fname = sanitize_filename(name);
    let path = PathBuf::from(format!("nodes/{}/{}", port_str(&node.port), fname));
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
    addr.split(':').next().unwrap_or("127.0.0.1")
}

/* --- Gossip and Healing Functions --- */

/// The main gossip loop task
async fn spawn_gossip_loop(node: Arc<Node>) {
    loop {
        // Wait for the gossip interval
        tokio::time::sleep(node.gossip_interval).await;

        // Find out who to ping
        let Some(next_addr) = node.get_next().await else {
            println!(
                "[{}] Gossip: No next node set, skipping health check.",
                node.port
            );
            continue;
        };

        println!("[{}] Gossip: Sending PING to {}", node.port, next_addr);
        match check_node_health(node.clone(), &next_addr).await {
            Ok(_) => {
                println!("[{}] Gossip: Received PONG from {}", node.port, next_addr);
            }
            Err(e) => {
                // Health check failed, start the healing process
                eprintln!(
                    "[{}] Gossip: Health check failed for {}: {}",
                    node.port, next_addr, e
                );

                // Start healing in a new task to not block the gossip loop
                let heal_node = node.clone();
                tokio::spawn(async move {
                    let node_port = heal_node.port.clone();
                    if let Err(e) = handle_node_death(heal_node, next_addr).await {
                        eprintln!("[{node_port}] Gossip: Node healing process failed: {e}");
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
    println!(
        "[{}] Starting healing process for node {}",
        node.port, dead_addr
    );
    let dead_port = port_str(&dead_addr).to_string();
    let dead_host = host_of(&dead_addr);

    // 1. Update local map to Dead
    node.update_node_status(dead_port.clone(), crate::NodeStatus::Dead)
        .await;

    // 2. Broadcast change
    println!("[{}] Broadcasting node {} as Dead", node.port, dead_port);
    node.broadcast_netmap_update().await;

    // 3. Start a new process
    println!("[{}] Respawning node at {}", node.port, dead_addr);
    let exe = current_exe()?;

    let mut cmd = Command::new(exe);
    cmd.arg("run")
        .arg("--addr")
        .arg(&dead_addr)
        .arg("--wait-time")
        .arg(node.gossip_interval.as_millis().to_string());

    #[cfg(unix)]
    {
        // use std::os::unix::process::CommandExt as _;
        let _ = cmd.process_group(0);
    }

    // Spawn the child and detach it by just dropping the handle.
    // The process will keep running.
    let _ = cmd.spawn()?;

    // Wait for it to be up
    println!(
        "[{}] Waiting for respawned node {} to listen...",
        node.port, dead_addr
    );
    wait_until_listening(dead_host, dead_port.parse()?, Duration::from_secs(10)).await?;
    println!("[{}] Respawned node {} is up.", node.port, dead_addr);

    // 4. Update map to Alive
    node.update_node_status(dead_port.clone(), crate::NodeStatus::Alive)
        .await;

    // 5. Share shared data
    println!("[{}] Sharing network data with {}", node.port, dead_addr);
    share_data_with_new_node(&node, &dead_addr).await?;

    // 6. Broadcast change (Alive)
    println!("[{}] Broadcasting node {} as Alive", node.port, dead_port);
    node.broadcast_netmap_update().await;

    println!(
        "[{}] Healing process for {} complete.",
        node.port, dead_addr
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
    let next_hop = node.get_next_for_node(port_str(new_node_addr)).await;
    if let Some(next_addr) = next_hop {
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
