use std::{error, sync::Arc, time::Duration, path::PathBuf};
use std::path::Path;
use tokio::fs;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader
};
use tokio::net::{TcpListener, TcpStream};

use crate::{
    node::{port_str, append_edge, Node},
    protocol::{self, Command},
};

type AnyErr = Box<dyn error::Error + Send + Sync>;

/// Run the TCP server and handle connections.
pub async fn run(bind_addr: &str) -> Result<(), AnyErr> {
    let listener = TcpListener::bind(bind_addr).await?;
    let local = listener.local_addr()?;
    let node = Node::new(local.to_string());
    println!("node listening on {}", node.port);

    // Create nodes/<port> directory once this node is up
    let port_only = port_str(&node.port);
    let node_dir = format!("nodes/{}", port_only);
    if let Err(e) = fs::create_dir_all(&node_dir).await {
        eprintln!("[{}] failed to create {}: {}", node.port, node_dir, e);
    } else {
        println!("[{}] created {}", node.port, node_dir);
    }

    loop {
        let (stream, peer) = listener.accept().await?;
        let node = Arc::clone(&node);
        tokio::spawn(async move {
            if let Err(e) = handle_client(node, stream).await {
                eprintln!("client {peer}: error: {e}");
            }
        });
    }
}

async fn handle_client(node: Arc<Node>, stream: TcpStream) -> Result<(), AnyErr> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }

        match protocol::parse_line(&line) {
            Ok(cmd) => match cmd {
                Command::SetNext(addr) => handle_set_next(&node, &mut writer, addr).await?,
                Command::Get => handle_get(&node, &mut writer).await?,
                Command::Ring { ttl, msg } => handle_ring(&node, &mut writer, ttl, msg).await?,

                // WALK
                Command::WalkStart => handle_walk_start(&node, &mut writer).await?,
                Command::WalkHop { token, start_addr, history } =>
                    handle_walk_hop(&node, &mut writer, token, start_addr, history).await?,
                Command::WalkDone { token, history } =>
                    handle_walk_done(&node, &mut writer, token, history).await?,

                // INVESTIGATION / NETMAP
                Command::InvestigateStart =>
                    handle_investigate_start(&node, &mut writer).await?,
                Command::InvestigateHop { token, start_addr, entries } =>
                    handle_investigate_hop(&node, &mut writer, token, start_addr, entries).await?,
                Command::InvestigateDone { token, entries } =>
                    handle_investigate_done(&node, &mut writer, token, entries).await?,
                Command::NetmapSet { entries } =>
                    handle_netmap_set(&node, &mut writer, entries).await?,
                Command::NetmapGet =>
                    handle_netmap_get(&node, &mut writer).await?,

                // FILE
                Command::PushFile { size, name } =>
                    handle_push_file(&node, &mut reader, &mut writer, size, name).await?,
                Command::FileHop { token, start_addr, size, name } =>
                    handle_file_hop(&node, &mut reader, &mut writer, token, start_addr, size, name).await?,

                // LIST (local)
                Command::ListFiles =>
                    handle_list_files(&node, &mut writer).await?,
            },
            Err(e) => handle_error(&mut writer, e).await?,
        }
    }

    Ok(())
}

/* --- Command handlers --- */

async fn handle_set_next<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    addr: String,
) -> Result<(), AnyErr> {
    node.set_next(addr.clone()).await;
    writer.write_all(format!("OK next={}\n", addr).as_bytes()).await?;
    Ok(())
}

async fn handle_get<W: AsyncWrite + Unpin>(node: &Node, writer: &mut W) -> Result<(), AnyErr> {
    let next = node.get_next().await.unwrap_or_else(|| "<unset>".to_string());
    writer
        .write_all(format!("PORT {}\nNEXT {}\nOK\n", node.port, next).as_bytes())
        .await?;
    Ok(())
}

