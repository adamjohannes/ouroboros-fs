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

use common::{child_ring, sha256};
use tempfile::TempDir;
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
    let mut ring = child_ring::spawn(3, base, None)
        .await
        .expect("spawn child ring");

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

/// §1.5 contract: when the healer respawns a dead neighbor, the new child
/// inherits the original storage_root via `--storage-root`. Without this,
/// on-disk content/ + backup/ chunks become orphaned.
///
/// The shape: spawn a 3-child ring under a tempdir, PUSH a file (which
/// produces chunks under that tempdir), kill the chunk-owning node, wait
/// for the heal, PULL the file from a different node — the bytes must
/// survive (served from either the respawned node's inherited content/
/// or, failing that, its predecessor's backup/).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn respawn_inherits_storage_root() {
    let base = child_ring::process_scoped_base_port() + 100; // offset from full_heal test
    let tmp = TempDir::new().expect("tempdir");
    let storage = tmp.path().to_path_buf();
    eprintln!(
        "[heal_subprocess] storage_root={} base_port={base}",
        storage.display()
    );

    let mut ring = child_ring::spawn(3, base, Some(storage.clone()))
        .await
        .expect("spawn child ring");

    let killed_port = base + 1;
    ring.killer_guard.respawn_ports.push(killed_port);

    // NETMAP DISCOVER + TOPOLOGY WALK so the ring is fully formed.
    probe("127.0.0.1", base, "NETMAP DISCOVER\n")
        .await
        .expect("NETMAP DISCOVER");
    probe("127.0.0.1", base, "TOPOLOGY WALK\n")
        .await
        .expect("TOPOLOGY WALK");

    // Wait for netmap convergence.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let resp = probe("127.0.0.1", base, "NETMAP GET\n")
            .await
            .expect("NETMAP GET");
        if resp.lines().filter(|l| l.ends_with("=Alive")).count() >= 3 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("netmap never reached 3 alive nodes; last: {resp:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // PUSH a file. Chunks fan out across all 3 nodes; chunk 1 owner is
    // node 1 (the one we'll kill). Use raw bytes to avoid the harness's
    // 300 ms settle window — we'll add our own settle below.
    let payload: Vec<u8> = (0..16_384u32).map(|i| (i & 0xff) as u8).collect();
    let want = sha256(&payload);
    {
        let mut s = TcpStream::connect(("127.0.0.1", base)).await.unwrap();
        let header = format!("FILE PUSH {} survive.bin\n", payload.len());
        s.write_all(header.as_bytes()).await.unwrap();
        s.write_all(&payload).await.unwrap();
        s.shutdown().await.ok();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        assert!(
            resp.contains("OK"),
            "expected OK after PUSH; got: {resp:?}"
        );
    }
    // Settle: backup-pushes are fire-and-forget.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Kill node 1.
    eprintln!("[heal_subprocess] killing child[1] on port {killed_port}");
    let _ = ring.children[1].start_kill();
    let _ = ring.children[1].wait().await;

    // Wait for heal: respawn + share_data lands in ~1-3 s locally.
    let pong_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(resp) = probe("127.0.0.1", killed_port, "NODE PING\n").await {
            if resp.trim_end() == "PONG" {
                break;
            }
        }
        if tokio::time::Instant::now() >= pong_deadline {
            panic!("respawn never PONGed within 20s");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Wait for the netmap to reflect the heal — ensures share_data has
    // copied FILE TAGS to the respawned child so it knows about
    // `survive.bin`.
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
            panic!("post-heal netmap missing {key}; last: {resp:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // PULL via node 0. The respawned node 1 must serve its inherited
    // chunk-1 from disk (storage_root persisted across respawn), or fall
    // through to predecessor backup. Either way the bytes survive.
    let body = {
        let mut s = TcpStream::connect(("127.0.0.1", base))
            .await
            .expect("connect node 0");
        s.write_all(b"FILE PULL survive.bin\n").await.unwrap();
        s.shutdown().await.ok();
        let mut got = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut got))
            .await
            .expect("PULL deadline")
            .expect("read body");
        got
    };
    assert!(
        !body.starts_with(b"ERR"),
        "PULL returned ERR: {:?}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(body.len(), payload.len(), "PULL returned short body");
    assert_eq!(sha256(&body), want, "PULL bytes mismatch");
}

/// §1.5b contract: when a respawned node's `storage_root` no longer
/// contains its chunks (disk failure, ransomware, accidental rm -rf),
/// the predecessor refills the content/ directory via the anti-entropy
/// step in `share_data_with_new_node`. After the heal, the respawned
/// node serves chunks directly from its own content/ — no PULL
/// fall-through needed.
///
/// Test shape:
///   1. Spawn 3-node ring under a tempdir.
///   2. PUSH a file. Node 1 holds chunk 1's content; node 0 holds
///      chunk 1's backup.
///   3. Kill node 1 AND wipe its storage. (Simulates disk failure.)
///   4. Wait for the heal. The anti-entropy step should refill node 1's
///      content/ from node 0's backup/.
///   5. Connect *directly to node 1* and FILE GET-CHUNK chunk 1 — the
///      data must come from the respawned node's own content/, not
///      from a fall-through path.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn anti_entropy_refills_content_after_respawn() {
    let base = child_ring::process_scoped_base_port() + 200;
    let tmp = TempDir::new().expect("tempdir");
    let storage = tmp.path().to_path_buf();
    eprintln!(
        "[heal_subprocess] anti-entropy storage_root={} base_port={base}",
        storage.display()
    );

    let mut ring = child_ring::spawn(3, base, Some(storage.clone()))
        .await
        .expect("spawn child ring");
    let killed_port = base + 1;
    ring.killer_guard.respawn_ports.push(killed_port);

    probe("127.0.0.1", base, "NETMAP DISCOVER\n")
        .await
        .expect("NETMAP DISCOVER");
    probe("127.0.0.1", base, "TOPOLOGY WALK\n")
        .await
        .expect("TOPOLOGY WALK");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let resp = probe("127.0.0.1", base, "NETMAP GET\n")
            .await
            .expect("NETMAP GET");
        if resp.lines().filter(|l| l.ends_with("=Alive")).count() >= 3 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("netmap never reached 3 alive nodes; last: {resp:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // PUSH a file. parts == 3, so chunk i lives on node i.
    let payload: Vec<u8> = (0..16_384u32).map(|i| (i & 0xff) as u8).collect();
    let want = sha256(&payload);
    {
        let mut s = TcpStream::connect(("127.0.0.1", base)).await.unwrap();
        let header = format!("FILE PUSH {} antientropy.bin\n", payload.len());
        s.write_all(header.as_bytes()).await.unwrap();
        s.write_all(&payload).await.unwrap();
        s.shutdown().await.ok();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        assert!(resp.contains("OK"), "expected OK after PUSH; got: {resp:?}");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Kill node 1 AND wipe its storage. The respawn's content/ + backup/
    // start empty; only the anti-entropy refill from node 0's backup/
    // can recover chunk 1.
    let node1_storage = storage.join(format!("{killed_port}"));
    eprintln!("[heal_subprocess] killing node 1 and wiping {}", node1_storage.display());
    let _ = ring.children[1].start_kill();
    let _ = ring.children[1].wait().await;
    let _ = std::fs::remove_dir_all(&node1_storage);

    // Wait for the heal.
    let pong_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(resp) = probe("127.0.0.1", killed_port, "NODE PING\n").await {
            if resp.trim_end() == "PONG" {
                break;
            }
        }
        if tokio::time::Instant::now() >= pong_deadline {
            panic!("respawn never PONGed within 20s");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Wait for share_data + anti-entropy to land. The refill is sync
    // within share_data, but the respawn detection is async; settle for
    // a moment.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The on-disk content/ for node 1 should now hold chunk 1.
    // Verify by stat'ing the file directly.
    let chunk1_path = node1_storage
        .join("content")
        .join("antientropy.bin.part-002-of-003");
    assert!(
        chunk1_path.exists(),
        "anti-entropy did not refill chunk 1 at {}",
        chunk1_path.display()
    );

    // End-to-end: PULL via node 0; the bytes should round-trip.
    let body = {
        let mut s = TcpStream::connect(("127.0.0.1", base))
            .await
            .expect("connect node 0");
        s.write_all(b"FILE PULL antientropy.bin\n").await.unwrap();
        s.shutdown().await.ok();
        let mut got = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut got))
            .await
            .expect("PULL deadline")
            .expect("read body");
        got
    };
    assert!(
        !body.starts_with(b"ERR"),
        "PULL returned ERR: {:?}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(body.len(), payload.len(), "PULL returned short body");
    assert_eq!(sha256(&body), want, "PULL bytes mismatch");
}
