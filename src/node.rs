use crate::NodeStatus;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::{RwLock, oneshot},
};

#[derive(Debug, Clone)]
pub struct FileTag {
    pub start: u16,
    pub size: u64,
}

/// Shared node state & actions.
///
/// - `next_port`: configured next hop (if any).
/// - WALK uses a token->oneshot table at the start node.
/// - FILE push also uses token->oneshot at the start node (to confirm loop).
#[derive(Debug)]
pub struct Node {
    /// Where this node is listening
    pub port: String,

    /// Address of the next node in the ring, one until set via NODE NEXT
    pub next_port: RwLock<Option<String>>,

    // WALK pending acks (start node only)
    pending_walks: RwLock<HashMap<String, oneshot::Sender<String>>>,
    walk_counter: AtomicU64,

    // FILE pending acks (start node only)
    pending_files: RwLock<HashMap<String, oneshot::Sender<()>>>,
    file_counter: AtomicU64,

    /// Status of all nodes on the network
    network_nodes: RwLock<HashMap<String, NodeStatus>>,

    /// Mapping of file name -> (start port, size)
    pub file_tags: RwLock<HashMap<String, FileTag>>,

    /// Time between gossip health checks
    pub gossip_interval: Duration,

    /// Map of `port -> next_port` for the entire ring
    pub topology_map: RwLock<HashMap<String, String>>,
}

impl Node {
    pub fn new(port: String, gossip_interval: Duration) -> Arc<Self> {
        let network_nodes = RwLock::new(HashMap::new());

        Arc::new(Self {
            port,
            next_port: RwLock::new(None),
            pending_walks: RwLock::new(HashMap::new()),
            walk_counter: AtomicU64::new(1),
            pending_files: RwLock::new(HashMap::new()),
            file_counter: AtomicU64::new(1),
            network_nodes,
            file_tags: RwLock::new(HashMap::new()),
            gossip_interval,
            topology_map: RwLock::new(HashMap::new()),
        })
    }

    pub async fn set_next(&self, addr: String) {
        *self.next_port.write().await = Some(addr);
    }

    pub async fn get_next(&self) -> Option<String> {
        self.next_port.read().await.clone()
    }

    pub async fn forward_ring_forward(
        &self,
        ttl: u32,
        msg: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(next) = self.get_next().await {
            let mut s = TcpStream::connect(&next).await?;
            let line = format!("RING FORWARD {} {}\n", ttl, msg);
            s.write_all(line.as_bytes()).await?;
        }
        Ok(())
    }

    /* ---------------- FILE TAGS ---------------- */

    pub async fn set_file_tag(&self, name: &str, start_port: u16, size: u64) {
        self.file_tags.write().await.insert(
            name.to_string(),
            FileTag {
                start: start_port,
                size,
            },
        );
    }

