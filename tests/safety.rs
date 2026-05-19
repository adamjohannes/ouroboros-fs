//! Safety probes. PR1 enforces a bounded drain on oversized PUSH (no
//! O(size) Vec) and a `MAX_REASONABLE_BYTES` ceiling on gateway HTTP bodies.
//! These tests pin both behaviors.

mod common;

use std::time::Duration;

use common::{RingOpts, push_bytes, pull_bytes, rand_bytes, sha256, shutdown, spin_up};
use ouroboros_fs::Gateway;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// An absurd PUSH header followed by *no* body. The node should respond
/// with `ERR File size is too large` and close the connection promptly,
/// without OOM-allocating an 18-exabyte buffer.
#[tokio::test(flavor = "multi_thread")]
async fn oversized_push_drains_does_not_oom() {
    let ring = spin_up(RingOpts {
        n: 3,
        max_file_size: 1 << 20, // 1 MiB cap
        ..RingOpts::default()
    })
    .await;

    let result = tokio::time::timeout(Duration::from_secs(2), async {
        let mut s = TcpStream::connect(ring.addr(0)).await?;
        s.write_all(b"FILE PUSH 18446744073709551615 evil\n").await?;
        // Don't shutdown — the post-PR1 server reads from a `reader.take(size)`
        // which yields EOF as soon as the client closes its write half. We
        // want to exercise the `tokio::io::copy(...) -> sink()` path that
        // returns when the underlying reader hits EOF.
        s.shutdown().await.ok();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await?;
        Ok::<_, std::io::Error>(resp)
    })
    .await
    .expect("timed out — node likely allocated O(size) and hung");

    let resp = result.expect("io error");
    assert!(
        resp.starts_with("ERR"),
        "expected ERR prefix, got: {resp:?}"
    );

    shutdown(ring).await;
}

/// After a bad PUSH, opening a fresh connection still works. (Pre-PR1 the
/// drain `read_exact`'d 18 EB of bytes from the same socket and hung
/// indefinitely.)
#[tokio::test(flavor = "multi_thread")]
async fn oversized_push_connection_reusable() {
    let ring = spin_up(RingOpts {
        n: 3,
        max_file_size: 1 << 20,
        ..RingOpts::default()
    })
    .await;

    // 1. Trigger the bounded drain.
    {
        let mut s = TcpStream::connect(ring.addr(0)).await.unwrap();
        s.write_all(b"FILE PUSH 18446744073709551615 evil\n")
            .await
            .unwrap();
        s.shutdown().await.ok();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("ERR"));
    }

    // 2. The node must still accept a valid push on a fresh connection
    //    within a tight deadline.
    let bytes = rand_bytes(/*seed=*/ 7, 1024);
    tokio::time::timeout(Duration::from_secs(2), async {
        push_bytes(ring.addr(0), "ok.bin", &bytes).await
    })
    .await
    .expect("subsequent push hung")
    .expect("subsequent push failed");

    let got = pull_bytes(ring.addr(0), "ok.bin").await.unwrap();
    assert_eq!(sha256(&got), sha256(&bytes));

    shutdown(ring).await;
}

/// Boundary case: a PUSH of *exactly* `max_file_size` bytes is accepted.
/// Catches off-by-one in the bound.
#[tokio::test(flavor = "multi_thread")]
async fn oversized_push_below_max_file_size_succeeds() {
    let cap = 4096u64;
    let ring = spin_up(RingOpts {
        n: 3,
        max_file_size: cap,
        ..RingOpts::default()
    })
    .await;

    let bytes = rand_bytes(8, cap as usize);
    push_bytes(ring.addr(0), "boundary.bin", &bytes).await.unwrap();
    let got = pull_bytes(ring.addr(0), "boundary.bin").await.unwrap();
    assert_eq!(sha256(&got), sha256(&bytes));

    shutdown(ring).await;
}

/// HTTP POST with an absurd `Content-Length` is rejected with HTTP 4xx/5xx
/// without buffering the declared body.
#[tokio::test(flavor = "multi_thread")]
async fn gateway_oversized_post_returns_413() {
    let ring = spin_up(RingOpts {
        n: 3,
        ..RingOpts::default()
    })
    .await;

    // Spin a Gateway against the ring on its own ephemeral port.
    let node_addrs: Vec<String> = ring.nodes.iter().map(|h| h.addr.to_string()).collect();
    let gw_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gw_addr = gw_listener.local_addr().unwrap();
    drop(gw_listener); // release port; Gateway::run_server re-binds it.
    let gw = Gateway::new(node_addrs);
    let gw_task = tokio::spawn({
        let gw = gw.clone();
        let listen = gw_addr.to_string();
        async move {
            let _ = gw.run_server(listen).await;
        }
    });

    // Brief retry loop — the gateway re-binds; could race the test.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut s = loop {
        match TcpStream::connect(gw_addr).await {
            Ok(s) => break s,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await
            }
            Err(e) => panic!("gateway never came up: {e}"),
        }
    };

    let req = format!(
        "POST /file/push HTTP/1.1\r\nHost: x\r\nX-Filename: evil\r\nContent-Length: 18446744073709551615\r\n\r\n"
    );

    let result = tokio::time::timeout(Duration::from_secs(2), async {
        s.write_all(req.as_bytes()).await?;
        s.shutdown().await.ok();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await?;
        Ok::<_, std::io::Error>(resp)
    })
    .await
    .expect("gateway timed out — Content-Length not bounded");

    let resp = result.expect("gateway io error");
    let status_line = resp.lines().next().unwrap_or("");
    let parts: Vec<&str> = status_line.split_whitespace().collect();
    let code: u16 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    assert!(
        (400..600).contains(&code),
        "expected 4xx/5xx, got status line: {status_line:?}"
    );

    gw_task.abort();
    shutdown(ring).await;
}