async fn handle_ring<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    mut ttl: u32,
    msg: String,
) -> Result<(), AnyErr> {
    println!("[{}] RING(ttl={}) msg: {}", node.port, ttl, msg);

    if ttl > 0 {
        ttl -= 1;
        if let Some(next_addr) = node.get_next().await {
            if let Err(e) = node.forward_ring(ttl, &msg).await {
                eprintln!("[{}] forward error to {}: {}", node.port, next_addr, e);
            }
        } else {
            eprintln!("[{}] no next node set; dropping", node.port);
        }
    }

    writer.write_all(b"OK\n").await?;
    Ok(())
}

/// Handle "WALK" from the client on the start node.
async fn handle_walk_start<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
) -> Result<(), AnyErr> {
    let token = node.make_walk_token();
    let rx = node.register_walk(token.as_str()).await;

    let Some(history) = node.first_walk_history().await else {
        writer.write_all(b"ERR no next hop set\n").await?;
        return Ok(());
    };

    if let Err(e) = node.forward_walk_hop(&token, &node.port, &history).await {
        writer.write_all(format!("ERR forward failed: {e}\n").as_bytes()).await?;
        return Ok(());
    }

    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(final_history)) => {
            for seg in final_history.split(';').filter(|s| !s.is_empty()) {
                writer.write_all(format!("{seg}\n").as_bytes()).await?;
            }
            writer.write_all(b"OK\n").await?;
        }
        Ok(Err(_)) => { writer.write_all(b"ERR walk canceled\n").await?; }
        Err(_) => { writer.write_all(b"ERR walk timeout\n").await?; }
    }

    Ok(())
}

async fn handle_walk_hop<W: AsyncWrite + Unpin>(
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
        if let Err(e) = node.send_walk_done(&start_addr, &token, &new_history).await {
            eprintln!("[{}] WALK DONE send failed to {}: {}", node.port, start_addr, e);
        }
    } else {
        if let Err(e) = node.forward_walk_hop(&token, &start_addr, &new_history).await {
            eprintln!("[{}] WALK forward failed to {}: {}", node.port, next_addr, e);
        }
    }

    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

async fn handle_walk_done<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    token: String,
    history: String,
) -> Result<(), AnyErr> {
    let _ = node.finish_walk(&token, history).await;
    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

/* -------- INVESTIGATION / NETMAP -------- */

async fn handle_investigate_start<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
) -> Result<(), AnyErr> {
    let token = node.make_invest_token();

    let Some(_next) = node.get_next().await else {
        writer.write_all(b"ERR no next hop set\n").await?;
        return Ok(());
    };

    // entries begins with "my_port=Alive"
    let entries = format!("{}=Alive", port_str(&node.port));
    if let Err(e) = node.forward_invest_hop(&token, &node.port, &entries).await {
        writer.write_all(format!("ERR forward failed: {e}\n").as_bytes()).await?;
        return Ok(());
    }

    // We don't need to wait here; it's a background ring discovery.
    writer.write_all(b"OK\n").await?;
    Ok(())
}

async fn handle_investigate_hop<W: AsyncWrite + Unpin>(
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
        if let Err(e) = node.send_invest_done(&start_addr, &token, &new_entries).await {
            eprintln!("[{}] INVEST DONE send failed to {}: {}", node.port, start_addr, e);
        }
    } else {
        if let Err(e) = node.forward_invest_hop(&token, &start_addr, &new_entries).await {
            eprintln!("[{}] INVEST forward failed to {}: {}", node.port, next_addr, e);
        }
    }

    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

