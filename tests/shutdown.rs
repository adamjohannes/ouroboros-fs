//! Series G — graceful shutdown probes.
//!
//! These exercise the new `serve_with_shutdown` function: tests that
//! verify SIGTERM/SIGINT behavior end-to-end live in the subprocess
//! suite (#[ignore]); the in-process tests here cover the drain logic
//! by firing the shutdown channel directly.

mod common;

use std::sync::Arc;
use std::time::Duration;

use ouroboros_fs::{AuthToken, FsyncMode, Node, bind, serve_with_shutdown};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Basic contract: firing the shutdown signal causes serve_with_shutdown
/// to return. Without an in-flight handler this should be near-instant.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_signal_returns_serve_with_shutdown() {
    let tmp = TempDir::new().unwrap();
    let (node, listener, _addr) = bind(
        "127.0.0.1:0",
        Duration::ZERO,
        1 << 20,
        tmp.path().to_path_buf(),
        false,
        FsyncMode::None,
        AuthToken::disabled(),
        Duration::ZERO,
        0,
    )
    .await
    .unwrap();
    let _node: Arc<Node> = Arc::clone(&node);

    let (tx, rx) = tokio::sync::oneshot::channel();
    let serve_task = tokio::spawn(async move {
        serve_with_shutdown(node, listener, rx, Duration::from_secs(1)).await;
    });

    // Fire the signal and assert the task returns within a small window.
    tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), serve_task).await;
    assert!(
        result.is_ok(),
        "serve_with_shutdown didn't return after signal"
    );
}

/// Drain timeout: a handler that hangs past the drain deadline should be
/// aborted by `JoinSet::abort_all`. The serve_with_shutdown call returns
/// shortly after the deadline, not after the handler finally exits.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_drains_inflight_handler_within_timeout() {
    let tmp = TempDir::new().unwrap();
    let (node, listener, addr) = bind(
        "127.0.0.1:0",
        Duration::ZERO,
        1 << 20,
        tmp.path().to_path_buf(),
        false,
        FsyncMode::None,
        AuthToken::disabled(),
        Duration::ZERO, // no idle timeout — let the handler stall on read
        0,
    )
    .await
    .unwrap();

    let (tx, rx) = tokio::sync::oneshot::channel();
    let serve_task = tokio::spawn(async move {
        // Drain timeout 300 ms — short so the test runs fast.
        serve_with_shutdown(node, listener, rx, Duration::from_millis(300)).await;
    });

    // Open a connection and stall it: don't send any bytes. The handler
    // is now blocked in `read_line` (no idle timeout, no AUTH required).
    let _stalled = TcpStream::connect(addr).await.unwrap();
    // Brief settle so the handler is scheduled and reading.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let start = tokio::time::Instant::now();
    tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), serve_task).await;
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "serve_with_shutdown didn't return");
    // Should return within ~drain_timeout + small slack, not wait
    // forever for the stalled handler.
    assert!(
        elapsed < Duration::from_secs(1),
        "drain took too long: {elapsed:?}"
    );
}

/// In-flight handler with no idle timeout completes naturally before the
/// drain deadline: the drain should NOT need to abort. Verify by sending
/// a complete request before signal, then signaling.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_lets_quick_handler_finish() {
    let tmp = TempDir::new().unwrap();
    let (node, listener, addr) = bind(
        "127.0.0.1:0",
        Duration::ZERO,
        1 << 20,
        tmp.path().to_path_buf(),
        false,
        FsyncMode::None,
        AuthToken::disabled(),
        Duration::ZERO,
        0,
    )
    .await
    .unwrap();

    let (tx, rx) = tokio::sync::oneshot::channel();
    let serve_task = tokio::spawn(async move {
        serve_with_shutdown(node, listener, rx, Duration::from_secs(2)).await;
    });

    // Fire a complete NODE PING. The handler should finish promptly.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"NODE PING\n").await.unwrap();
    s.shutdown().await.ok();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    assert_eq!(buf.trim_end(), "PONG");

    tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), serve_task).await;
    assert!(result.is_ok());
}