    /// Serializes file tags into a single line: `name1:start1:size1;name2:start2:size2`
    pub async fn get_file_tags_entries(&self) -> String {
        let tags = self.file_tags.read().await;
        let mut items: Vec<(&String, &FileTag)> = tags.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));

        items
            .into_iter()
            .map(|(name, tag)| {
                // Replace special chars in name to avoid parsing errors
                let safe_name = name.replace(':', "_").replace(';', "_");
                format!("{}:{}:{}", safe_name, tag.start, tag.size)
            })
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Parses file tags from a single line: `name1:start1:size1;name2:start2:size2`
    pub async fn set_file_tags_from_entries(&self, entries: &str) {
        let mut tags = self.file_tags.write().await;
        tags.clear();
        for entry in entries.split(';').filter(|s| !s.is_empty()) {
            let parts: Vec<_> = entry.splitn(3, ':').collect();
            if parts.len() == 3 {
                let name = parts[0];
                let start_res = parts[1].parse::<u16>();
                let size_res = parts[2].parse::<u64>();
                if let (Ok(start), Ok(size)) = (start_res, size_res) {
                    tags.insert(name.to_string(), FileTag { start, size });
                }
            }
        }
    }

    /* ---------------- TOPOLOGY (WALK) helpers ---------------- */

    fn next_token(&self) -> String {
        let n = self.walk_counter.fetch_add(1, Ordering::Relaxed);
        format!("{}-{}", self.port, n)
    }

    pub fn make_walk_token(&self) -> String {
        self.next_token()
    }

    pub async fn register_walk(&self, token: &str) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.pending_walks
            .write()
            .await
            .insert(token.to_string(), tx);
        rx
    }

    pub async fn forward_topology_hop(
        &self,
        token: &str,
        start_addr: &str,
        history: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(next) = self.get_next().await {
            let mut s = TcpStream::connect(&next).await?;
            let line = format!("TOPOLOGY HOP {} {} {}\n", token, start_addr, history);
            s.write_all(line.as_bytes()).await?;
        }
        Ok(())
    }

    pub async fn finish_walk(&self, token: &str, history: String) -> bool {
        if let Some(tx) = self.pending_walks.write().await.remove(token) {
            let _ = tx.send(history);
            true
        } else {
            false
        }
    }

    pub async fn send_topology_done(
        &self,
        start_addr: &str,
        token: &str,
        history: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut s = TcpStream::connect(start_addr).await?;
        let line = format!("TOPOLOGY DONE {} {}\n", token, history);
        s.write_all(line.as_bytes()).await?;
        Ok(())
    }

    /* ---------------- FILE helpers ---------------- */

    fn next_file_token(&self) -> String {
        let n = self.file_counter.fetch_add(1, Ordering::Relaxed);
        format!("file-{}-{}", self.port, n)
    }

    pub fn make_file_token(&self) -> String {
        self.next_file_token()
    }

    pub async fn register_file(&self, token: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.pending_files
            .write()
            .await
            .insert(token.to_string(), tx);
        rx
    }

    pub async fn finish_file(&self, token: &str) -> bool {
        if let Some(tx) = self.pending_files.write().await.remove(token) {
            let _ = tx.send(());
            true
        } else {
            false
        }
    }

    pub async fn forward_file_relay_blob(
        &self,
        token: &str,
        start_addr: &str,
        size: u64,
        name: &str,
        data: &[u8],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(next) = self.get_next().await {
            let mut s = TcpStream::connect(&next).await?;
            let header = format!(
                "FILE RELAY-BLOB {} {} {} {}\n",
                token, start_addr, size, name
            );
            s.write_all(header.as_bytes()).await?;
            s.write_all(data).await?;
        }
        Ok(())
    }
}

/* ---------- WALK utility ---------- */

pub fn port_str(addr: &str) -> &str {
    addr.rsplit(':').next().unwrap_or(addr)
}

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
    pub async fn first_walk_history(&self) -> Option<String> {
        let next = self.get_next().await?;
        Some(append_edge(String::new(), &self.port, &next))
    }
}

/* ---------- NETMAP (INVESTIGATION) helpers ---------- */

fn host_str(addr: &str) -> &str {
    addr.split(':').next().unwrap_or("127.0.0.1")
}

fn parse_entries(entries: &str) -> HashMap<String, NodeStatus> {
    let mut map = HashMap::new();
    for part in entries.split(',') {
        let kv = part.trim();
        if kv.is_empty() {
            continue;
        }
        let mut it = kv.splitn(2, '=');
        let k = it.next().unwrap_or("").trim();
        let v = it.next().unwrap_or("").trim();
        if k.is_empty() {
            continue;
        }
        let status = match v {
            "Alive" | "alive" => NodeStatus::Alive,
            "Dead" | "dead" => NodeStatus::Dead,
            _ => NodeStatus::Alive,
        };
        map.insert(k.to_string(), status);
    }
    map
}

fn serialize_entries(map: &HashMap<String, NodeStatus>) -> String {
    let mut keys: Vec<_> = map.keys().cloned().collect();
    keys.sort_unstable();
    let mut out = String::new();
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(match map.get(k) {
            Some(NodeStatus::Alive) => "Alive",
            Some(NodeStatus::Dead) => "Dead",
            None => "Alive",
        });
    }
    out
}

impl Node {
    pub fn make_invest_token(&self) -> String {
        self.next_token()
    }

    pub fn entries_with_self(&self, entries: &str) -> String {
        let mut map = parse_entries(entries);
        map.insert(port_str(&self.port).to_string(), NodeStatus::Alive);
        serialize_entries(&map)
    }

