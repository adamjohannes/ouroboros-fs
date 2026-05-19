//! In-process test harness for OuroborosFS.
//!
//! Spawns N nodes in the same tokio runtime, wires them into a ring, and
//! exposes thin client helpers for `FILE PUSH` / `FILE PULL`. Each `Ring`
//! owns a `TempDir`; concurrent test runs never collide on disk or ports.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ouroboros_fs::{Node, bind, serve};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_chacha::ChaCha20Rng;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

pub struct NodeHandle {
    pub addr: SocketAddr,
    pub node: Arc<Node>,
    pub serve: JoinHandle<()>,
}

pub struct Ring {
    pub nodes: Vec<NodeHandle>,
    _tmp: TempDir,
}

impl Ring {
    pub fn addr(&self, idx: usize) -> SocketAddr {
        self.nodes[idx].addr
    }
}

#[derive(Clone)]
pub struct RingOpts {
    pub n: usize,
    pub gossip_interval: Duration,
    pub max_file_size: u64,
}

impl Default for RingOpts {
    fn default() -> Self {
        Self {
            n: 3,
            // Gossip disabled by default — failover tests opt in.
            gossip_interval: Duration::ZERO,
            max_file_size: 1 << 30,
        }
    }
}

/// Spin up an `n`-node ring on ephemeral ports under a fresh tempdir.
///
/// Order matters: bind every node first (so all addresses are known), spawn
/// serve loops next, *then* wire `NODE NEXT`. Wiring before all serve loops
/// are running would race the accept queue.
pub async fn spin_up(opts: RingOpts) -> Ring {
    assert!(opts.n >= 1);
    let tmp = TempDir::new().expect("tempdir");

    // 1. Bind each node and capture its OS-assigned address.
    let mut bound = Vec::with_capacity(opts.n);
    for i in 0..opts.n {
        let storage = tmp.path().join(format!("ring-{i}"));
        let (node, listener, addr) = bind(
            "127.0.0.1:0",
            opts.gossip_interval,
            opts.max_file_size,
            storage,
            /*respawn_dead=*/ false,
        )
        .await
        .expect("bind");
        bound.push((node, listener, addr));
    }

    // 2. Spawn serve tasks. The accept queue (1024) buffers wiring requests
    //    that arrive before the spawned task is scheduled.
    let mut nodes = Vec::with_capacity(opts.n);
    for (node, listener, addr) in bound {
        let node_for_serve = Arc::clone(&node);
        let serve_task = tokio::spawn(async move {
            serve(node_for_serve, listener).await;
        });
        nodes.push(NodeHandle {
            addr,
            node,
            serve: serve_task,
        });
    }

    // 3. Wire `NODE NEXT` around the ring (i -> i+1, last -> 0).
    if opts.n > 1 {
        for i in 0..opts.n {
            let from = nodes[i].addr;
            let to = nodes[(i + 1) % opts.n].addr;
            send_node_next(from, to).await.expect("NODE NEXT");
        }
    } else {
        // Single-node ring still needs a next hop; point it at itself.
        send_node_next(nodes[0].addr, nodes[0].addr)
            .await
            .expect("NODE NEXT (self)");
    }

    // 4. Trigger NETMAP DISCOVER from node 0; poll every node's network_size
    //    until it reflects the full ring (or a deadline elapses).
    fire_and_forget(nodes[0].addr, b"NETMAP DISCOVER\n")
        .await
        .expect("NETMAP DISCOVER");
    poll_until(Duration::from_secs(3), || async {
        for h in &nodes {
            if h.node.network_size().await < opts.n {
                return false;
            }
        }
        true
    })
    .await
    .expect("netmap converged");

    // 5. Trigger TOPOLOGY WALK; poll topology_map until it has every edge.
    fire_and_forget(nodes[0].addr, b"TOPOLOGY WALK\n")
        .await
        .expect("TOPOLOGY WALK");
    poll_until(Duration::from_secs(3), || async {
        for h in &nodes {
            if h.node.topology_map.read().await.len() < opts.n {
                return false;
            }
        }
        true
    })
    .await
    .expect("topology converged");

    Ring { nodes, _tmp: tmp }
}

/// Abort every serve task. The `TempDir` is dropped when `Ring` goes out of
/// scope; callers can also call this explicitly for orderly teardown.
pub async fn shutdown(ring: Ring) {
    for h in &ring.nodes {
        h.serve.abort();
    }
    drop(ring);
}

