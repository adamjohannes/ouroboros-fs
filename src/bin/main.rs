use clap::{Parser, Subcommand, ValueEnum};
use ouroboros_fs::{AuthToken, FsyncMode, run};
use serde::Deserialize;
use std::{env, error::Error, fs, path::Path, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::{Child, Command},
    time::sleep,
};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser)]
#[command(name = "ouroboros_fs", version, about = "Ring TCP server & tools")]
struct Cli {
    /// Log format: text (default, human-readable) or json (one event per
    /// line, structured fields). JSON suits Splunk/ELK/Datadog ingestion.
    #[arg(long, value_enum, default_value_t = LogFormat::Text, global = true)]
    log_format: LogFormat,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

/// TOML schema for `--config` on `Cmd::Run`. Every field is `Option<T>` so
/// the file may set any subset; missing fields fall through to built-in
/// defaults. CLI flags override file values. (NEXT_STEPS.md §4.5.)
#[derive(Default, Deserialize)]
struct RunConfig {
    addr: Option<String>,
    wait_time: Option<u64>,
    file_size: Option<u64>,
    storage_root: Option<PathBuf>,
    fsync_mode: Option<CliFsyncMode>,
    auth_token: Option<String>,
    idle_timeout: Option<u64>,
    max_conns: Option<u32>,
    shutdown_timeout: Option<u64>,
}

/// TOML schema for `--config` on `Cmd::Gateway`. The file's top-level
/// table is `[gateway]` so a single config file can describe both run and
/// gateway sections; we only deserialize `[gateway]` here.
#[derive(Default, Deserialize)]
struct GatewayConfig {
    listen: Option<String>,
    #[serde(default)]
    nodes: Vec<String>,
    auth_token: Option<String>,
}

#[derive(Default, Deserialize)]
struct GatewayConfigWrapper {
    #[serde(default)]
    gateway: GatewayConfig,
}

#[derive(Default, Deserialize)]
struct RunConfigWrapper {
    #[serde(default)]
    run: RunConfig,
}

fn load_run_config(path: &Path) -> Result<RunConfig, Box<dyn Error + Send + Sync>> {
    let raw =
        fs::read_to_string(path).map_err(|e| format!("read config {}: {e}", path.display()))?;
    // Accept either `[run]` table or top-level keys for back-compat with
    // simple deployments.
    if raw.contains("[run]") {
        let w: RunConfigWrapper =
            toml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
        Ok(w.run)
    } else {
        let cfg: RunConfig =
            toml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
        Ok(cfg)
    }
}

fn load_gateway_config(path: &Path) -> Result<GatewayConfig, Box<dyn Error + Send + Sync>> {
    let raw =
        fs::read_to_string(path).map_err(|e| format!("read config {}: {e}", path.display()))?;
    if raw.contains("[gateway]") {
        let w: GatewayConfigWrapper =
            toml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
        Ok(w.gateway)
    } else {
        let cfg: GatewayConfig =
            toml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
        Ok(cfg)
    }
}

/// CLI mirror of `FsyncMode` so clap can derive a `--fsync-mode` value parser
/// without adding a `clap` dep to the library crate. Also Deserialize so
/// the same enum works in TOML config files.
#[derive(Copy, Clone, Debug, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CliFsyncMode {
    None,
    Data,
    Full,
}