    pub async fn set_network_nodes_from_entries(&self, entries: &str) {
        let map = parse_entries(entries);
        *self.network_nodes.write().await = map;
    }

    /// Quick count of known nodes (>=1)
    pub async fn network_size(&self) -> usize {
        let n = self.network_nodes.read().await.len();
        if n == 0 { 1 } else { n }
    }

    /// Human-friendly lines for "NETMAP GET"
    pub async fn get_network_nodes_lines(&self) -> Vec<String> {
        let map = self.network_nodes.read().await;
        let mut keys: Vec<_> = map.keys().cloned().collect();
        keys.sort_unstable();
        keys.into_iter()
            .map(|k| {
                format!(
                    "{}={:?}",
                    k,
                    map.get(&k).cloned().unwrap_or(NodeStatus::Alive)
                )
            })
            .collect()
    }

    pub async fn forward_netmap_hop(
        &self,
        token: &str,
        start_addr: &str,
        entries: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(next) = self.get_next().await {
            let mut s = TcpStream::connect(&next).await?;
            let line = format!("NETMAP HOP {} {} {}\n", token, start_addr, entries);
            s.write_all(line.as_bytes()).await?;
        }
        Ok(())
    }

    pub async fn send_netmap_done(
        &self,
        start_addr: &str,
        token: &str,
        entries: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut s = TcpStream::connect(start_addr).await?;
        let line = format!("NETMAP DONE {} {}\n", token, entries);
        s.write_all(line.as_bytes()).await?;
        Ok(())
    }

    pub async fn broadcast_netmap(&self, entries: &str) {
        let map = parse_entries(entries);
        let host = host_str(&self.port).to_string();
        for port in map.keys() {
            let addr = format!("{}:{}", host, port);
            if addr == self.port {
                continue;
            } // Don't broadcast to self
            if let Ok(mut s) = TcpStream::connect(&addr).await {
                let line = format!("NETMAP SET {}\n", entries);
                let _ = s.write_all(line.as_bytes()).await;
            }
        }
    }
}

/* ---------- Gossip/Topology helpers ---------- */
impl Node {
    pub async fn update_node_status(&self, port: String, status: NodeStatus) {
        self.network_nodes.write().await.insert(port, status);
    }

    pub async fn get_network_nodes_entries(&self) -> String {
        let map = self.network_nodes.read().await;
        serialize_entries(&map)
    }

    /// Gets current netmap entries and broadcasts them
    pub async fn broadcast_netmap_update(&self) {
        let entries = self.get_network_nodes_entries().await;
        self.broadcast_netmap(&entries).await;
    }

    /// Parses "7000->7001;7001->7002" and stores it
    pub async fn set_topology_from_history(&self, history: &str) {
        let mut map = self.topology_map.write().await;
        map.clear();
        for edge in history.split(';').filter(|s| !s.is_empty()) {
            if let Some((from, to)) = edge.split_once("->") {
                map.insert(from.to_string(), to.to_string());
            }
        }
        println!("[{}] Topology map updated", self.port);
    }

    /// Serializes topology map back to "7000->7001;7001->7002"
    pub async fn get_topology_history(&self) -> String {
        let map = self.topology_map.read().await;
        let mut keys: Vec<_> = map.keys().cloned().collect();
        keys.sort_unstable();
        keys.into_iter()
            .map(|k| format!("{}->{}", k, map.get(&k).unwrap_or(&"".to_string())))
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Broadcasts the full topology map to all nodes
    pub async fn broadcast_topology_set(&self) {
        let history = self.get_topology_history().await;
        if history.is_empty() {
            return;
        }

        let map = self.network_nodes.read().await;
        let host = host_str(&self.port).to_string();
        println!("[{}] Broadcasting topology: {}", self.port, history);
        for port in map.keys() {
            let addr = format!("{}:{}", host, port);
            if addr == self.port {
                continue;
            }
            if let Ok(mut s) = TcpStream::connect(&addr).await {
                let line = format!("TOPOLOGY SET {}\n", history);
                let _ = s.write_all(line.as_bytes()).await;
            }
        }
    }

    /// Finds the next hop for a *specific node* from the stored topology
    pub async fn get_next_for_node(&self, port: &str) -> Option<String> {
        self.topology_map.read().await.get(port).cloned()
    }
}