/// "Kill" a node by aborting its serve task. The listener is dropped and the
/// socket closed; later connections to that address get ECONNREFUSED.
/// `respawn_dead=false` prevents the rest of the ring from exec'ing a binary
/// to bring it back.
pub async fn kill_node(ring: &mut Ring, idx: usize) {
    ring.nodes[idx].serve.abort();
    // Yield so the abort completes before the test pulls.
    tokio::task::yield_now().await;
}

// ---------- client helpers ----------

/// Push `bytes` to `addr` as a `FILE PUSH`. Returns `Ok(())` on receipt of
/// any non-error response (the server sends a textual ACK ending in `OK`).
///
/// **Note:** the start node ACKs the client as soon as it has streamed bytes
/// to its next hop, which is *before* every downstream relay has actually
/// saved its chunk. Under load this means a fast-following PULL can race
/// the relay and observe missing chunks. We sleep briefly after the ACK to
/// let the relay catch up. PR7 (fan-out + N-ACK synchronization) will make
/// this unnecessary; until then the harness papers over the documented
/// async-relay race so tests stay deterministic.
pub async fn push_bytes(addr: SocketAddr, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let mut s = TcpStream::connect(addr).await?;
    let header = format!("FILE PUSH {} {}\n", bytes.len(), name);
    s.write_all(header.as_bytes()).await?;
    s.write_all(bytes).await?;
    s.shutdown().await.ok();

    let mut resp = String::new();
    s.read_to_string(&mut resp).await?;
    if resp.starts_with("ERR") {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, resp));
    }

    // Settle window: relay completes asynchronously past the start node's ACK.
    // Under PR4's parallel pull, even brief delays here matter — the pull
    // can issue all chunk fetches at once, racing the still-in-flight
    // relay. PR7's fan-out + N-ACK synchronization eliminates the gap.
    sleep(Duration::from_millis(300)).await;
    Ok(())
}

/// Pull a file's bytes from `addr`. Server responds with raw bytes until EOF;
/// any `ERR ...` response is detected via the (very rare) case where the
/// first 4 bytes spell `ERR `.
pub async fn pull_bytes(addr: SocketAddr, name: &str) -> std::io::Result<Vec<u8>> {
    let mut s = TcpStream::connect(addr).await?;
    let header = format!("FILE PULL {}\n", name);
    s.write_all(header.as_bytes()).await?;
    s.shutdown().await.ok();

    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await?;
    if buf.starts_with(b"ERR ") {
        let msg = String::from_utf8_lossy(&buf).to_string();
        return Err(std::io::Error::new(std::io::ErrorKind::Other, msg));
    }
    Ok(buf)
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

pub fn rand_bytes(seed: u64, len: usize) -> Vec<u8> {
    use rand::RngCore;
    let mut rng = ChaCha20Rng::seed_from_u64(seed);
    let mut out = vec![0u8; len];
    rng.fill_bytes(&mut out);
    out
}

// Suppress unused-import warning for StdRng on platforms that don't need it.
#[allow(dead_code)]
fn _stdrng_marker() -> StdRng {
    StdRng::seed_from_u64(0)
}

// ---------- internal ----------

async fn send_node_next(from: SocketAddr, to: SocketAddr) -> std::io::Result<()> {
    let mut s = TcpStream::connect(from).await?;
    let line = format!("NODE NEXT {to}\n");
    s.write_all(line.as_bytes()).await?;
    let mut reader = BufReader::new(s);
    let mut buf = String::new();
    let _ = tokio::time::timeout(
        Duration::from_millis(500),
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut buf),
    )
    .await;
    Ok(())
}

async fn fire_and_forget(addr: SocketAddr, line: &[u8]) -> std::io::Result<()> {
    let mut s = TcpStream::connect(addr).await?;
    s.write_all(line).await?;
    let _ = tokio::time::timeout(Duration::from_millis(100), async {
        let mut tmp = [0u8; 64];
        let _ = s.read(&mut tmp).await;
    })
    .await;
    Ok(())
}

/// Poll `cond` every 25 ms until it returns true or `deadline` elapses.
async fn poll_until<F, Fut>(deadline: Duration, mut cond: F) -> Result<(), &'static str>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    loop {
        if cond().await {
            return Ok(());
        }
        if start.elapsed() >= deadline {
            return Err("poll_until: deadline exceeded");
        }
        sleep(Duration::from_millis(25)).await;
    }
}

