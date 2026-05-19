//! Round-trip integration tests. Push a file to a ring, pull it back,
//! verify SHA-256 matches.

mod common;

use std::time::Duration;

use common::{RingOpts, pull_bytes, push_bytes, rand_bytes, sha256, shutdown, spin_up};

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_small_3node() {
    let ring = spin_up(RingOpts {
        n: 3,
        ..RingOpts::default()
    })
    .await;

    let bytes = rand_bytes(/*seed=*/ 1, 4 * 1024);
    let want = sha256(&bytes);

    push_bytes(ring.addr(0), "small.bin", &bytes).await.unwrap();
    let got = pull_bytes(ring.addr(0), "small.bin").await.unwrap();
    assert_eq!(got.len(), bytes.len(), "length mismatch");
    assert_eq!(sha256(&got), want, "SHA-256 mismatch");

    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_small_5node() {
    let ring = spin_up(RingOpts {
        n: 5,
        ..RingOpts::default()
    })
    .await;

    let bytes = rand_bytes(2, 16 * 1024);
    let want = sha256(&bytes);

    push_bytes(ring.addr(0), "five.bin", &bytes).await.unwrap();
    let got = pull_bytes(ring.addr(0), "five.bin").await.unwrap();
    assert_eq!(got.len(), bytes.len());
    assert_eq!(sha256(&got), want);

    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_two_files() {
    let ring = spin_up(RingOpts {
        n: 3,
        ..RingOpts::default()
    })
    .await;

    let a = rand_bytes(10, 2048);
    let b = rand_bytes(11, 4096);
    let want_a = sha256(&a);
    let want_b = sha256(&b);

    push_bytes(ring.addr(0), "a.bin", &a).await.unwrap();
    push_bytes(ring.addr(0), "b.bin", &b).await.unwrap();

    let got_a = pull_bytes(ring.addr(0), "a.bin").await.unwrap();
    let got_b = pull_bytes(ring.addr(0), "b.bin").await.unwrap();
    assert_eq!(sha256(&got_a), want_a);
    assert_eq!(sha256(&got_b), want_b);

    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrency_stress_concurrent_pulls() {
    use futures::stream::{FuturesUnordered, StreamExt};

    let ring = spin_up(RingOpts {
        n: 5,
        ..RingOpts::default()
    })
    .await;

    let bytes = rand_bytes(20, 1 << 20); // 1 MiB
    let want = sha256(&bytes);
    push_bytes(ring.addr(0), "concurrent.bin", &bytes)
        .await
        .unwrap();

    let mut tasks = FuturesUnordered::new();
    for _ in 0..8 {
        let addr = ring.addr(0);
        tasks.push(async move { pull_bytes(addr, "concurrent.bin").await });
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while let Some(r) = tasks.next().await {
        let got = r.expect("pull failed");
        assert_eq!(sha256(&got), want, "one of the concurrent pulls corrupted");
        assert!(
            tokio::time::Instant::now() < deadline,
            "concurrent pulls exceeded deadline"
        );
    }

    shutdown(ring).await;
}

/// PR2 regression-pin. Run several PULLs in parallel with a NETMAP DISCOVER
/// + TOPOLOGY WALK that touch the topology RwLock as writers. Pre-PR2 the
/// pull held a read guard for the entire chunk-fetch loop, blocking the
/// writes and risking deadlock under load. Post-PR2 the pull snapshots once
/// and drops the guard.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_pull_with_heal() {
    use futures::stream::{FuturesUnordered, StreamExt};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    let ring = spin_up(RingOpts {
        n: 5,
        ..RingOpts::default()
    })
    .await;

    let bytes = rand_bytes(/*seed=*/ 50, 4 * 1024 * 1024);
    let want = sha256(&bytes);
    push_bytes(ring.addr(0), "heal_target.bin", &bytes)
        .await
        .unwrap();

    let addr0 = ring.addr(0);

    // Fire 4 pulls concurrently with a periodic NETMAP DISCOVER + TOPOLOGY WALK.
    let mut pulls = FuturesUnordered::new();
    for _ in 0..4 {
        pulls.push(async move { pull_bytes(addr0, "heal_target.bin").await });
    }

    let chatter = tokio::spawn(async move {
        for _ in 0..6 {
            if let Ok(mut s) = TcpStream::connect(addr0).await {
                let _ = s.write_all(b"NETMAP DISCOVER\n").await;
                let _ = s.shutdown().await;
            }
            if let Ok(mut s) = TcpStream::connect(addr0).await {
                let _ = s.write_all(b"TOPOLOGY WALK\n").await;
                let _ = s.shutdown().await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while let Some(r) = pulls.next().await {
        let got = r.expect("pull failed during heal chatter");
        assert_eq!(
            sha256(&got),
            want,
            "concurrent pull/heal corrupted bytes"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "pulls didn't make progress while heal commands ran"
        );
    }
    chatter.await.unwrap();

    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrency_stress_push_pull_same_file() {
    let ring = spin_up(RingOpts {
        n: 3,
        ..RingOpts::default()
    })
    .await;

    let v1 = rand_bytes(30, 8 * 1024);
    let v2 = rand_bytes(31, 8 * 1024);
    let want_v1 = sha256(&v1);
    let want_v2 = sha256(&v2);

    // Seed with v1 and confirm it's retrievable before introducing the race.
    push_bytes(ring.addr(0), "shared.bin", &v1).await.unwrap();
    let warm = pull_bytes(ring.addr(0), "shared.bin").await.unwrap();
    assert_eq!(sha256(&warm), want_v1);

    // Concurrently push v2 and pull. **OuroborosFS does NOT promise atomic
    // per-file replacement** — the per-chunk relay is asynchronous and a
    // parallel pull can interleave bytes from both versions. The contract
    // we verify here is just "neither op hangs and neither side crashes the
    // node." Discard `pulled` content; PR7 will tighten this.
    let addr0 = ring.addr(0);
    let v2_clone = v2.clone();
    let push_task = tokio::spawn(async move {
        push_bytes(addr0, "shared.bin", &v2_clone).await
    });
    let pull_task = tokio::spawn(async move { pull_bytes(addr0, "shared.bin").await });

    let push_res = push_task.await.unwrap();
    let pull_res = pull_task.await.unwrap();
    push_res.expect("push failed");
    pull_res.expect("pull failed");
    let _ = (want_v1, want_v2); // silence unused warnings until PR7

    shutdown(ring).await;
}

/// Large-file streaming check. Run with `cargo test --release -- --ignored`.
/// Pre-PR3 this is allowed to be slow; PR3 adds an RSS sentinel under cfg(linux).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn large_file_streaming_100mb() {
    let ring = spin_up(RingOpts {
        n: 5,
        ..RingOpts::default()
    })
    .await;

    let bytes = rand_bytes(42, 100 * 1024 * 1024);
    let want = sha256(&bytes);

    push_bytes(ring.addr(0), "huge.bin", &bytes).await.unwrap();
    let got = pull_bytes(ring.addr(0), "huge.bin").await.unwrap();
    assert_eq!(got.len(), bytes.len());
    assert_eq!(sha256(&got), want);

    shutdown(ring).await;
}

/// PR3 regression pin. When chunk N (with 0 < N < parts-1) is unreachable
/// AND its predecessor (backup holder) is also dead, the streaming PULL
/// emits the prefix bytes correctly and then *omits* the missing chunk —
/// no zero-padding. The output is short, but everything before the gap is
/// byte-identical to the original. Pre-PR3 the same property held but was
/// hidden behind a Vec<u8> accumulator; this test pins prefix-correctness
/// so future changes to the streaming path (PR4 in particular) don't
/// silently break it.
#[tokio::test(flavor = "multi_thread")]
async fn pull_dead_chunk_is_short_not_zero_padded() {
    use std::time::Duration;

    let mut ring = spin_up(RingOpts {
        n: 5,
        gossip_interval: Duration::from_millis(200),
        ..RingOpts::default()
    })
    .await;

    // 5 chunks across 5 nodes; chunk i lives on node i.
    let total: u64 = 5 * 4096; // 20 KiB, evenly divisible
    let bytes = rand_bytes(/*seed=*/ 60, total as usize);
    push_bytes(ring.addr(0), "doomed.bin", &bytes).await.unwrap();

    // Allow backup notification to populate predecessors before killing.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Kill chunk-2's owner AND chunk-2's predecessor (which holds its
    // backup). Both content and backup are now unreachable.
    common::kill_node(&mut ring, 2).await;
    common::kill_node(&mut ring, 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let got = pull_bytes(ring.addr(0), "doomed.bin")
        .await
        .expect("pull should complete even when the chunk is irretrievable");

    // Chunks evenly divide; each is 4 KiB.
    let chunk_len = (total / 5) as usize;
    let prefix_end = 2 * chunk_len; // bytes 0..8192 (chunks 0 and 1)

    assert!(
        got.len() < bytes.len(),
        "expected short output (got {} bytes, input {})",
        got.len(),
        bytes.len()
    );
    assert!(
        got.len() >= prefix_end,
        "lost the prefix too: got {} bytes, expected >= {}",
        got.len(),
        prefix_end
    );
    assert_eq!(
        &got[..prefix_end],
        &bytes[..prefix_end],
        "prefix bytes (chunks 0..2) corrupted by the streaming refactor"
    );

    common::shutdown(ring).await;
}

/// PR7 fan-out happy path. Same shape as `happy_path_small_5node` but
/// pinned to the new code path explicitly so a future refactor that
/// regresses fan-out is caught by name.
#[tokio::test(flavor = "multi_thread")]
async fn fanout_push_basic() {
    let ring = spin_up(RingOpts {
        n: 5,
        ..RingOpts::default()
    })
    .await;
    let bytes = rand_bytes(/*seed=*/ 70, 16 * 1024);
    let want = sha256(&bytes);
    push_bytes(ring.addr(0), "fanout.bin", &bytes).await.unwrap();
    let got = pull_bytes(ring.addr(0), "fanout.bin").await.unwrap();
    assert_eq!(sha256(&got), want);
    shutdown(ring).await;
}

/// PR7 fan-out fails fast when any chunk target is unreachable. Pre-PR7
/// the relay chain would deliver the start node's chunk and silently lose
/// the others; PR7 surfaces the failure synchronously so the client can
/// retry.
#[tokio::test(flavor = "multi_thread")]
async fn fanout_push_dead_target_returns_err() {
    let mut ring = spin_up(RingOpts {
        n: 5,
        ..RingOpts::default()
    })
    .await;

    // Kill a chunk-target node before the push.
    common::kill_node(&mut ring, 2).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let bytes = rand_bytes(71, 16 * 1024);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        push_bytes(ring.addr(0), "broken.bin", &bytes),
    )
    .await
    .expect("push hung; fan-out should fail fast on dead target");

    assert!(
        result.is_err(),
        "expected push to fail when a chunk target is dead"
    );
    shutdown(ring).await;
}

/// PR7 preserves the parts==1 short-circuit: a single-node ring saves the
/// whole file locally without any fan-out connections.
#[tokio::test(flavor = "multi_thread")]
async fn fanout_push_parts_eq_one() {
    let ring = spin_up(RingOpts {
        n: 1,
        ..RingOpts::default()
    })
    .await;
    let bytes = rand_bytes(72, 8 * 1024);
    let want = sha256(&bytes);
    push_bytes(ring.addr(0), "alone.bin", &bytes).await.unwrap();
    let got = pull_bytes(ring.addr(0), "alone.bin").await.unwrap();
    assert_eq!(sha256(&got), want);
    shutdown(ring).await;
}

/// PR5 wire-protocol pin. The relay header now carries `consumed` so
/// receivers can compute their slice without re-walking
/// `sum_len_up_to_inclusive` per hop. Push a file with a deliberately
/// non-uniform per-hop size mix, pull, SHA-256 — verifies every relay hop
/// honored its `consumed` value.
#[tokio::test(flavor = "multi_thread")]
async fn relay_consumed_field_honored() {
    // 3-node ring, 10-byte file -> chunk sizes [4, 3, 3]. Hop 1 receives
    // consumed=4, hop 2 receives consumed=7. Pre-PR5 each hop recomputed
    // the sum; post-PR5 they trust the header.
    let ring = spin_up(RingOpts {
        n: 3,
        ..RingOpts::default()
    })
    .await;

    let bytes: Vec<u8> = (0u8..10).collect();
    let want = sha256(&bytes);

    push_bytes(ring.addr(0), "consumed.bin", &bytes).await.unwrap();
    let got = pull_bytes(ring.addr(0), "consumed.bin").await.unwrap();
    assert_eq!(got.len(), bytes.len());
    assert_eq!(sha256(&got), want);

    shutdown(ring).await;
}
