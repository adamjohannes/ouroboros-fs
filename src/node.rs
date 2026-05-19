use crate::NodeStatus;
use serde::Serialize;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::{RwLock, oneshot},
};
use tracing;

#[derive(Debug, Clone, Serialize)]
pub struct FileTag {
    pub start: u16,
    pub size: u64,
    pub parts: u32,
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

    // HEAL pending acks (start node only)
    pending_heals: RwLock<HashMap<String, oneshot::Sender<()>>>,

    /// Status of all nodes on the network
    network_nodes: RwLock<HashMap<String, NodeStatus>>,

    /// Mapping of file name -> (start port, size, parts)
    pub file_tags: RwLock<HashMap<String, FileTag>>,

    /// Time between gossip health checks
    pub gossip_interval: Duration,

    /// Max file size.
    pub file_size: u64,

    /// Map of `port -> next_port` for the entire ring
    pub topology_map: RwLock<HashMap<String, String>>,

    /// Filesystem root under which `<port>/content/` and `<port>/backup/` live.
    /// Binary defaults to `PathBuf::from("nodes")`; tests pass a tempdir.
    pub storage_root: PathBuf,

    /// When false, dead-neighbor detection skips the respawn step.
    /// Tests set this to false; the binary keeps it true.
    pub respawn_dead: AtomicBool,

    /// Counts how many times this node has called `broadcast_netmap_update`.
    /// Useful for tests that want to assert "exactly one broadcast per dead
    /// host"; also provides a cheap debug signal in production.
    pub netmap_broadcasts: AtomicU64,
}