// ---------- Gateway harness ----------

pub struct GatewayHandle {
    pub addr: SocketAddr,
    pub task: JoinHandle<()>,
}

/// Spin up an N-node ring + a Gateway pointed at it.
///
/// Lifts the proven pattern from `tests/safety.rs::gateway_oversized_post_returns_413`:
/// bind ephemeral, drop the listener, let `Gateway::run_server` re-bind it,
/// and tolerate the brief race window via a 1 s connect-retry loop.
pub async fn spin_up_with_gateway(opts: RingOpts) -> (Ring, GatewayHandle) {
    use tokio::net::TcpListener;

    let ring = spin_up(opts).await;

    let node_addrs: Vec<String> = ring.nodes.iter().map(|h| h.addr.to_string()).collect();
    let gw_listener = TcpListener::bind("127.0.0.1:0").await.expect("gw bind");
    let gw_addr = gw_listener.local_addr().expect("gw addr");
    drop(gw_listener);
    let gw = ouroboros_fs::Gateway::new(node_addrs);
    let listen = gw_addr.to_string();
    let task = tokio::spawn(async move {
        let _ = gw.run_server(listen).await;
    });

    // Wait for the gateway's accept loop to be reachable. Under heavy
    // parallel test load (7+ binaries, many rings spinning up at once)
    // the gateway can take a few seconds to bind+accept; 5 s is the
    // conservative ceiling — generous enough that CI rarely flakes,
    // not so high that real failures take forever to surface.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(gw_addr).await.is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("gateway never became reachable on {gw_addr}");
        }
        sleep(Duration::from_millis(10)).await;
    }

    (ring, GatewayHandle { addr: gw_addr, task })
}

// ---------- Minimal HTTP client ----------

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn body_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> serde_json::Result<T> {
        serde_json::from_slice(&self.body)
    }
}

/// Hand-rolled HTTP/1.1 request: write the lines, drain to EOF, parse.
/// The Gateway always sends `Connection: close`, so read-until-EOF is fine.
///
/// Retries the initial connect for up to 1 s — under heavy parallel test
/// load the gateway's accept loop can briefly race the client's connect,
/// producing transient ECONNREFUSED. The retry is the simplest mitigation
/// short of a Gateway API change to take a pre-bound listener.
async fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<HttpResponse> {
    let connect_deadline = Instant::now() + Duration::from_secs(1);
    let mut s = loop {
        match TcpStream::connect(addr).await {
            Ok(s) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused
                && Instant::now() < connect_deadline =>
            {
                sleep(Duration::from_millis(20)).await;
                continue;
            }
            Err(e) => return Err(e),
        }
    };

    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");

    s.write_all(req.as_bytes()).await?;
    if !body.is_empty() {
        s.write_all(body).await?;
    }
    s.shutdown().await.ok();

    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await?;

    parse_http_response(&raw)
}

