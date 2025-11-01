use std::{error, sync::Arc, time::Duration};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use crate::{
    node::{port_str, append_edge, Node},
    protocol::{self, Command},
};

type AnyErr = Box<dyn error::Error + Send + Sync>;

/// Run the TCP server and handle connections.
pub async fn run(bind_addr: &str) -> Result<(), AnyErr> {
    // 1. Bind to the port using TCP
    let listener = TcpListener::bind(bind_addr).await?;

    // 2. Get the final node address
    let local = listener.local_addr()?;

    // 3. Initialize the node
    let node = Node::new(local.to_string());
    println!("node listening on {}", node.port);

    // (NEW) 4. Create nodes/<port> directory once this node is up
    let port_only = port_str(&node.port);
    let node_dir = format!("nodes/{}", port_only);
    if let Err(e) = fs::create_dir_all(&node_dir).await {
        eprintln!("[{}] failed to create {}: {}", node.port, node_dir, e);
    } else {
        println!("[{}] created {}", node.port, node_dir);
    }

    // 5. Accept & serve forever
    loop {
        let (stream, peer) = listener.accept().await?;

        // Clone the node so it can be used to run the routines
        let node = Arc::clone(&node);

        // Handle the client asynchronously
        tokio::spawn(async move {
            if let Err(e) = handle_client(node, stream).await {
                eprintln!("client {peer}: error: {e}");
            }
        });
    }
}

async fn handle_client(node: Arc<Node>, stream: TcpStream) -> Result<(), AnyErr> {
    let (reader, mut writer) = stream.into_split();

    // Get the message coming from the client
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        // Read the first line of the message
        if reader.read_line(&mut line).await? == 0 {
            break;
        }

        // Handle the command
        match protocol::parse_line(&line) {
            Ok(cmd) => match cmd {
                Command::SetNext(addr) => handle_set_next(&node, &mut writer, addr).await?,
                Command::Get => handle_get(&node, &mut writer).await?,
                Command::Ring { ttl, msg } => handle_ring(&node, &mut writer, ttl, msg).await?,

                // WALK commands
                Command::WalkStart => handle_walk_start(&node, &mut writer).await?,
                Command::WalkHop {
                    token,
                    start_addr,
                    history,
                } => handle_walk_hop(&node, &mut writer, token, start_addr, history).await?,
                Command::WalkDone { token, history } => {
                    handle_walk_done(&node, &mut writer, token, history).await?
                }
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
    writer
        .write_all(format!("OK next={}\n", addr).as_bytes())
        .await?;
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

    // TTL check
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

    // Reply to client
    writer.write_all(b"OK\n").await?;
    Ok(())
}

/// Handle "WALK" from the client on the start node.
async fn handle_walk_start<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
) -> Result<(), AnyErr> {
    // 1) Build a token & register an oneshot waiter for completion
    let token = node.make_walk_token();
    let rx = node.register_walk(token.as_str()).await;

    // 2) Start the history with "self->next"
    let Some(history) = node.first_walk_history().await else {
        writer.write_all(b"ERR no next hop set\n").await?;
        return Ok(());
    };

    // 3) Forward the hop
    if let Err(e) = node.forward_walk_hop(&token, &node.port, &history).await {
        writer
            .write_all(format!("ERR forward failed: {e}\n").as_bytes())
            .await?;
        return Ok(());
    }

    // 4) Wait up to 30s for completion
    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(final_history)) => {
            for seg in final_history.split(';').filter(|s| !s.is_empty()) {
                writer.write_all(format!("{seg}\n").as_bytes()).await?;
            }
            writer.write_all(b"OK\n").await?;
        }
        Ok(Err(_canceled)) => {
            writer.write_all(b"ERR walk canceled\n").await?;
        }
        Err(_elapsed) => {
            writer.write_all(b"ERR walk timeout\n").await?;
        }
    }

    Ok(())
}

/// Handle an in-flight "WALK HOP ..." arriving from the previous node.
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
        if let Err(e) = node
            .forward_walk_hop(&token, &start_addr, &new_history)
            .await
        {
            eprintln!("[{}] WALK forward failed to {}: {}", node.port, next_addr, e);
        }
    }

    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

/// Handle "WALK DONE ..." arriving at the start node.
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

/* --- Errors --- */

async fn handle_error<W: AsyncWrite + Unpin>(writer: &mut W, err: String) -> Result<(), AnyErr> {
    writer
        .write_all(format!("ERR {}\n", err).as_bytes())
        .await?;
    Ok(())
}