async fn handle_investigate_done<W: AsyncWrite + Unpin>(
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

/* -------- FILE: PUSH / HOP handlers -------- */

async fn handle_push_file<R, W>(
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
    let name = Path::new(&name).file_name().unwrap().to_str().unwrap();
    // Ensure we have a next hop (so the ring is valid)
    let Some(_next) = node.get_next().await else {
        writer.write_all(b"ERR no next hop set\n").await?;
        // Drain bytes to keep connection sane
        let mut sink = vec![0u8; size as usize];
        reader.read_exact(&mut sink).await?;
        return Ok(());
    };

    // Read exactly `size` bytes from client
    let mut buf = vec![0u8; size as usize];
    reader.read_exact(&mut buf).await?;

    // Store locally under nodes/<port>/<name>
    let saved_as = save_into_node_dir(node, &name, &buf).await?;

    // Prepare token & waiter
    let token = node.make_file_token();
    let rx = node.register_file(&token).await;

    // Forward around the ring
    if let Err(e) = node.forward_file_hop(&token, &node.port, size, &name, &buf).await {
        writer.write_all(format!("ERR forward failed: {e}\n").as_bytes()).await?;
        return Ok(());
    }

    // Wait up to 60s for loop completion (file returns to start)
    match tokio::time::timeout(Duration::from_secs(60), rx).await {
        Ok(Ok(())) => {
            writer
                .write_all(
                    format!("FILE {} bytes '{}' shared with all nodes\nOK\n", size, name)
                        .as_bytes(),
                )
                .await?;
            println!("[{}] file shared: {} ({} bytes) -> {}", node.port, name, size, saved_as.display());
        }
        Ok(Err(_)) => { writer.write_all(b"ERR file share canceled\n").await?; }
        Err(_) => { writer.write_all(b"ERR file share timeout\n").await?; }
    }

    Ok(())
}

async fn handle_file_hop<R, W>(
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

    // Save locally (best-effort)
    if let Err(e) = save_into_node_dir(node, &name, &buf).await {
        eprintln!("[{}] failed to save file '{}': {}", node.port, name, e);
    }

    // Forward to next
    if let Some(_) = node.get_next().await {
        if let Err(e) = node.forward_file_hop(&token, &start_addr, size, &name, &buf).await {
            eprintln!("[{}] FILE forward failed: {}", node.port, e);
        }
    }

    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

/* -------- LIST_FILES: local-only listing -------- */

async fn handle_list_files<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
) -> Result<(), AnyErr> {
    let files = list_local_files(node).await?;
    if files.is_empty() {
        writer.write_all(b"(empty)\n").await?;
    } else {
        for f in files {
            writer.write_all(format!("{}\n", f).as_bytes()).await?;
        }
    }
    writer.write_all(b"OK\n").await?;
    Ok(())
}

/* --- Helpers & Errors --- */

async fn handle_error<W: AsyncWrite + Unpin>(writer: &mut W, err: String) -> Result<(), AnyErr> {
    writer.write_all(format!("ERR {}\n", err).as_bytes()).await?;
    Ok(())
}

fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        let bad = ch == '/' || ch == '\\' || ch == '\0' || ch == ':' || ch == '|' || ch == ';' || ch == '\n' || ch == '\r';
        if bad { out.push('_'); } else { out.push(ch); }
    }
    if out.is_empty() { "_".into() } else { out }
}

async fn save_into_node_dir(node: &Node, name: &str, data: &[u8]) -> Result<PathBuf, AnyErr> {
    let fname = sanitize_filename(name);
    let path = PathBuf::from(format!("nodes/{}/{}", port_str(&node.port), fname));
    fs::write(&path, data).await?;
    Ok(path)
}

async fn list_local_files(node: &Node) -> Result<Vec<String>, AnyErr> {
    let dir = format!("nodes/{}", port_str(&node.port));
    let mut out = Vec::new();
    let mut rd = match fs::read_dir(&dir).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[{}] read_dir {} failed: {}", node.port, dir, e);
            return Ok(out);
        }
    };

    while let Ok(Some(ent)) = rd.next_entry().await {
        let ty = ent.file_type().await;
        if let Ok(t) = ty {
            if t.is_file() {
                if let Some(name) = ent.file_name().to_str() {
                    out.push(name.to_string());
                }
            }
        }
    }

    out.sort_unstable();
    Ok(out)
}