fn parse_http_response(raw: &[u8]) -> std::io::Result<HttpResponse> {
    // Find the header/body split.
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "no \\r\\n\\r\\n in response")
        })?;
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "non-utf8 head"))?;
    let body = raw[split + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.split_whitespace();
    let _proto = parts.next();
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad status"))?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

pub async fn http_get(addr: SocketAddr, path: &str) -> std::io::Result<HttpResponse> {
    http_request(addr, "GET", path, &[], &[]).await
}

pub async fn http_post(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<HttpResponse> {
    http_request(addr, "POST", path, headers, body).await
}

pub async fn http_options(addr: SocketAddr, path: &str) -> std::io::Result<HttpResponse> {
    http_request(addr, "OPTIONS", path, &[], &[]).await
}

// ---------- Subprocess "ring of real binaries" for heal coverage ----------

pub mod child_ring {
    //! Spawns the release binary as N child processes for tests that need
    //! `respawn_dead=true` (i.e., the full `handle_node_death` path with
    //! exec + `share_data_with_new_node`). Used only by the
    //! `#[ignore]`d heal subprocess test.

    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio::process::{Child, Command};
    use tokio::time::{Instant, sleep};

    /// Process-scoped base port. Range 30000..60000; 30000 ports of slack so
    /// PID-collision probability between two simultaneous test runs is
    /// ~0.1 % (vs 0.03 % the original plan claimed for a smaller modulo).
    pub fn process_scoped_base_port() -> u16 {
        30_000 + (std::process::id() % 30_000) as u16
    }

    /// RAII guard that SIGKILLs every recorded child on Drop, even on panic.
    /// Hand-rolled (no `scopeguard` dep). Best-effort: errors are swallowed
    /// because Drop runs in non-async context.
    pub struct KillerGuard {
        pids: Vec<u32>,
        /// Ports we expect a respawned grandchild to bind on. Drop will
        /// `lsof -t` them and SIGKILL whatever's listening.
        pub respawn_ports: Vec<u16>,
    }

    impl KillerGuard {
        pub fn new() -> Self {
            Self {
                pids: Vec::new(),
                respawn_ports: Vec::new(),
            }
        }

        pub fn record_pid(&mut self, pid: u32) {
            self.pids.push(pid);
        }
    }

    impl Drop for KillerGuard {
        fn drop(&mut self) {
            // Kill direct children.
            for pid in &self.pids {
                #[cfg(unix)]
                unsafe {
                    libc::kill(*pid as i32, libc::SIGKILL);
                }
            }
            // Best-effort cleanup of grandchildren respawned on the listed
            // ports (the test binary doesn't directly own them).
            for port in &self.respawn_ports {
                let out = std::process::Command::new("lsof")
                    .arg("-t")
                    .arg(format!("-iTCP:{port}"))
                    .arg("-sTCP:LISTEN")
                    .output();
                if let Ok(out) = out {
                    if let Ok(s) = String::from_utf8(out.stdout) {
                        for pid_str in s.lines() {
                            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                                #[cfg(unix)]
                                unsafe {
                                    libc::kill(pid, libc::SIGKILL);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// 3-node ring of release binaries. Children own their listeners; we
    /// own the SIGKILL contract via `KillerGuard`.
    pub struct ChildRing {
        pub base_port: u16,
        pub children: Vec<Child>,
        pub killer_guard: KillerGuard,
    }

    /// Resolve the release binary path. If it's missing, run
    /// `cargo build --release` synchronously. Cheap when warm.
    fn release_binary() -> PathBuf {
        let path = PathBuf::from("target/release/ouroboros_fs");
        if !path.exists() {
            let status = std::process::Command::new("cargo")
                .args(["build", "--release"])
                .status()
                .expect("run cargo build --release");
            assert!(status.success(), "cargo build --release failed");
        }
        path
    }

    pub async fn wait_until_listening(
        host: &str,
        port: u16,
        deadline: Duration,
    ) -> std::io::Result<()> {
        let start = Instant::now();
        let addr = format!("{host}:{port}");
        loop {
            if TcpStream::connect(&addr).await.is_ok() {
                return Ok(());
            }
            if start.elapsed() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("not listening on {addr}"),
                ));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// Spawn `n` binaries on `base_port..base_port+n` with gossip 200 ms.
    /// Wires them with `NODE NEXT` after each is reachable. Returns the
    /// `ChildRing` whose KillerGuard cleanup fires on Drop.
    pub async fn spawn(n: u16, base_port: u16) -> std::io::Result<ChildRing> {
        let exe = release_binary();
        let mut children = Vec::with_capacity(n as usize);
        let mut killer_guard = KillerGuard::new();

        for i in 0..n {
            let port = base_port + i;
            let child = Command::new(&exe)
                .args([
                    "run",
                    "--addr",
                    &format!("127.0.0.1:{port}"),
                    "--wait-time",
                    "200",
                ])
                .kill_on_drop(true)
                .spawn()?;
            if let Some(pid) = child.id() {
                killer_guard.record_pid(pid);
            }
            children.push(child);
        }

        // Wait for each to listen (deadline 5 s per child).
        for i in 0..n {
            let port = base_port + i;
            wait_until_listening("127.0.0.1", port, Duration::from_secs(5)).await?;
        }

        // Wire the ring: i -> (i+1) % n.
        for i in 0..n {
            let from = format!("127.0.0.1:{}", base_port + i);
            let to = format!("127.0.0.1:{}", base_port + ((i + 1) % n));
            let mut s = TcpStream::connect(&from).await?;
            let line = format!("NODE NEXT {to}\n");
            s.write_all(line.as_bytes()).await?;
            // Drain the ACK so the connection closes cleanly.
            let mut buf = [0u8; 64];
            let _ = tokio::time::timeout(Duration::from_millis(500), s.read(&mut buf)).await;
        }

        Ok(ChildRing {
            base_port,
            children,
            killer_guard,
        })
    }
}
