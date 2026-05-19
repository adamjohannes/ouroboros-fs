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