impl Node {
    pub fn new(
        port: String,
        gossip_interval: Duration,
        file_size: u64,
        storage_root: PathBuf,
        respawn_dead: bool,
    ) -> Arc<Self> {
        let network_nodes = RwLock::new(HashMap::new());

        Arc::new(Self {
            port,
            next_port: RwLock::new(None),
            pending_walks: RwLock::new(HashMap::new()),
            walk_counter: AtomicU64::new(1),
            pending_heals: RwLock::new(HashMap::new()),
            network_nodes,
            file_tags: RwLock::new(HashMap::new()),
            gossip_interval,
            file_size,
            topology_map: RwLock::new(HashMap::new()),
            storage_root,
            respawn_dead: AtomicBool::new(respawn_dead),
            netmap_broadcasts: AtomicU64::new(0),
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

    // File Tags

    pub async fn set_file_tag(&self, name: &str, start_port: u16, size: u64, parts: u32) {
        self.file_tags.write().await.insert(
            name.to_string(),
            FileTag {
                start: start_port,
                size,
                parts,
            },
        );
    }

    /// Serializes file tags into a single line: `name1:start1:size1:parts1;name2:start2:size2:parts2`
    pub async fn get_file_tags_entries(&self) -> String {
        let tags = self.file_tags.read().await;
        let mut items: Vec<(&String, &FileTag)> = tags.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));

        items
            .into_iter()
            .map(|(name, tag)| {
                // Replace special chars in name to avoid parsing errors
                let safe_name = name.replace(':', "_").replace(';', "_");
                format!("{}:{}:{}:{}", safe_name, tag.start, tag.size, tag.parts)
            })
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Parses file tags from a single line: `name1:start1:size1:parts1;name2:start2:size2:parts2`
    pub async fn set_file_tags_from_entries(&self, entries: &str) {
        let mut tags = self.file_tags.write().await;
        tags.clear();
        for entry in entries.split(';').filter(|s| !s.is_empty()) {
            let parts: Vec<_> = entry.splitn(4, ':').collect();
            if parts.len() == 4 {
                let name = parts[0];
                let start_res = parts[1].parse::<u16>();
                let size_res = parts[2].parse::<u64>();
                let parts_res = parts[3].parse::<u32>();
                if let (Ok(start), Ok(size), Ok(parts_num)) = (start_res, size_res, parts_res) {
                    tags.insert(
                        name.to_string(),
                        FileTag {
                            start,
                            size,
                            parts: parts_num,
                        },
                    );
                }
            }
        }
    }

    // --- Topology (WALK) helpers

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

    pub async fn register_heal_walk(&self, token: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.pending_heals
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

    pub async fn finish_heal_walk(&self, token: &str) -> bool {
        if let Some(tx) = self.pending_heals.write().await.remove(token) {
            let _ = tx.send(());
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
}

// --- WALK utility

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

// --- NETMAP (INVESTIGATION) helpers

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

// --- Gossip/Topology helpers
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
        self.netmap_broadcasts.fetch_add(1, Ordering::Relaxed);
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
        tracing::debug!(node = %self.port, "Topology map updated");
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
        tracing::debug!(node = %self.port, history = %history, "Broadcasting topology");
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

#[cfg(test)]
mod tests {
    use super::{Node, append_edge, host_str, parse_entries, port_str, serialize_entries};
    use crate::NodeStatus;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// Build a Node suitable for unit tests. Uses a tempdir-style storage
    /// path that's never written to (no Node method here actually creates
    /// files), gossip disabled, respawn disabled.
    fn test_node(port: &str) -> std::sync::Arc<Node> {
        Node::new(
            port.to_string(),
            Duration::ZERO,
            1 << 30,
            PathBuf::from("/tmp/ouroboros_unit_unused"),
            false,
        )
    }

    #[test]
    fn port_str_ipv4() {
        assert_eq!(port_str("127.0.0.1:7000"), "7000");
    }

    #[test]
    fn port_str_hostname() {
        assert_eq!(port_str("localhost:8080"), "8080");
    }

    #[test]
    fn port_str_no_colon() {
        // Documents the fallback behavior: returns the input unchanged.
        assert_eq!(port_str("7000"), "7000");
    }

    #[test]
    fn port_str_ipv6_brackets() {
        // rsplit handles IPv6-with-brackets correctly (port_str picks the
        // last colon-separated chunk). Diverges from `host_of` in server.rs,
        // which splits on the first colon.
        assert_eq!(port_str("[::1]:7000"), "7000");
    }

    #[test]
    fn append_edge_first() {
        let h = append_edge(String::new(), "127.0.0.1:7000", "127.0.0.1:7001");
        assert_eq!(h, "7000->7001");
    }

    #[test]
    fn append_edge_subsequent() {
        let h = append_edge(
            "7000->7001".into(),
            "127.0.0.1:7001",
            "127.0.0.1:7002",
        );
        assert_eq!(h, "7000->7001;7001->7002");
    }

    // --- host_str (private fn): pinned for documentation parity with
    //     server.rs::host_of. host_str splits on the first colon.

    #[test]
    fn host_str_ipv4() {
        assert_eq!(host_str("127.0.0.1:7000"), "127.0.0.1");
    }

    #[test]
    fn host_str_no_colon_returns_input() {
        // `addr.split(':').next()` always yields the input on non-empty
        // strings; the `unwrap_or("127.0.0.1")` fallback is only reached
        // when split returns None — which never happens. So a colon-less
        // input is returned verbatim.
        assert_eq!(host_str("foo"), "foo");
    }

    #[test]
    fn host_str_empty_returns_empty() {
        // Documents that the "127.0.0.1" fallback path in the source is
        // effectively unreachable: split on an empty string yields a
        // single empty chunk, not None, so we get "" not "127.0.0.1".
        assert_eq!(host_str(""), "");
    }

    #[test]
    fn host_str_ipv6_brackets_known_gap() {
        // Mirrors the `host_of_ipv6_bracket_pin` in server.rs — the simple
        // `split(':')` strategy doesn't handle IPv6 brackets. Pinning the
        // current behavior so a future fix has a visible regression target.
        assert_eq!(host_str("[::1]:7000"), "[");
    }

    // --- parse_entries / serialize_entries

    #[test]
    fn parse_entries_alive_dead_mixed() {
        let m = parse_entries("7000=Alive,7001=Dead");
        assert_eq!(m.get("7000"), Some(&NodeStatus::Alive));
        assert_eq!(m.get("7001"), Some(&NodeStatus::Dead));
        assert_eq!(m.len(), 2);
        // Lowercase variants accepted.
        let m2 = parse_entries("7002=alive,7003=dead");
        assert_eq!(m2.get("7002"), Some(&NodeStatus::Alive));
        assert_eq!(m2.get("7003"), Some(&NodeStatus::Dead));
    }

    #[test]
    fn parse_entries_unknown_status_defaults_alive() {
        // The parser is lenient: an unrecognized status falls back to Alive.
        let m = parse_entries("7000=Maybe");
        assert_eq!(m.get("7000"), Some(&NodeStatus::Alive));
    }

    #[test]
    fn parse_entries_skips_empty_and_keyless() {
        // Trailing comma, double comma, "=Alive" (empty key) all silently dropped.
        let m = parse_entries("7000=Alive,,=Alive,7001=Dead,");
        assert_eq!(m.len(), 2);
        assert!(m.contains_key("7000"));
        assert!(m.contains_key("7001"));
    }

    #[test]
    fn parse_entries_handles_whitespace() {
        let m = parse_entries(" 7000 = Alive , 7001 = Dead ");
        assert_eq!(m.get("7000"), Some(&NodeStatus::Alive));
        assert_eq!(m.get("7001"), Some(&NodeStatus::Dead));
    }

    #[test]
    fn serialize_entries_sorted_by_key() {
        let mut m: HashMap<String, NodeStatus> = HashMap::new();
        m.insert("7002".into(), NodeStatus::Alive);
        m.insert("7000".into(), NodeStatus::Dead);
        m.insert("7001".into(), NodeStatus::Alive);
        // Ordering is deterministic regardless of insertion order.
        assert_eq!(serialize_entries(&m), "7000=Dead,7001=Alive,7002=Alive");
    }

    #[test]
    fn serialize_entries_empty_map() {
        let m: HashMap<String, NodeStatus> = HashMap::new();
        assert_eq!(serialize_entries(&m), "");
    }

    #[test]
    fn parse_serialize_roundtrip() {
        let mut m: HashMap<String, NodeStatus> = HashMap::new();
        for (k, v) in [
            ("7000", NodeStatus::Alive),
            ("7001", NodeStatus::Dead),
            ("7002", NodeStatus::Alive),
            ("7003", NodeStatus::Alive),
            ("7004", NodeStatus::Dead),
        ] {
            m.insert(k.into(), v);
        }
        let s = serialize_entries(&m);
        let m2 = parse_entries(&s);
        assert_eq!(m2, m);
    }

    // --- entries_with_self / update_node_status / get_network_nodes_lines

    #[tokio::test]
    async fn entries_with_self_inserts_self() {
        let node = test_node("127.0.0.1:7000");
        let out = node.entries_with_self("7001=Alive");
        // Sorted output, so 7000 appears first.
        assert_eq!(out, "7000=Alive,7001=Alive");
    }

    #[tokio::test]
    async fn entries_with_self_overwrites_self_to_alive() {
        let node = test_node("127.0.0.1:7000");
        // Even if the input claims we're Dead, the local entry is forced to Alive
        // (the only authoritative source for "this node is Alive" is the node itself).
        let out = node.entries_with_self("7000=Dead,7001=Alive");
        assert_eq!(out, "7000=Alive,7001=Alive");
    }

    #[tokio::test]
    async fn update_node_status_then_get_lines() {
        let node = test_node("127.0.0.1:7000");
        node.update_node_status("7001".into(), NodeStatus::Dead).await;
        let lines = node.get_network_nodes_lines().await;
        assert_eq!(lines, vec!["7001=Dead"]);
    }

    #[tokio::test]
    async fn get_network_nodes_lines_empty_returns_empty_vec() {
        let node = test_node("127.0.0.1:7000");
        let lines = node.get_network_nodes_lines().await;
        assert!(lines.is_empty());
    }

    // --- topology_map round-trip

    #[tokio::test]
    async fn set_topology_from_history_basic() {
        let node = test_node("127.0.0.1:7000");
        node.set_topology_from_history("7000->7001;7001->7002").await;
        let m = node.topology_map.read().await;
        assert_eq!(m.get("7000"), Some(&"7001".to_string()));
        assert_eq!(m.get("7001"), Some(&"7002".to_string()));
        assert_eq!(m.len(), 2);
    }

    #[tokio::test]
    async fn set_topology_from_history_clears_previous() {
        let node = test_node("127.0.0.1:7000");
        node.set_topology_from_history("7000->7001").await;
        node.set_topology_from_history("9000->9001").await;
        let m = node.topology_map.read().await;
        // Old edge is gone; only the new one remains.
        assert!(!m.contains_key("7000"));
        assert_eq!(m.get("9000"), Some(&"9001".to_string()));
    }

    #[tokio::test]
    async fn set_topology_from_history_skips_malformed() {
        let node = test_node("127.0.0.1:7000");
        node.set_topology_from_history("7000->7001;garbage;7001->7002").await;
        let m = node.topology_map.read().await;
        assert_eq!(m.len(), 2);
    }

    #[tokio::test]
    async fn set_topology_from_history_empty_string_is_noop_clear() {
        let node = test_node("127.0.0.1:7000");
        node.set_topology_from_history("7000->7001").await;
        node.set_topology_from_history("").await;
        let m = node.topology_map.read().await;
        assert!(m.is_empty());
    }

    #[tokio::test]
    async fn topology_history_roundtrip() {
        let node = test_node("127.0.0.1:7000");
        let original = "7000->7001;7001->7002;7002->7000";
        node.set_topology_from_history(original).await;
        let s = node.get_topology_history().await;
        // Round-trip via canonical sort-by-from. Original is already sorted.
        assert_eq!(s, original);
    }

    // --- file_tags round-trip

    #[tokio::test]
    async fn set_file_tags_from_entries_basic() {
        let node = test_node("127.0.0.1:7000");
        node.set_file_tags_from_entries("a:7000:1024:3;b:7001:2048:5").await;
        let tags = node.file_tags.read().await;
        let a = tags.get("a").unwrap();
        assert_eq!(a.start, 7000);
        assert_eq!(a.size, 1024);
        assert_eq!(a.parts, 3);
        let b = tags.get("b").unwrap();
        assert_eq!(b.start, 7001);
        assert_eq!(b.size, 2048);
        assert_eq!(b.parts, 5);
    }

    #[tokio::test]
    async fn set_file_tags_from_entries_skips_bad_arity() {
        // "a:1:2" only has 3 fields; should be silently dropped.
        let node = test_node("127.0.0.1:7000");
        node.set_file_tags_from_entries("a:1:2;b:7001:2048:5").await;
        let tags = node.file_tags.read().await;
        assert!(!tags.contains_key("a"));
        assert!(tags.contains_key("b"));
    }

    #[tokio::test]
    async fn set_file_tags_from_entries_skips_bad_numerics() {
        let node = test_node("127.0.0.1:7000");
        node.set_file_tags_from_entries("a:notnum:1:1;b:7001:2048:5").await;
        let tags = node.file_tags.read().await;
        assert!(!tags.contains_key("a"));
        assert!(tags.contains_key("b"));
    }

    #[tokio::test]
    async fn file_tags_entries_roundtrip() {
        let node = test_node("127.0.0.1:7000");
        let original = "alpha:7000:100:2;beta:7001:200:3";
        node.set_file_tags_from_entries(original).await;
        let s = node.get_file_tags_entries().await;
        // Canonical form is sort-by-name, which alpha/beta already satisfy.
        assert_eq!(s, original);
    }

    #[tokio::test]
    async fn file_tags_entries_sanitizes_separators() {
        // Names containing ':' or ';' get the offending chars replaced with '_'
        // on serialize, so the output is a *valid* round-trippable string but
        // the original name is not preserved verbatim.
        let node = test_node("127.0.0.1:7000");
        node.set_file_tag("a:b;c", 7000, 1, 1).await;
        let s = node.get_file_tags_entries().await;
        assert!(!s.contains("a:b;c"));
        assert!(s.starts_with("a_b_c:7000:1:1"));
    }

    // --- forward_ring_forward / broadcast_netmap[_update]

    #[tokio::test]
    async fn forward_ring_forward_no_next_is_noop() {
        // No next set; forward should silently succeed without attempting
        // any TCP connection.
        let node = test_node("127.0.0.1:7000");
        let res = node.forward_ring_forward(0, "msg").await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn broadcast_netmap_no_other_hosts_is_noop() {
        // The only entry is self; the loop's self-skip means no TCP attempts.
        let node = test_node("127.0.0.1:7000");
        node.broadcast_netmap("7000=Alive").await;
        // No assertion needed beyond "did not hang or panic"; the counter
        // assertion is in the next test and covers broadcast_netmap_update.
    }

    #[tokio::test]
    async fn broadcast_netmap_update_increments_counter() {
        let node = test_node("127.0.0.1:7000");
        let before = node.netmap_broadcasts.load(Ordering::Relaxed);
        node.broadcast_netmap_update().await;
        let after = node.netmap_broadcasts.load(Ordering::Relaxed);
        assert_eq!(after - before, 1);
    }
}
