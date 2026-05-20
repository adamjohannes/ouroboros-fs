//! Failover tests. Kill a node, exercise the backup-chunk path on PULL.

mod common;

use std::time::Duration;

use common::{RingOpts, kill_node, pull_bytes, push_bytes, rand_bytes, sha256, shutdown, spin_up};
use ouroboros_fs::node::port_str;

#[tokio::test(flavor = "multi_thread")]
async fn failover_kill_one_then_pull() {
    let mut ring = spin_up(RingOpts {
        n: 5,
        gossip_interval: Duration::from_millis(200),
        ..RingOpts::default()
    })
    .await;

    let bytes = rand_bytes(/*seed=*/ 100, 256 * 1024);
    let want = sha256(&bytes);

    push_bytes(ring.addr(0), "hot.bin", &bytes).await.unwrap();

    // Allow backup notification to populate predecessors.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Kill one node and let the others detect it.
    kill_node(&mut ring, 2).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    let got = pull_bytes(ring.addr(0), "hot.bin")
        .await
        .expect("pull after failover");
    assert_eq!(got.len(), bytes.len(), "length mismatch after failover");
    assert_eq!(sha256(&got), want, "SHA-256 mismatch after failover");

    shutdown(ring).await;
}

/// Adjacent double failure: chunk owner *and* its predecessor (backup
/// holder) both die. §3.1 contract: when adjacent owner+predecessor failure
/// makes a chunk permanently unrecoverable, the PULL emits the bytes it
/// could recover followed by a `\nERR truncated expected=<E> got=<G>\n`
/// trailer. Aware clients (the gateway, future SDKs) detect the trailer;
/// pure-byte clients still see a short body.
#[tokio::test(flavor = "multi_thread")]
async fn adjacent_double_failure_emits_truncation_signal() {
    let mut ring = spin_up(RingOpts {
        n: 5,
        gossip_interval: Duration::from_millis(200),
        ..RingOpts::default()
    })
    .await;

    let bytes = rand_bytes(101, 256 * 1024);
    push_bytes(ring.addr(0), "doomed.bin", &bytes)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Kill chunk owner (2) and its predecessor (1) which holds chunk 2's
    // backup. PULL completes but the body is short; we expect a
    // truncation trailer.
    kill_node(&mut ring, 2).await;
    kill_node(&mut ring, 1).await;
    tokio::time::sleep(Duration::from_millis(800)).await;

    let got = pull_bytes(ring.addr(0), "doomed.bin")
        .await
        .expect("pull should complete even with corruption");

    // The trailer is `\nERR truncated expected=<E> got=<G>\n` appended
    // after the (short) body. Verify both the short body and the trailer.
    let trailer_marker = b"\nERR truncated";
    let trailer_pos = got
        .windows(trailer_marker.len())
        .rposition(|w| w == trailer_marker)
        .expect("expected truncation trailer in PULL response");

    let body = &got[..trailer_pos];
    assert!(
        body.len() < bytes.len(),
        "body should be shorter than original; got {} vs {}",
        body.len(),
        bytes.len()
    );

    let trailer = &got[trailer_pos..];
    let trailer_str = std::str::from_utf8(trailer).expect("trailer is utf-8");
    assert!(
        trailer_str.contains(&format!("expected={}", bytes.len())),
        "trailer should announce expected size {}: {trailer_str:?}",
        bytes.len()
    );
    assert!(
        trailer_str.contains(&format!("got={}", body.len())),
        "trailer should announce actual emitted size {}: {trailer_str:?}",
        body.len()
    );

    shutdown(ring).await;
}

