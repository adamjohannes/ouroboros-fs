//! Series B durability probes: fsync mode round-trip, atomic write-then-rename,
//! and the startup janitor that sweeps orphan `*.partial` files.

mod common;

use std::time::Duration;

use common::{RingOpts, pull_bytes, push_bytes, sha256, shutdown, spin_up};
use ouroboros_fs::{FsyncMode, bind, serve};
use tempfile::TempDir;

/// Round-trip with `FsyncMode::Full`. We can't actually verify fsync was
/// called from a test, but we can verify the protocol still works under the
/// stricter mode (no rename races, no missed bytes, no extra trailers
/// observable to the client).
#[tokio::test(flavor = "multi_thread")]
async fn fsync_full_round_trip_small_file() {
    let ring = spin_up(RingOpts {
        n: 3,
        fsync_mode: FsyncMode::Full,
        ..RingOpts::default()
    })
    .await;

    let payload = b"durable-bytes-here";
    push_bytes(ring.addr(0), "durable.bin", payload).await.unwrap();
    let got = pull_bytes(ring.addr(0), "durable.bin").await.unwrap();
    assert_eq!(sha256(&got), sha256(payload));

    shutdown(ring).await;
}

/// After a successful PUSH there must be no `*.partial` files left in any
/// node's content/ or backup/ directory. If one slips through, the rename
/// step or the atomic-write contract is broken.
#[tokio::test(flavor = "multi_thread")]
async fn no_partial_files_remain_after_push() {
    let ring = spin_up(RingOpts {
        n: 3,
        fsync_mode: FsyncMode::Data,
        ..RingOpts::default()
    })
    .await;

    push_bytes(ring.addr(0), "p.bin", &[0xAB; 4096]).await.unwrap();

    // Touch every node's storage tree.
    let storage = ring._tmp_path();
    let mut found_partials = Vec::new();
    let mut stack = vec![storage];
    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await.unwrap();
        while let Some(e) = entries.next_entry().await.unwrap() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|s| s.to_str()) == Some("partial") {
                found_partials.push(p);
            }
        }
    }

    assert!(
        found_partials.is_empty(),
        "expected no .partial files; found: {found_partials:?}"
    );

    shutdown(ring).await;
}

/// Pre-create an orphan `<chunk>.partial` in a node's content dir, then bind
/// the node fresh. The startup janitor must remove the orphan.
#[tokio::test(flavor = "multi_thread")]
async fn bind_sweeps_orphan_partials() {
    let tmp = TempDir::new().unwrap();
    let storage = tmp.path().join("ring");

    // Bind once to create the directory tree.
    let (node, listener, addr) = bind(
        "127.0.0.1:0",
        Duration::ZERO,
        1 << 20,
        storage.clone(),
        false,
        FsyncMode::None,
    )
    .await
    .unwrap();
    let port = addr.port().to_string();
    let serve_task = tokio::spawn({
        let n = std::sync::Arc::clone(&node);
        async move { serve(n, listener).await }
    });
    serve_task.abort();

    // Drop a bogus orphan partial and a bogus orphan backup partial.
    let content_dir = storage.join(&port).join("content");
    let backup_dir = storage.join(&port).join("backup");
    let orphan_content = content_dir.join("ghost.bin.partial");
    let orphan_backup = backup_dir.join("ghost.bin.partial");
    tokio::fs::write(&orphan_content, b"junk").await.unwrap();
    tokio::fs::write(&orphan_backup, b"junk").await.unwrap();

    // A file that does NOT have the .partial suffix must NOT be swept.
    let real = content_dir.join("realfile.bin");
    tokio::fs::write(&real, b"keep me").await.unwrap();

    // Re-bind; the janitor in `bind` should sweep the partials.
    let (_node2, listener2, _addr2) = bind(
        &format!("127.0.0.1:{port}"),
        Duration::ZERO,
        1 << 20,
        storage.clone(),
        false,
        FsyncMode::None,
    )
    .await
    .unwrap();
    drop(listener2);

    assert!(
        !orphan_content.exists(),
        "orphan content/.partial should have been swept"
    );
    assert!(
        !orphan_backup.exists(),
        "orphan backup/.partial should have been swept"
    );
    assert!(
        real.exists(),
        "non-partial file should not be touched by janitor"
    );
}

/// Bit-rot a chunk on disk between PUSH and PULL. The owner's GET-CHUNK
/// must detect the trailer-hash mismatch and respond with size=0, which
/// causes the puller to fall through to the predecessor's backup. End
/// result: the client sees the original bytes.
#[tokio::test(flavor = "multi_thread")]
async fn bit_rot_in_content_falls_through_to_backup() {
    let ring = spin_up(RingOpts {
        n: 3,
        ..RingOpts::default()
    })
    .await;

    let payload = b"clean payload bytes that will be rotted on disk";
    push_bytes(ring.addr(0), "rot.bin", payload).await.unwrap();

    // Rot chunk 0 (owned by node 0) by flipping a body byte. Trailer bytes
    // live at the end; we touch byte 0 to be sure we're inside the body.
    let port0 = ouroboros_fs::node::port_str(&ring.nodes[0].node.port).to_string();
    let chunk0 = ring.nodes[0]
        .node
        .storage_root
        .join(&port0)
        .join("content")
        .join("rot.bin.part-001-of-003");
    let mut bytes = tokio::fs::read(&chunk0).await.unwrap();
    assert!(
        bytes.len() > 32,
        "expected body+trailer; got {} bytes",
        bytes.len()
    );
    bytes[0] ^= 0xFF;
    tokio::fs::write(&chunk0, &bytes).await.unwrap();

    // Pull from node 1 so the puller has to fetch chunk 0 from node 0
    // (which serves size=0) and then recover from the predecessor backup.
    let got = pull_bytes(ring.addr(1), "rot.bin").await.unwrap();
    assert_eq!(
        got.len(),
        payload.len(),
        "expected full body via backup fall-through; got {} bytes",
        got.len()
    );
    assert_eq!(
        sha256(&got),
        sha256(payload),
        "expected backup fall-through to recover original bytes"
    );

    shutdown(ring).await;
}
