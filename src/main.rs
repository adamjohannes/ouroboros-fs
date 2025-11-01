use std::{env, error::Error, sync::Arc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

#[derive(Debug)]
struct Node {
    /// Where the node is listening
    port: String,
    /// Address of the next node in the ring
    next_port: RwLock<Option<String>>,
}

fn listen_addr() -> String {
    // Accept either "7001" or "127.0.0.1:7001" via CLI arg or PORT env, default to 7001
    let arg = env::args().nth(1);
    let from_env = env::var("PORT").ok();
    let s = arg.or(from_env).unwrap_or_else(|| "9000".to_string());
    if s.contains(':') { s } else { format!("0.0.0.0:{s}") }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let addr = listen_addr();
    let listener = TcpListener::bind(&addr).await?;
    let local = listener.local_addr()?;
    let node = Arc::new(Node {
        port: local.to_string(),
        next_port: RwLock::new(None),
    });

    println!("node listening on {}", local);

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

async fn handle_client(node: Arc<Node>, stream: TcpStream) -> Result<(), Box<dyn Error>> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim_end();

        if let Some(rest) = trimmed.strip_prefix("SET_NEXT ") {
            let addr = rest.trim().to_string();
            {
                let mut lock = node.next_port.write().await;
                *lock = Some(addr.clone());
            }
            writer.write_all(format!("OK next={}\n", addr).as_bytes()).await?;
        } else if trimmed == "GET" {
            let next = node.next_port.read().await.clone();
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
        } else if let Some(rest) = trimmed.strip_prefix("RING ") {
            // Syntax: RING <hops> <message...>
            let mut parts = rest.splitn(2, ' ');
            let hops_str = parts.next().unwrap_or("");
            let msg = parts.next().unwrap_or("").trim();

            let mut hops: u32 = hops_str.parse().unwrap_or(0);
            println!("[{}] RING(hops={}) msg: {}", node.port, hops, msg);

            if hops > 0 {
                hops -= 1;
                if let Some(next_addr) = node.next_port.read().await.clone() {
                    if let Err(e) = forward_ring(&next_addr, hops, msg).await {
                        eprintln!("[{}] forward error to {}: {}", node.port, next_addr, e);
                    }
                } else {
                    eprintln!("[{}] no next node set; dropping", node.port);
                }
            }
            writer.write_all(b"OK\n").await?;
        } else {
            writer.write_all(b"ERR unknown command\n").await?;
        }
    }

    Ok(())
}

async fn forward_ring(addr: &str, hops: u32, msg: &str) -> Result<(), Box<dyn Error>> {
    let mut s = TcpStream::connect(addr).await?;
    let cmd = format!("RING {} {}\n", hops, msg);
    s.write_all(cmd.as_bytes()).await?;
    Ok(())
}
