use std::{error, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use crate::{
    node::{Node, append_edge},
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

    loop {
        // 4. Accept messages on the bound port
        let (stream, peer) = listener.accept().await?;

        // 5. Clone the node so it can be used to run the routines
        //    - This ensures that the borrow checker won't cause compilation errors
        let node = Arc::clone(&node);

        // 6. Handle the client asynchronously
        tokio::spawn(async move {
            if let Err(e) = handle_client(node, stream).await {
                eprintln!("client {peer}: error: {e}");
            }
        });
    }
}

async fn handle_client(node: Arc<Node>, stream: TcpStream) -> Result<(), AnyErr> {
    // 1. Get reader and writer streams
    let (reader, mut writer) = stream.into_split();

    // 2. Get the message coming from the client
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        // 3. Read the first line of the message
        if reader.read_line(&mut line).await? == 0 {
            break;
        }

        // 4. Handle the command
        match protocol::parse_line(&line) {
            Ok(cmd) => match cmd {
                Command::SetNext(addr) => handle_set_next(&node, &mut writer, addr).await?,
                Command::Get => handle_get(&node, &mut writer).await?,
                Command::Ring { ttl, msg } => handle_ring(&node, &mut writer, ttl, msg).await?,

                // New WALK commands
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
    // 1. Update the node's "next_port" value
    node.set_next(addr.clone()).await;

    // 2. Reply to the client informing the value was updated
    writer
        .write_all(format!("OK next={}\n", addr).as_bytes())
        .await?;
    Ok(())
}

async fn handle_get<W: AsyncWrite + Unpin>(node: &Node, writer: &mut W) -> Result<(), AnyErr> {
    // 1. Get the node's "next_port" value
    let next = node.get_next().await;

    // 2. Reply to the client informing the node's port and "next_port"
    writer
        .write_all(
            format!(
                "PORT {}\nNEXT {}\n",
                node.port,
                next.as_deref().unwrap_or("<unset>")
            )
            .as_bytes(),
        )
        .await?;
    writer.write_all(b"OK\n").await?;
    Ok(())
}

async fn handle_ring<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    mut ttl: u32,
    msg: String,
) -> Result<(), AnyErr> {
    println!("[{}] RING(ttl={}) msg: {}", node.port, ttl, msg);

    // 1. Check if the TTL expired
    if ttl > 0 {
        // 1.1. Reduce TTL
        ttl -= 1;

        // 1.2. Forward message if we have a next hop
        if let Some(next_addr) = node.get_next().await {
            if let Err(e) = node.forward_ring(ttl, &msg).await {
                eprintln!("[{}] forward error to {}: {}", node.port, next_addr, e);
            }
        } else {
            eprintln!("[{}] no next node set; dropping", node.port);
        }
    }

    // 2. Reply to client
    writer.write_all(b"OK\n").await?;
    Ok(())
}

/// Handle "WALK" from the client on the start node.
async fn handle_walk_start<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
) -> Result<(), AnyErr> {
    // 1. Require a next hop; otherwise there is no ring to walk.
    let Some(next_addr) = node.get_next().await else {
        return handle_error(writer, "next not set".into()).await;
    };

    // 2. Create a unique token that identifies this specific walk request.
    let token = node.make_walk_token();

    // 3. Start the on-wire history with this node -> next hop.
    let mut history = String::new();
    history = append_edge(history, &node.port, &next_addr);

    // 4. Register this WALK so we can await completion on this connection.
    let rx = node.register_walk(&token).await;

    // 5. Kick off the walk by forwarding to the next node.
    let start_addr = node.port.clone();
    if let Err(e) = node.forward_walk_hop(&token, &start_addr, &history).await {
        // 5.1 If it fails to forward, reply with an error.
        let _ = node
            .finish_walk(&token, format!("ERR forward: {}", e))
            .await;
        return handle_error(writer, format!("walk forward failed: {e}")).await;
    }

    // 6. Wait for completion (the last hop will send "WALK DONE" to the start node).
    match timeout(Duration::from_secs(30), rx).await {
        // 6.1 Success: render semicolon-separated single line into multi-line for the user
        Ok(Ok(final_history_single_line)) => {
            let printable = final_history_single_line.replace(';', "\n");
            writer.write_all(printable.as_bytes()).await?;
            if !printable.ends_with('\n') {
                writer.write_all(b"\n").await?;
            }
            writer.write_all(b"OK\n").await?;
        }
        // 6.2 The oneshot was dropped (unlikely) — surface a clear error
        Ok(Err(_canceled)) => {
            handle_error(writer, "walk canceled".into()).await?;
        }
        // 6.3 Timeout waiting for the loop to close — avoid hanging the client forever
        Err(_elapsed) => {
            handle_error(writer, "walk timeout".into()).await?;
        }
    }
    Ok(())
}

/// Handle "WALK HOP ..." coming from the previous node.
async fn handle_walk_hop<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    token: String,
    start_addr: String,
    history: String, // semicolon-separated single line
) -> Result<(), AnyErr> {
    // 1. Fetch our next hop. If we don't have one, we cannot proceed.
    let Some(next_addr) = node.get_next().await else {
        // 1.1. Acknowledge and return; the start node will eventually time out.
        let _ = writer.write_all(b"OK\n").await; // ignore potential EPIPE
        return Ok(());
    };

    // 2. Append our edge "this->next" to the single-line history (with ';').
    let new_history = append_edge(history, &node.port, &next_addr);

    // 3. If the next hop is the start node, we close the loop by sending "WALK DONE".
    if next_addr == start_addr {
        if let Err(e) = node.send_walk_done(&start_addr, &token, &new_history).await {
            eprintln!("[{}] WALK DONE send failed: {}", node.port, e);
        }
    } else {
        // 4. Otherwise forward to the next node.
        if let Err(e) = node
            .forward_walk_hop(&token, &start_addr, &new_history)
            .await
        {
            eprintln!(
                "[{}] WALK forward failed to {}: {}",
                node.port, next_addr, e
            );
        }
    }

    // 5. Best-effort ACK to the previous node (ignore errors if peer closed early).
    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

/// Handle "WALK DONE ..." arriving at the start node.
async fn handle_walk_done<W: AsyncWrite + Unpin>(
    node: &Node,
    writer: &mut W,
    token: String,
    history: String, // semicolon-separated
) -> Result<(), AnyErr> {
    // 1. Try to deliver the final history to whoever is waiting on this token.
    //    If there is no waiter, this node wasn't the start — we just ignore.
    let _delivered = node.finish_walk(&token, history).await;

    // 2. Optional ACK (best-effort; the peer might already be gone).
    let _ = writer.write_all(b"OK\n").await;
    Ok(())
}

/* --- Errors --- */

/// Send a protocol error back to the client (single line).
async fn handle_error<W: AsyncWrite + Unpin>(writer: &mut W, err: String) -> Result<(), AnyErr> {
    writer
        .write_all(format!("ERR {}\n", err).as_bytes())
        .await?;
    Ok(())
}
