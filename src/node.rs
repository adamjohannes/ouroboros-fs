use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::{oneshot, RwLock},
};

/// Shared node state & actions.
///
/// Notes:
/// - `next_port` is the *configured* next hop in the ring (if any).
/// - We keep **no persistent topology** for WALK. The entire path is carried
///   inside the message (`history`) hop-by-hop.
/// - Only the *start node* allocates a short-lived "pending walk" entry
///   (a oneshot sender keyed by `token`) so we can reply on the same client
///   connection when the loop closes.
#[derive(Debug)]
pub struct Node {
    /// Where this node is listening (e.g., "127.0.0.1:7001")
    pub port: String,

    /// Address of the next node in the ring; None until set via SET_NEXT
    pub next_port: RwLock<Option<String>>,

    // --- ephemeral, per-request state (start node only) ---
    pending_walks: RwLock<HashMap<String, oneshot::Sender<String>>>,
    walk_counter: AtomicU64,
}

impl Node {
    /// Construct a new node handle (wrapped in Arc for cheap clones).
    pub fn new(port: String) -> Arc<Self> {
        Arc::new(Self {
            port,
            next_port: RwLock::new(None),
            pending_walks: RwLock::new(HashMap::new()),
            walk_counter: AtomicU64::new(1),
        })
    }

    /// Update the `next_port` pointer.
    pub async fn set_next(&self, addr: String) {
        *self.next_port.write().await = Some(addr);
    }

    /// Read the `next_port` pointer.
    pub async fn get_next(&self) -> Option<String> {
        self.next_port.read().await.clone()
    }

    /* ---------------- Existing RING forwarding ---------------- */

    /// Fire-and-forget forward of a RING command to the next node.
    pub async fn forward_ring(
        &self,
        ttl: u32,
        msg: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(next) = self.get_next().await {
            let mut s = TcpStream::connect(&next).await?;
            let line = format!("RING {} {}\n", ttl, msg);
            s.write_all(line.as_bytes()).await?;
        }
        Ok(())
    }

    /* ---------------- WALK helpers ---------------- */

    /// Create a unique token for a WALK request (scoped to this node address).
    fn next_token(&self) -> String {
        let n = self.walk_counter.fetch_add(1, Ordering::Relaxed);
        format!("{}-{}", self.port, n)
    }

    /// Register a pending WALK at the start node and get a receiver we can await.
    pub async fn register_walk(&self, token: &str) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.pending_walks.write().await.insert(token.to_string(), tx);
        rx
    }

    /// Complete a pending WALK (if any) and deliver the final history.
    /// Returns `true` if a waiter was found.
    pub async fn finish_walk(&self, token: &str, history: String) -> bool {
        if let Some(tx) = self.pending_walks.write().await.remove(token) {
            let _ = tx.send(history);
            true
        } else {
            false
        }
    }

    /// Forward one WALK hop to `next` with the current single-line history.
    pub async fn forward_walk_hop(
        &self,
        token: &str,
        start_addr: &str,
        history: &str, // semicolon-separated list of edges
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(next) = self.get_next().await {
            let mut s = TcpStream::connect(&next).await?;
            // Single-line payload (critical for a line-delimited protocol)
            let line = format!("WALK HOP {} {} {}\n", token, start_addr, history);
            s.write_all(line.as_bytes()).await?;
        }
        Ok(())
    }

    /// Send the final result back to the start node (closing the loop).
    pub async fn send_walk_done(
        &self,
        start_addr: &str,
        token: &str,
        history: &str, // semicolon-separated
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut s = TcpStream::connect(start_addr).await?;
        let line = format!("WALK DONE {} {}\n", token, history);
        s.write_all(line.as_bytes()).await?;
        Ok(())
    }
}

/* ---------- utility helpers used by WALK flow ---------- */

/// Extract the port number from "ip:port" (falls back to the whole string
/// if the format is unexpected).
pub fn port_str(addr: &str) -> &str {
    addr.rsplit(':').next().unwrap_or(addr)
}

/// Append "A->B" to a **single-line** history using ';' as the delimiter.
/// On the wire we MUST stay single-line because the protocol is line-delimited.
pub fn append_edge(mut history: String, from_addr: &str, to_addr: &str) -> String {
    let from = port_str(from_addr);
    let to = port_str(to_addr);
    let edge = format!("{from}->{to}");
    if history.is_empty() {
        edge
    } else {
        history.push(';');
        history.push_str(&edge);
        history
    }
}

impl Node {
    /// Convenience: start the history with "self->next".
    pub async fn first_walk_history(&self) -> Option<String> {
        let next = self.get_next().await?;
        Some(append_edge(String::new(), &self.port, &next))
    }

    /// Expose a public token factory for the server.
    pub fn make_walk_token(&self) -> String {
        self.next_token()
    }
}
