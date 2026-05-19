//! Subprocess heal test. Validates the full `handle_node_death` path that
//! the in-process harness can't exercise (it sets `respawn_dead=false`).
//!
//! `#[ignore]`d by default. Run with:
//!
//! ```bash
//! cargo test --release -- --ignored heal_subprocess
//! ```
//!
//! **Residual risks** (documented in tests/common/mod.rs::child_ring):
//!
//! - Two simultaneous `cargo test --ignored heal_subprocess` invocations
//!   on the same machine may collide on the modulo-based port. ~0.1 %
//!   probability per pair of runs in the 30000-60000 range.
//! - The test owns the original 3 children directly and SIGKILLs them on
//!   panic via KillerGuard. The respawned grandchild is owned by the
//!   ring's healer, not the test — KillerGuard cleans that up
//!   best-effort via `lsof -t -iTCP:<port>`. CI runners are ephemeral so
//!   any leak is bounded by the runner lifetime.

mod common;

use std::time::Duration;

use common::child_ring;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Send a single line to `host:port` and read until EOF, with a deadline.
async fn probe(host: &str, port: u16, line: &str) -> std::io::Result<String> {
    let line_owned = line.to_string();
    let host_owned = host.to_string();
    let result = tokio::time::timeout(Duration::from_secs(3), async move {
        let mut s = TcpStream::connect((host_owned.as_str(), port)).await?;
        s.write_all(line_owned.as_bytes()).await?;
        s.shutdown().await.ok();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await?;
        Ok::<_, std::io::Error>(resp)
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "probe timed out"))?;
    result
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn full_heal_respawns_dead_child_and_broadcasts() {
    let base = child_ring::process_scoped_base_port();
    eprintln!("[heal_subprocess] using base_port={base}");

    // Spawn the 3-child ring. Children are reaped by KillerGuard on Drop.
    let mut ring = child_ring::spawn(3, base).await.expect("spawn child ring");

    // Record the port we expect a respawned grandchild to bind on, so the
    // KillerGuard can clean it up via lsof on Drop.
    let killed_port = base + 1;
    ring.killer_guard.respawn_ports.push(killed_port);

    // Trigger NETMAP DISCOVER on node 0; poll until each node has all 3
    // entries. The `wait_until_listening` calls in `spawn` already
    // guaranteed the children are up.
    probe("127.0.0.1", base, "NETMAP DISCOVER\n")
        .await
        .expect("NETMAP DISCOVER");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let resp = probe("127.0.0.1", base, "NETMAP GET\n")
            .await
            .expect("NETMAP GET");
        // Header lines look like "<port>=Alive"; count them.
        let alive_count = resp
            .lines()
            .filter(|l| l.ends_with("=Alive"))
            .count();
        if alive_count >= 3 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("netmap never reached 3 alive nodes; last resp: {resp:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Kill child[1] (the middle node). On Unix this sends SIGKILL via
    // tokio::process::Child::start_kill().
    eprintln!("[heal_subprocess] killing child[1] on port {killed_port}");
    let _ = ring.children[1].start_kill();
    let _ = ring.children[1].wait().await;

    // Confirm the kill landed: the killed port should refuse connections
    // for at least one instant before the respawn.
    let mut saw_refused = false;
    let refused_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < refused_deadline {
        match TcpStream::connect(("127.0.0.1", killed_port)).await {
            Err(_) => {
                saw_refused = true;
                break;
            }
            Ok(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    assert!(saw_refused, "killed port never showed refused; respawn raced the kill?");

    // Wait for the heal to land. The contract: after handle_node_death
    // completes (logged "Healing process complete."), the port is bound
    // again by a fresh process and `NODE PING` returns `PONG`. The detector
    // takes ~1 gossip cycle (200 ms) to notice; the respawn + share-data
    // path is another ~50-100 ms locally.
    let pong_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut respawned = false;
    while tokio::time::Instant::now() < pong_deadline {
        if let Ok(resp) = probe("127.0.0.1", killed_port, "NODE PING\n").await {
            if resp.trim_end() == "PONG" {
                respawned = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        respawned,
        "respawned node on port {killed_port} never PONGed within 20s"
    );

    // Confirm node 0's netmap reflects the heal: the killed port appears
    // as Alive (the final broadcast in handle_node_death). The broadcast
    // races the test's NETMAP GET — poll briefly for it.
    let netmap_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let key = format!("{killed_port}=Alive");
    loop {
        let resp = probe("127.0.0.1", base, "NETMAP GET\n")
            .await
            .expect("NETMAP GET after heal");
        if resp.contains(&key) {
            break;
        }
        if tokio::time::Instant::now() >= netmap_deadline {
            panic!(
                "netmap should show {killed_port} as Alive after heal; last: {resp:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ring.killer_guard cleans everything up on Drop — including the
    // grandchild listener on `killed_port`.
}