/// Pin PR4's broadcast-deduplication. When a multi-chunk pull encounters a
/// dead node, every failed chunk used to trigger its own
/// `broadcast_netmap_update`. PR4 batches: at most one broadcast per dead
/// host per pull. We assert via the `netmap_broadcasts` atomic counter on
/// the pulling node.
#[tokio::test(flavor = "multi_thread")]
async fn failover_no_double_broadcast() {
    use std::sync::atomic::Ordering;

    let mut ring = spin_up(RingOpts {
        n: 5,
        // Disable gossip — we want the pull, not the ambient ping loop, to
        // be the broadcast trigger. (Gossip would also call
        // broadcast_netmap_update on its own once it noticed the kill.)
        gossip_interval: Duration::ZERO,
        ..RingOpts::default()
    })
    .await;

    // 5 chunks across 5 nodes. Killing node 2 takes out chunk 2; the pull
    // will fall back to its predecessor (node 1) for the backup. Only one
    // chunk fails so one broadcast is the natural outcome — to also
    // exercise the dedup path I do a second pull in case something
    // re-broadcasts.
    let bytes = rand_bytes(/*seed=*/ 200, 5 * 4096);
    push_bytes(ring.addr(0), "trace.bin", &bytes).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    kill_node(&mut ring, 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let pulling_node = ring.nodes[0].node.clone();
    let before = pulling_node.netmap_broadcasts.load(Ordering::Relaxed);

    let got = pull_bytes(ring.addr(0), "trace.bin")
        .await
        .expect("pull through dead node");
    assert_eq!(sha256(&got), sha256(&bytes), "backup served wrong bytes");

    let after = pulling_node.netmap_broadcasts.load(Ordering::Relaxed);
    let delta = after - before;

    // The contract: at most ONE broadcast for one dead host, regardless of
    // how many chunks failed against it. Pre-PR4 this was N (one per
    // failed chunk); post-PR4 it's exactly 1.
    assert_eq!(
        delta, 1,
        "expected exactly 1 netmap broadcast for one dead host, got {delta}"
    );

    shutdown(ring).await;
}

/// PR6 contract: after a push, every chunk's predecessor holds a byte-for-byte
/// backup. We walk every node's `<storage_root>/<port>/backup/` directory and
/// hash the contents. The push-based path replaces an older notify-then-pull
/// dance; this test pins the new direct-push behavior.
#[tokio::test(flavor = "multi_thread")]
async fn backup_present_after_push() {
    let ring = spin_up(RingOpts {
        n: 4,
        ..RingOpts::default()
    })
    .await;

    let bytes = rand_bytes(/*seed=*/ 300, 4 * 4096); // 16 KiB, 4 chunks of 4 KiB
    push_bytes(ring.addr(0), "backed.bin", &bytes)
        .await
        .unwrap();

    // Allow PR6's spawned push_to_predecessor tasks to complete.
    tokio::time::sleep(Duration::from_millis(400)).await;

    // For each node i, look in nodes[i].storage_root/<port>/backup/ and
    // verify that the chunk whose owner is i+1 lives there.
    let chunk_size = 4096usize;
    let parts = ring.nodes.len() as u32;
    for i in 0..ring.nodes.len() {
        let owner_idx = (i + 1) % ring.nodes.len();
        let owner_chunk_index = owner_idx as u32; // chunk i lives on node i
        let chunk_name = format!(
            "backed.bin.part-{:03}-of-{:03}",
            owner_chunk_index + 1,
            parts
        );
        let backup_holder = &ring.nodes[i];
        let port = port_str(&backup_holder.node.port);
        let path = backup_holder
            .node
            .storage_root
            .join(port)
            .join("backup")
            .join(&chunk_name);

        let raw = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("backup missing on node {i} at {}: {e}", path.display()));
        // Series B trailer: the on-disk chunk is `body || sha256(body)`.
        // Strip the 32-byte trailer before hashing the body for comparison.
        assert!(
            raw.len() >= 32,
            "backup file shorter than trailer on node {i}: {} bytes",
            raw.len()
        );
        let body = &raw[..raw.len() - 32];
        let start = (owner_chunk_index as usize) * chunk_size;
        let end = start + chunk_size;
        assert_eq!(
            sha256(body),
            sha256(&bytes[start..end]),
            "backup on node {i} for chunk {} corrupt",
            owner_chunk_index + 1
        );
    }

    shutdown(ring).await;
}

// Pre-PR7 this test pinned that the saving node's primary save survived
// a dead predecessor (the backup push was best-effort). Under PR7's
// fan-out PUSH every non-start node is *also* a chunk target, so killing
// the predecessor of the start is the same as killing a chunk target —
// and the PUSH must fail fast rather than silently produce a half-saved
// file. The test's pre-PR7 contract no longer maps cleanly onto the new
// architecture; the surviving promises are covered by
// `failover_kill_one_then_pull` (a pre-kill push remains pullable via
// backup) and `backup_present_after_push` (backups land on predecessors
// when the ring is healthy). Removed.