impl From<CliFsyncMode> for FsyncMode {
    fn from(m: CliFsyncMode) -> Self {
        match m {
            CliFsyncMode::None => FsyncMode::None,
            CliFsyncMode::Data => FsyncMode::Data,
            CliFsyncMode::Full => FsyncMode::Full,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a single node (server). Any flag may also be set via
    /// `--config <toml>`; explicit CLI flags always win.
    Run {
        /// Path to a TOML config file. Provides defaults for any flag
        /// the user doesn't pass on the command line. Built-in defaults
        /// fill in anything the file doesn't set either.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Address to bind. If omitted, see --port, then $PORT, then default.
        #[arg(long)]
        addr: Option<String>,
        /// Provide only the port, and host defaults to 127.0.0.1
        #[arg(short, long)]
        port: Option<u16>,
        /// Time (ms) between health checks to the next node. 0 to disable. Defaults to 5000.
        #[arg(long)]
        wait_time: Option<u64>,
        /// Max file size in bytes. 0 to disable. Defaults to 1_000_000_000.
        #[arg(short, long)]
        file_size: Option<u64>,
        /// Filesystem root under which `<port>/content/` and `<port>/backup/`
        /// directories live. Defaults to `nodes` relative to the cwd. The
        /// healer passes this explicitly when respawning a dead neighbor so
        /// the new process inherits the original on-disk chunks.
        #[arg(long)]
        storage_root: Option<PathBuf>,
        /// Durability of chunk writes: none|data|full. Defaults to full.
        #[arg(long, value_enum)]
        fsync_mode: Option<CliFsyncMode>,
        /// Pre-shared key (64 hex chars / 32 bytes) for the wire-protocol
        /// AUTH handshake. Falls back to the OUROBOROS_AUTH_TOKEN env var
        /// and then to the config file. Disabled if none of those is set.
        #[arg(long)]
        auth_token: Option<String>,
        /// Per-connection idle timeout in seconds. 0 disables. Defaults to 60.
        #[arg(long)]
        idle_timeout: Option<u64>,
        /// Max concurrent client connections. 0 disables. Defaults to 1024.
        #[arg(long)]
        max_conns: Option<u32>,
        /// Graceful-shutdown drain timeout in seconds. Defaults to 30.
        #[arg(long)]
        shutdown_timeout: Option<u64>,
    },

    /// Run a standalone gateway pointed at one or more existing ring
    /// nodes. Use this in production: each ring node is its own systemd
    /// unit, the gateway is its own unit. (See samples/systemd/.)
    Gateway {
        /// Path to a TOML config file. Same precedence rules as `run`.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Address the gateway listens on for HTTP + TCP-proxy clients.
        /// Defaults to 127.0.0.1:8000.
        #[arg(long)]
        listen: Option<String>,
        /// Ring node addresses. Pass multiple `--node` flags. The
        /// gateway tries each in order until one connects.
        #[arg(long = "node", num_args = 1..)]
        nodes: Vec<String>,
        /// Pre-shared bearer/AUTH token (64-char hex). Falls back to the
        /// OUROBOROS_AUTH_TOKEN env var. Disabled if neither is set.
        #[arg(long)]
        auth_token: Option<String>,
    },

    /// Spawn N nodes and stitch them into a ring. Development helper —
    /// for production deploy, use `run` for each node + `gateway` for
    /// the entry point. The legacy `set-network` name still works.
    #[command(alias = "set-network")]
    DevNetwork {
        /// Number of nodes to start
        #[arg(short = 'n', long = "nodes", default_value_t = 3)]
        nodes: u16,
        /// Base port to use (ports are base, base+1, ..., base+N-1)
        #[arg(short = 'p', long = "base-port", default_value_t = 7000)]
        base_port: u16,
        /// Interface to bind and to use when wiring SET_NEXT
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Do not block, just start and wire nodes, then return
        #[arg(long)]
        no_block: bool,
        /// Extra wait after spawning children before wiring (ms)
        #[arg(long, default_value_t = 200u64)]
        wait_ms: u64,
        /// Time (ms) between health checks for each node. 0 to disable.
        #[arg(short = 'w', long = "wait-time", default_value_t = 5000u64)]
        wait_time: u64,
        /// Inform if the "nodes" directory should be reused.
        #[arg(short, long)]
        overwrite_nodes_dir: bool,
        /// Run the DNS Gateway on this port
        #[arg(long = "dns-port")]
        dns_port: Option<u16>,
        /// Max file size in bytes. 0 to disable. Defaults to 1 gigabyte.
        #[arg(short, long, default_value_t = 1_000_000_000u64)]
        file_size: u64,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let cli = Cli::parse();

    // Initialize tracing subscriber. JSON suits log shippers; text is for
    // a human reading `journalctl` or stdout. (NEXT_STEPS.md §4.1.)
    match cli.log_format {
        LogFormat::Text => {
            fmt()
                .with_timer(fmt::time::UtcTime::rfc_3339())
                .with_env_filter(EnvFilter::from_default_env())
                .with_target(true)
                .init();
        }
        LogFormat::Json => {
            fmt()
                .with_timer(fmt::time::UtcTime::rfc_3339())
                .with_env_filter(EnvFilter::from_default_env())
                .with_target(true)
                .json()
                .init();
        }
    }

    match cli.command {
        Cmd::Run {
            config,
            addr,
            port,
            wait_time,
            file_size,
            storage_root,
            fsync_mode,
            auth_token,
            idle_timeout,
            max_conns,
            shutdown_timeout,
        } => {
            // Load config file if --config was passed; otherwise an empty
            // (all-None) struct fills nothing and the built-in defaults
            // apply for everything.
            let cfg: RunConfig = if let Some(p) = &config {
                load_run_config(p)?
            } else {
                RunConfig::default()
            };
            // Precedence: CLI > config > built-in default.
            let addr_or_port = addr.is_some() || port.is_some();
            let bind_str = if addr_or_port {
                resolve_listen_addr(addr, port)
            } else if let Some(a) = cfg.addr.clone() {
                normalize_addr(a)
            } else {
                resolve_listen_addr(None, None) // env or default
            };
            let wait_time = wait_time.or(cfg.wait_time).unwrap_or(5000);
            let file_size = file_size.or(cfg.file_size).unwrap_or(1_000_000_000);
            let storage_root = storage_root
                .or(cfg.storage_root.clone())
                .unwrap_or_else(|| PathBuf::from("nodes"));
            let fsync_mode_cli: CliFsyncMode =
                fsync_mode.or(cfg.fsync_mode).unwrap_or(CliFsyncMode::Full);
            let token_str = auth_token.or(cfg.auth_token.clone());
            let idle_timeout = idle_timeout.or(cfg.idle_timeout).unwrap_or(60);
            let max_conns = max_conns.or(cfg.max_conns).unwrap_or(1024);
            let shutdown_timeout = shutdown_timeout.or(cfg.shutdown_timeout).unwrap_or(30);

            let gossip_interval = Duration::from_millis(wait_time);
            let token = resolve_auth_token(token_str)?;
            run(
                &bind_str,
                gossip_interval,
                file_size,
                storage_root,
                fsync_mode_cli.into(),
                token,
                Duration::from_secs(idle_timeout),
                max_conns,
                Duration::from_secs(shutdown_timeout),
            )
            .await
        }
        Cmd::Gateway {
            config,
            listen,
            nodes,
            auth_token,
        } => {
            let cfg: GatewayConfig = if let Some(p) = &config {
                load_gateway_config(p)?
            } else {
                GatewayConfig::default()
            };
            let listen = listen
                .or(cfg.listen.clone())
                .unwrap_or_else(|| "127.0.0.1:8000".to_string());
            // Nodes: CLI fully replaces config when any are passed (this
            // matches `--node` semantics — clap appends; no easy way to
            // signal "use config-only"). Empty Vec from CLI falls back to
            // config; empty config plus empty CLI errors.
            let node_addrs = if !nodes.is_empty() {
                nodes
            } else if !cfg.nodes.is_empty() {
                cfg.nodes
            } else {
                return Err(
                    "no ring nodes configured: pass --node or set [gateway].nodes in --config"
                        .into(),
                );
            };
            let token = resolve_auth_token(auth_token.or(cfg.auth_token))?;
            let gateway = ouroboros_fs::Gateway::with_auth(node_addrs, token);
            tracing::info!(addr = %listen, "Starting standalone gateway");
            gateway.run_server(listen).await?;
            Ok(())
        }
        Cmd::DevNetwork {
            nodes,
            base_port,
            host,
            no_block,
            wait_ms,
            wait_time,
            overwrite_nodes_dir,
            dns_port,
            file_size,
        } => {
            set_network(
                nodes,
                base_port,
                &host,
                !no_block,
                Duration::from_millis(wait_ms),
                wait_time,
                overwrite_nodes_dir,
                dns_port,
                file_size,
            )
            .await
        }
    }
}

// --- run

fn resolve_listen_addr(addr: Option<String>, port: Option<u16>) -> String {
    // Priority:
    // 1. --addr
    // 2. --port
    // 3. PORT env
    // 4. default
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

/// Resolve the auth token: --auth-token > $OUROBOROS_AUTH_TOKEN > disabled.
/// Disabled auth is documented as development-only; we log a warning so it
/// shows up in production deployments by accident-detection.
fn resolve_auth_token(flag: Option<String>) -> Result<AuthToken, Box<dyn Error + Send + Sync>> {
    let raw = flag.or_else(|| env::var("OUROBOROS_AUTH_TOKEN").ok());
    match raw {
        Some(hex) => Ok(AuthToken::from_hex(&hex)?),
        None => {
            tracing::warn!(
                "No auth token configured (--auth-token / OUROBOROS_AUTH_TOKEN). \
                 Wire-protocol AUTH handshake is DISABLED. Only acceptable for \
                 single-host development."
            );
            Ok(AuthToken::disabled())
        }
    }
}

// --- set-network

#[allow(clippy::too_many_arguments)]
async fn set_network(
    nodes: u16,
    base_port: u16,
    host: &str,
    block: bool,
    extra_wait: Duration,
    wait_time: u64,
    overwrite_nodes_dir: bool,
    dns_port: Option<u16>,
    max_file_size: u64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    if nodes == 0 {
        tracing::warn!("--nodes must be >= 1");
        return Ok(());
    }

    // Make this parent `set-network` process a new process group leader, then
    // all children spawned by it (and their children) will inherit this PGID.
    #[cfg(unix)]
    let pgid = std::process::id();
    #[cfg(unix)]
    unsafe {
        if libc::setpgid(0, 0) == -1 {
            tracing::warn!(
                error = ?std::io::Error::last_os_error(),
                "Could not set process group"
            );
        } else {
            tracing::info!(pgid = %pgid, "Process group leader set");
        }
    }

    // Prepare a fresh "nodes/" directory
    let nodes_root = Path::new("nodes");
    if nodes_root.exists() && overwrite_nodes_dir {
        fs::remove_dir_all(nodes_root)?;
        tracing::info!("Created a fresh 'nodes' directory");
    }
    fs::create_dir_all(nodes_root)?;

    let exe = current_exe()?;
    tracing::info!(
        nodes,
        host,
        base_port,
        end_port = base_port + nodes - 1,
        exe = ?exe,
        "Starting network"
    );

    // 1. Spawn children
    let mut children: Vec<Child> = Vec::with_capacity(nodes as usize);
    for i in 0..nodes {
        let port = base_port + i;
        let addr = format!("{host}:{port}");
        let mut cmd = Command::new(&exe);
        cmd.arg("run")
            .arg("--addr")
            .arg(&addr)
            .arg("--wait-time")
            .arg(wait_time.to_string())
            .arg("--file-size")
            .arg(max_file_size.to_string());

        let child = cmd.spawn()?;
        children.push(child);
        tracing::info!(addr = %addr, "Spawned node");
    }

    // 2. Give nodes a moment to bind
    if extra_wait > Duration::from_millis(0) {
        tokio::time::sleep(extra_wait).await;
    }

    // 3. Wait until all ports are listening
    for i in 0..nodes {
        let port = base_port + i;
        wait_until_listening(host, port, Duration::from_secs(5)).await?;
        tracing::info!(host, port, "Node is listening");
    }

    // 4. Wire the ring
    for i in 0..nodes {
        let this_port = base_port + i;
        let next_port = if i + 1 == nodes {
            base_port
        } else {
            base_port + i + 1
        };
        let this_addr = format!("{host}:{this_port}");
        let next_addr = format!("{host}:{next_port}");
        send_node_next(&this_addr, &next_addr).await?;
        tracing::info!(from = %this_addr, to = %next_addr, "Wired node");
    }

    tracing::info!("Ring wired successfully.");

    // 5. Start the DNS Gateway if requested
    if let Some(port) = dns_port {
        // Create the list of all node addresses
        let node_addrs: Vec<String> = (0..nodes)
            .map(|i| format!("{}:{}", host, base_port + i))
            .collect();

        let gateway = ouroboros_fs::Gateway::new(node_addrs);

        // Spawn the main gateway server
        let server_gateway = Arc::clone(&gateway);
        let dns_listen_addr = format!("{}:{}", host, port);
        tokio::spawn(async move {
            if let Err(e) = server_gateway.run_server(dns_listen_addr).await {
                tracing::error!(error = ?e, "Gateway server failed");
            }
        });
    }

    // 6. Start a full investigation from the first node
    let start_addr = format!("{host}:{base_port}");
    if let Err(e) = send_netmap_discover(&start_addr).await {
        tracing::warn!(start_addr = %start_addr, error = ?e, "Failed to start netmap discover");
    } else {
        tracing::info!(start_addr = %start_addr, "Started netmap discover");
    }

    // 7. Start a topology walk to populate topology maps
    if let Err(e) = send_topology_walk(&start_addr).await {
        tracing::warn!(start_addr = %start_addr, error = ?e, "Failed to start topology walk");
    } else {
        tracing::info!(start_addr = %start_addr, "Started topology walk");
    }

    // 8. Optionally block until user quits / Ctrl-C
    if block {
        tracing::info!("Type 'quit' or press Ctrl-C to stop…");
        wait_for_quit_or_ctrl_c().await;
        tracing::info!("Stopping nodes…");
    }

    // 9. Cleanup
    #[cfg(unix)]
    {
        tracing::info!(pgid = %pgid, "Stopping process group");
        // Send SIGTERM to the entire process group
        unsafe {
            libc::kill(-(pgid as i32), libc::SIGTERM);
        }
        // Wait for all children we know about to exit
        for mut child in children {
            let _ = child.wait().await;
        }
    }
    #[cfg(not(unix))]
    {
        // Fallback for non-Unix (Windows)
        for mut child in children {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
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
                    return Err(format!("timed out while waiting for {addr}").into());
                }
                sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn send_node_next(
    this_addr: &str,
    next_addr: &str,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut s = TcpStream::connect(this_addr).await?;
    let line = format!("NODE NEXT {next_addr}\n");
    s.write_all(line.as_bytes()).await?;

    // Accept "OK" or "OK <anything>"
    let mut reader = BufReader::new(s);
    let mut buf = String::new();
    let read = tokio::time::timeout(Duration::from_millis(150), reader.read_line(&mut buf)).await;
    if read.is_err() {
        // It's okay if the ACK races, we still consider the wiring successful
        return Ok(());
    }
    let ack = buf.trim();
    let upper = ack.to_ascii_uppercase();
    if !(upper == "OK" || upper.starts_with("OK ")) {
        return Err(format!("unexpected response to NODE NEXT from {this_addr}: {buf}").into());
    }
    Ok(())
}

async fn send_netmap_discover(start_addr: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut s = TcpStream::connect(start_addr).await?;
    s.write_all(b"NETMAP DISCOVER\n").await?;
    let mut reader = BufReader::new(s);
    let mut buf = String::new();
    let _ = tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut buf)).await;
    Ok(())
}

async fn send_topology_walk(start_addr: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut s = TcpStream::connect(start_addr).await?;
    s.write_all(b"TOPOLOGY WALK\n").await?;
    let mut reader = BufReader::new(s);
    let mut buf = String::new();
    let _ = tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut buf)).await;
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
