//! Integration tests for server handlers that the round_trip / failover /
//! safety suites don't directly exercise. Each test opens raw TCP, sends
//! a single command, asserts on the response, and shuts down.

mod common;

use std::time::Duration;

use common::{Ring, RingOpts, push_bytes, shutdown, spin_up};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Send `line` to `addr`, half-close, drain to EOF, return the response.
/// Wraps in a 2-second timeout so handler hangs fail loudly.
async fn send_line(addr: std::net::SocketAddr, line: &str) -> std::io::Result<String> {
    let line_owned = line.to_string();
    let result = tokio::time::timeout(Duration::from_secs(2), async move {
        let mut s = TcpStream::connect(addr).await?;
        s.write_all(line_owned.as_bytes()).await?;
        s.shutdown().await.ok();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await?;
        Ok::<_, std::io::Error>(resp)
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "send_line timed out"))?;
    result
}

// ---------- NODE handlers ----------

#[tokio::test(flavor = "multi_thread")]
async fn node_status_returns_port_and_next() {
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(ring.addr(0), "NODE STATUS\n").await.unwrap();
    let port0 = ring.addr(0).port();
    let port1 = ring.addr(1).port();
    assert!(resp.contains(&format!("PORT 127.0.0.1:{port0}")), "resp: {resp:?}");
    assert!(resp.contains(&format!("NEXT 127.0.0.1:{port1}")), "resp: {resp:?}");
    assert!(resp.trim_end().ends_with("OK"), "resp: {resp:?}");
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_status_self_loop_when_n_eq_one() {
    // Single-node ring: the harness wires next to self. STATUS reflects that.
    let ring = spin_up(RingOpts {
        n: 1,
        ..RingOpts::default()
    })
    .await;
    let resp = send_line(ring.addr(0), "NODE STATUS\n").await.unwrap();
    let port = ring.addr(0).port();
    assert!(resp.contains(&format!("NEXT 127.0.0.1:{port}")), "resp: {resp:?}");
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_ping_returns_pong() {
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(ring.addr(0), "NODE PING\n").await.unwrap();
    assert_eq!(resp.trim_end(), "PONG");
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_heal_hop_independent_of_walk_returns_ok() {
    // A bare HEAL-HOP without a registered walk-token still ACKs OK; the
    // handler ACKs first and works in a spawned task.
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(
        ring.addr(0),
        &format!("NODE HEAL-HOP tok-bare 127.0.0.1:{}\n", ring.addr(0).port()),
    )
    .await
    .unwrap();
    assert!(resp.starts_with("OK"), "resp: {resp:?}");
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn node_heal_done_unknown_token_acks() {
    // The handler is silent on unknown tokens (finish_heal_walk returns false).
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(ring.addr(0), "NODE HEAL-DONE bogus-token\n")
        .await
        .unwrap();
    assert!(resp.starts_with("OK"), "resp: {resp:?}");
    shutdown(ring).await;
}

// ---------- RING FORWARD ----------

#[tokio::test(flavor = "multi_thread")]
async fn ring_forward_ttl_zero_does_not_forward() {
    use std::sync::atomic::Ordering;
    let ring = spin_up(RingOpts::default()).await;
    let before = ring.nodes[0].node.netmap_broadcasts.load(Ordering::Relaxed);
    let resp = send_line(ring.addr(0), "RING FORWARD 0 hello\n")
        .await
        .unwrap();
    assert!(resp.starts_with("OK"), "resp: {resp:?}");
    // Give any spurious forward task time to misbehave.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = ring.nodes[0].node.netmap_broadcasts.load(Ordering::Relaxed);
    // RING FORWARD shouldn't trigger netmap broadcasts; this is just a cheap
    // observable signal that "no extra side-effect happened".
    assert_eq!(before, after);
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ring_forward_ttl_one_acks() {
    // We deliberately don't assert downstream observability — capturing the
    // forwarded message would require a custom listener wired in as the
    // next-hop, which is heavier than the test is worth. The contract here
    // is "sender gets OK and no panic."
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(ring.addr(0), "RING FORWARD 1 ttl1-msg\n")
        .await
        .unwrap();
    assert!(resp.starts_with("OK"), "resp: {resp:?}");
    shutdown(ring).await;
}

// ---------- TOPOLOGY ----------

#[tokio::test(flavor = "multi_thread")]
async fn topology_set_directly_populates_map() {
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(
        ring.addr(0),
        "TOPOLOGY SET 9000->9001;9001->9002\n",
    )
    .await
    .unwrap();
    assert!(resp.starts_with("OK"), "resp: {resp:?}");
    {
        let m = ring.nodes[0].node.topology_map.read().await;
        assert_eq!(m.get("9000"), Some(&"9001".to_string()));
        assert_eq!(m.get("9001"), Some(&"9002".to_string()));
    }
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn topology_set_empty_history_clears_map() {
    let ring = spin_up(RingOpts::default()).await;
    // First set a synthetic history to confirm clear actually clears.
    send_line(ring.addr(0), "TOPOLOGY SET 9000->9001\n")
        .await
        .unwrap();
    // SET with trailing space → empty history → clear.
    send_line(ring.addr(0), "TOPOLOGY SET \n").await.unwrap();
    {
        let m = ring.nodes[0].node.topology_map.read().await;
        assert!(m.is_empty());
    }
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn topology_walk_end_to_end_returns_history() {
    // spin_up already fires one WALK during convergence. Trigger a second
    // explicit WALK and assert the response shape.
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(ring.addr(0), "TOPOLOGY WALK\n").await.unwrap();
    // Response shape: each `from->to` segment on its own line, then `OK`.
    assert!(resp.contains("->"), "no edges in response: {resp:?}");
    assert!(resp.trim_end().ends_with("OK"), "resp: {resp:?}");
    // 3 nodes → 3 edges (closing the ring).
    let edges = resp.lines().filter(|l| l.contains("->")).count();
    assert_eq!(edges, 3, "edge count mismatch: {resp:?}");
    shutdown(ring).await;
}

// ---------- NETMAP ----------

#[tokio::test(flavor = "multi_thread")]
async fn netmap_set_directly_populates() {
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(ring.addr(0), "NETMAP SET 7000=Alive,7001=Dead\n")
        .await
        .unwrap();
    assert!(resp.starts_with("OK"), "resp: {resp:?}");
    let lines = ring.nodes[0].node.get_network_nodes_lines().await;
    assert!(lines.contains(&"7001=Dead".to_string()), "lines: {lines:?}");
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netmap_get_returns_alive_lines_then_ok() {
    // After spin_up, NETMAP DISCOVER has populated all nodes.
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(ring.addr(0), "NETMAP GET\n").await.unwrap();
    let lines: Vec<&str> = resp.lines().collect();
    // 3 alive entries + "OK".
    assert_eq!(lines.len(), 4, "resp: {resp:?}");
    for line in &lines[..3] {
        assert!(line.ends_with("=Alive"), "expected Alive: {line:?}");
    }
    assert_eq!(lines[3], "OK");
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netmap_get_empty_state_returns_empty_marker() {
    // Bypass spin_up's convergence: build a single-node ring without
    // running NETMAP DISCOVER, then GET. Reuses bind+serve directly.
    use ouroboros_fs::{FsyncMode, bind, serve};
    use std::sync::Arc;
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let (node, listener, addr) = bind(
        "127.0.0.1:0",
        Duration::ZERO,
        1 << 20,
        tmp.path().to_path_buf(),
        false,
        FsyncMode::None,
    )
    .await
    .unwrap();
    let serve_task = tokio::spawn({
        let node = Arc::clone(&node);
        async move { serve(node, listener).await; }
    });

    let resp = send_line(addr, "NETMAP GET\n").await.unwrap();
    assert_eq!(resp.lines().collect::<Vec<_>>(), vec!["(empty)", "OK"]);

    serve_task.abort();
    drop(tmp);
}

// ---------- FILE LIST CSV ----------

#[tokio::test(flavor = "multi_thread")]
async fn file_list_csv_header_present_when_empty() {
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(ring.addr(0), "FILE LIST\n").await.unwrap();
    // Just the header: `name,start,size`. Trailing newlines are normal.
    let mut lines = resp.lines();
    assert_eq!(lines.next(), Some("name,start,size"));
    assert_eq!(lines.next(), None, "expected empty body, got: {resp:?}");
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn file_list_csv_one_row_format() {
    let ring = spin_up(RingOpts::default()).await;
    let body = b"hi";
    push_bytes(ring.addr(0), "tiny.bin", body).await.unwrap();
    let resp = send_line(ring.addr(0), "FILE LIST\n").await.unwrap();
    let lines: Vec<&str> = resp.lines().collect();
    assert_eq!(lines[0], "name,start,size");
    let port0 = ring.addr(0).port();
    assert_eq!(lines[1], format!("tiny.bin,{port0},2"));
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn file_push_rejects_filename_with_comma() {
    // After Series A, names that contain `,` (or `"`, etc.) are rejected at
    // the parse layer rather than CSV-escaped at FILE LIST time. The CSV
    // escape function is still exercised as a unit test in server.rs.
    let ring = spin_up(RingOpts::default()).await;
    let res = push_bytes(ring.addr(0), "a,b.bin", b"x").await;
    assert!(
        res.is_err(),
        "expected push of comma-named file to be rejected, got {res:?}"
    );
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn file_push_rejects_filename_with_quote() {
    let ring = spin_up(RingOpts::default()).await;
    let res = push_bytes(ring.addr(0), "a\"b", b"x").await;
    assert!(
        res.is_err(),
        "expected push of quoted-name file to be rejected, got {res:?}"
    );
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn file_list_csv_sorted_by_name() {
    let ring = spin_up(RingOpts::default()).await;
    push_bytes(ring.addr(0), "c.bin", b"x").await.unwrap();
    push_bytes(ring.addr(0), "a.bin", b"x").await.unwrap();
    push_bytes(ring.addr(0), "b.bin", b"x").await.unwrap();
    let resp = send_line(ring.addr(0), "FILE LIST\n").await.unwrap();
    let lines: Vec<&str> = resp.lines().collect();
    // Header + 3 sorted rows.
    assert!(lines[1].starts_with("a.bin,"));
    assert!(lines[2].starts_with("b.bin,"));
    assert!(lines[3].starts_with("c.bin,"));
    shutdown(ring).await;
}

// ---------- Misc framing ----------

#[tokio::test(flavor = "multi_thread")]
async fn unknown_command_returns_err() {
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(ring.addr(0), "BLAH\n").await.unwrap();
    assert!(resp.starts_with("ERR "), "resp: {resp:?}");
    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crlf_terminator_works() {
    let ring = spin_up(RingOpts::default()).await;
    let resp = send_line(ring.addr(0), "NODE PING\r\n").await.unwrap();
    assert_eq!(resp.trim_end(), "PONG");
    shutdown(ring).await;
}

// Silence the unused-import warning in test binaries that don't use Ring.
#[allow(dead_code)]
fn _ring_marker(_: Ring) {}
