//! Series E hardening probes: idle-timeout and max-conns.

mod common;

use std::time::Duration;

use common::{RingOpts, shutdown, spin_up};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Open a TCP connection to a node with idle-timeout configured but never
/// send any bytes. The server should respond `ERR idle timeout` and close
/// within the configured window.
#[tokio::test(flavor = "multi_thread")]
async fn idle_connection_is_dropped_with_err_line() {
    let ring = spin_up(RingOpts {
        n: 1,
        idle_timeout: Duration::from_millis(500),
        ..RingOpts::default()
    })
    .await;

    let start = tokio::time::Instant::now();
    let mut s = TcpStream::connect(ring.addr(0)).await.unwrap();
    // Don't write anything. Server should ERR + close in ~500 ms.
    let mut buf = String::new();
    let read = tokio::time::timeout(Duration::from_secs(3), s.read_to_string(&mut buf)).await;
    let elapsed = start.elapsed();

    assert!(read.is_ok(), "read never returned; server didn't drop us");
    assert!(
        elapsed < Duration::from_secs(2),
        "idle drop took too long: {elapsed:?}"
    );
    assert!(
        buf.starts_with("ERR idle timeout") || buf.is_empty(),
        "expected idle ERR or close; got: {buf:?}"
    );

    shutdown(ring).await;
}

/// A client that sends a complete command before the timeout should get a
/// normal response — the timeout only fires on inactivity, not on every
/// connection.
#[tokio::test(flavor = "multi_thread")]
async fn idle_timeout_does_not_break_legitimate_clients() {
    let ring = spin_up(RingOpts {
        n: 1,
        idle_timeout: Duration::from_millis(500),
        ..RingOpts::default()
    })
    .await;

    let mut s = TcpStream::connect(ring.addr(0)).await.unwrap();
    s.write_all(b"NODE PING\n").await.unwrap();
    s.shutdown().await.ok();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    assert!(
        buf.trim_end() == "PONG",
        "expected PONG with idle timeout enabled; got: {buf:?}"
    );

    shutdown(ring).await;
}

/// With max_conns=2, holding two connections open and opening a third
/// should yield `ERR server busy`. (max_conns=1 would block the harness's
/// own NETMAP DISCOVER + TOPOLOGY WALK during `spin_up`.)
#[tokio::test(flavor = "multi_thread")]
async fn max_conns_saturated_returns_busy() {
    let ring = spin_up(RingOpts {
        n: 1,
        max_conns: 2,
        // Keep idle_timeout off so the held connections aren't reaped.
        idle_timeout: Duration::ZERO,
        ..RingOpts::default()
    })
    .await;

    // Two connections: open, don't write, hold open.
    let _hold1 = TcpStream::connect(ring.addr(0)).await.unwrap();
    let _hold2 = TcpStream::connect(ring.addr(0)).await.unwrap();
    // Give the server a moment to register the spawns + permits.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Third connection: should be rejected with `ERR server busy\n`.
    let mut s3 = TcpStream::connect(ring.addr(0)).await.unwrap();
    let mut buf = String::new();
    let read = tokio::time::timeout(Duration::from_secs(2), s3.read_to_string(&mut buf)).await;
    assert!(read.is_ok(), "third connection should have been promptly closed");
    assert!(
        buf.starts_with("ERR server busy"),
        "expected busy ERR; got: {buf:?}"
    );

    shutdown(ring).await;
}

/// Sanity: with the cap disabled (default), many concurrent connections work.
#[tokio::test(flavor = "multi_thread")]
async fn max_conns_zero_disables_cap() {
    let ring = spin_up(RingOpts {
        n: 1,
        max_conns: 0,
        ..RingOpts::default()
    })
    .await;

    // 8 concurrent NODE PINGs should all succeed.
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let addr = ring.addr(0);
        tasks.push(tokio::spawn(async move {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(b"NODE PING\n").await.unwrap();
            s.shutdown().await.ok();
            let mut buf = String::new();
            s.read_to_string(&mut buf).await.unwrap();
            buf
        }));
    }
    for t in tasks {
        let buf = t.await.unwrap();
        assert!(buf.trim_end() == "PONG", "unexpected: {buf:?}");
    }

    shutdown(ring).await;
}
