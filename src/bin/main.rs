use clap::{Parser, Subcommand};
use ring::run;
use std::{env, error::Error, fs, path::Path, path::PathBuf, time::Duration};
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
    /// Run a single node (server)
    Run {
        /// Address to bind (e.g., 127.0.0.1:7000). If omitted, see --port / $PORT / default.
        #[arg(long)]
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
            let bind = resolve_listen_addr(addr, port);
            run(&bind).await
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

/* -------------------------- set-network ------------------------- */

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

    // Prepare a fresh "nodes/" directory each time.
    let nodes_root = Path::new("nodes");
    if nodes_root.exists() {
        fs::remove_dir_all(nodes_root)?;
    }
    fs::create_dir_all(nodes_root)?;

    let exe = current_exe()?;
    println!(
        "starting {nodes} nodes at {host}:{base_port}..{} (exe: {:?})",
        base_port + nodes - 1,
        exe
    );

    // 1) Spawn children
    let mut children: Vec<Child> = Vec::with_capacity(nodes as usize);
    for i in 0..nodes {
        let port = base_port + i;
        let addr = format!("{host}:{port}");
        let mut cmd = Command::new(&exe);
        cmd.arg("run").arg("--addr").arg(&addr);

        #[cfg(unix)]
        {
            // Bring the extension trait into scope but silence "unused import" warnings.
            use std::os::unix::process::CommandExt as _;
            // Best-effort: make children share our process group.
            // (If this API isn't available on your toolchain it's a no-op.)
            let _ = cmd.process_group(0);
        }

        let child = cmd.spawn()?;
        children.push(child);
        println!("spawned node on {addr}");
    }

    // 2) Give them a moment to bind (user-configurable cushion)
    if extra_wait > Duration::from_millis(0) {
        tokio::time::sleep(extra_wait).await;
    }

    // 3) Wait until all ports are listening (bounded wait)
    for i in 0..nodes {
        let port = base_port + i;
        wait_until_listening(host, port, Duration::from_secs(5)).await?;
        println!("node {host}:{port} is listening");
    }

    // 4) Wire the ring via SET_NEXT
    for i in 0..nodes {
        let this_port = base_port + i;
        let next_port = if i + 1 == nodes {
            base_port
        } else {
            base_port + i + 1
        };
        let this_addr = format!("{host}:{this_port}");
        let next_addr = format!("{host}:{next_port}");
        send_set_next(&this_addr, &next_addr).await?;
        println!("wired {this_addr} -> {next_addr}");
    }

    println!("ring wired successfully.");

    // 5) Optionally block until user quits / Ctrl-C
    if block {
        println!("type 'quit' or press Ctrl-C to stop…");
        wait_for_quit_or_ctrl_c().await;
        println!("stopping nodes…");
    }

    // 6) Cleanup
    for mut child in children {
        let _ = child.kill().await;
        let _ = child.wait().await;
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

async fn send_set_next(this_addr: &str, next_addr: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut s = TcpStream::connect(this_addr).await?;
    let line = format!("SET_NEXT {next_addr}\n");
    s.write_all(line.as_bytes()).await?;

    // Best-effort ACK: accept "OK" or "OK <anything>"
    let mut reader = BufReader::new(s);
    let mut buf = String::new();
    let read = tokio::time::timeout(Duration::from_millis(150), reader.read_line(&mut buf)).await;
    if read.is_err() {
        // It's okay if the ACK races; we still consider wiring successful.
        return Ok(());
    }
    let ack = buf.trim();
    let upper = ack.to_ascii_uppercase();
    if !(upper == "OK" || upper.starts_with("OK ")) {
        return Err(format!("unexpected response to SET_NEXT from {this_addr}: {buf}").into());
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
