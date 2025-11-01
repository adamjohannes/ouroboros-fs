use clap::{Parser, Subcommand};
use ring::run;
use std::{env, error::Error, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::{Child, Command},
    time::sleep,
};

#[derive(Parser)]
#[command(name = "rust_socket_server", version, about = "Ring TCP server & tools")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a single node (the actual server)
    Run {
        /// Optional explicit address (e.g. 127.0.0.1:7000 or just 7000)
        #[arg(short, long)]
        addr: Option<String>,
        /// Convenience: provide only the port; host defaults to 127.0.0.1
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// Spawn N nodes and stitch them into a ring (replacement for run.sh)
    SetNetwork {
        /// Number of nodes to start
        #[arg(short = 'n', long = "nodes", default_value_t = 3)]
        nodes: u16,
        /// Base port to use (ports are base, base+1, ..., base+N-1)
        #[arg(short = 'p', long = "base-port", default_value_t = 7000)]
        base_port: u16,
        /// Host/interface to bind and to use when wiring SET_NEXT
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Do not block; just start and wire nodes, then return
        #[arg(long)]
        no_block: bool,
        /// Extra wait after spawning children before wiring (ms)
        #[arg(long, default_value_t = 200u64)]
        wait_ms: u64,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Run { addr, port } => {
            let addr = resolve_listen_addr(addr, port);
            run(&addr).await
        }
        Cmd::SetNetwork {
            nodes,
            base_port,
            host,
            no_block,
            wait_ms,
        } => set_network(nodes, base_port, &host, !no_block, Duration::from_millis(wait_ms)).await,
    }
}

/* ----------------------------- run ------------------------------ */

fn resolve_listen_addr(addr: Option<String>, port: Option<u16>) -> String {
    // Priority: explicit --addr, then --port, then PORT env, else default.
    if let Some(a) = addr {
        return normalize_addr(a);
    }
    if let Some(p) = port {
        return format!("127.0.0.1:{p}");
    }
    if let Ok(from_env) = env::var("PORT") {
        return normalize_addr(from_env);
    }
    "127.0.0.1:9000".to_string()
}

/// Accept "7001" or "127.0.0.1:7001"
fn normalize_addr(raw: String) -> String {
    if raw.contains(':') {
        raw
    } else {
        format!("127.0.0.1:{raw}")
    }
}

/* -------------------------- set-network -------------------------- */

async fn set_network(
    nodes: u16,
    base_port: u16,
    host: &str,
    block: bool,
    extra_wait: Duration,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if nodes == 0 {
        eprintln!("--nodes must be >= 1");
        return Ok(());
    }

    let exe = current_exe()?;
    println!(
        "Starting {nodes} nodes from {host}:{base_port} using {:?}",
        exe
    );

    // 1) Spawn children
    let mut children: Vec<Child> = Vec::with_capacity(nodes as usize);
    let mut ports: Vec<u16> = Vec::with_capacity(nodes as usize);
    for i in 0..nodes {
        let port = base_port + i;
        let addr = format!("{host}:{port}");
        let mut child = Command::new(&exe)
            .arg("run")
            .arg("--addr")
            .arg(&addr)
            .kill_on_drop(true)
            .spawn()?;
        println!("  - node {i:02} -> {addr} (pid={})", child.id().unwrap_or(0));
        children.push(child);
        ports.push(port);
    }

    // 2) Wait a bit + verify each port is listening
    sleep(extra_wait).await;
    for &port in &ports {
        wait_until_listening(host, port, Duration::from_secs(3)).await?;
    }

    // 3) Wire the ring: i -> (i+1) % N
    for (idx, &src_port) in ports.iter().enumerate() {
        let next_port = ports[(idx + 1) % ports.len()];
        send_set_next(host, src_port, host, next_port).await?;
    }
    println!("Ring stitched: {} nodes [{}…{}]", nodes, ports.first().unwrap(), ports.last().unwrap());

    // 4) Optionally block until 'quit' or Ctrl-C, then kill children
    if block {
        println!("Type 'quit' + Enter or press Ctrl-C to stop all nodes…");
        wait_for_quit_or_ctrl_c().await;
    }

    // 5) Cleanup
    for mut child in children {
        if let Err(e) = child.kill().await {
            // It's fine if it's already gone
            let _ = e;
        }
        let _ = child.wait().await;
    }
    Ok(())
}

fn current_exe() -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    Ok(std::env::current_exe()?)
}

async fn wait_until_listening(
    host: &str,
    port: u16,
    deadline: Duration,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let start = tokio::time::Instant::now();
    let addr = format!("{host}:{port}");
    loop {
        match TcpStream::connect(&addr).await {
            Ok(_) => return Ok(()),
            Err(_) => {
                if start.elapsed() > deadline {
                    return Err(format!("timeout waiting for {addr}").into());
                }
                sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn send_set_next(
    src_host: &str,
    src_port: u16,
    next_host: &str,
    next_port: u16,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let src_addr = format!("{src_host}:{src_port}");
    let next_addr = format!("{next_host}:{next_port}");
    let mut s = TcpStream::connect(&src_addr).await?;
    let line = format!("SET_NEXT {next_addr}\n");
    s.write_all(line.as_bytes()).await?;
    // Best-effort read small response (OK …), but we don't depend on it.
    let mut buf = String::new();
    let mut r = BufReader::new(s);
    // Don't hang: try reading one line with a tiny timeout.
    let read = tokio::time::timeout(Duration::from_millis(150), r.read_line(&mut buf)).await;
    if read.is_err() {
        // ignore slow readers; wiring is fire-and-forget
    }
    Ok(())
}

async fn wait_for_quit_or_ctrl_c() {
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = async {
            while let Ok(Some(line)) = stdin.next_line().await {
                if line.trim().eq_ignore_ascii_case("quit") { break; }
            }
        } => {},
    }
}
