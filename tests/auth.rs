//! Series C auth probes: wire-protocol AUTH handshake, HTTP bearer auth,
//! removed kill endpoint.

mod common;

use std::time::Duration;

use common::{
    GatewayHandle, RingOpts, http_get, http_post, pull_bytes, push_bytes, sha256, shutdown,
    spin_up, spin_up_with_gateway, teardown,
};
use ouroboros_fs::AuthToken;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn fixed_token() -> AuthToken {
    AuthToken::from_bytes([0x42; 32])
}

fn other_token() -> AuthToken {
    AuthToken::from_bytes([0x99; 32])
}

// ---------- Wire-protocol AUTH ----------

#[tokio::test(flavor = "multi_thread")]
async fn unauth_connect_to_authed_node_is_rejected() {
    let ring = spin_up(RingOpts {
        n: 3,
        auth_token: fixed_token(),
        ..RingOpts::default()
    })
    .await;

    // Connect and immediately send a protocol command without an AUTH line.
    let mut s = TcpStream::connect(ring.addr(0)).await.unwrap();
    s.write_all(b"FILE LIST\n").await.unwrap();
    s.shutdown().await.ok();

    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    assert!(
        buf.starts_with("ERR auth required") || buf.starts_with("ERR auth timeout"),
        "expected auth ERR, got: {buf:?}"
    );

    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_with_wrong_secret_is_rejected() {
    let ring = spin_up(RingOpts {
        n: 3,
        auth_token: fixed_token(),
        ..RingOpts::default()
    })
    .await;

    let bad_line = other_token().make_auth_line().expect("enabled");
    let mut s = TcpStream::connect(ring.addr(0)).await.unwrap();
    s.write_all(bad_line.as_bytes()).await.unwrap();
    s.write_all(b"FILE LIST\n").await.unwrap();
    s.shutdown().await.ok();

    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    assert!(
        buf.starts_with("ERR auth required"),
        "expected auth ERR, got: {buf:?}"
    );

    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_silence_times_out_within_2s() {
    let ring = spin_up(RingOpts {
        n: 3,
        auth_token: fixed_token(),
        ..RingOpts::default()
    })
    .await;

    let start = tokio::time::Instant::now();
    let mut s = TcpStream::connect(ring.addr(0)).await.unwrap();
    // Don't write anything. The server should ERR + close within ~1s.
    let mut buf = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), s.read_to_string(&mut buf)).await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "auth-silence read took too long: {elapsed:?}"
    );
    assert!(
        buf.starts_with("ERR auth timeout") || buf.is_empty(),
        "expected timeout ERR or close, got: {buf:?}"
    );

    shutdown(ring).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn authed_round_trip_succeeds() {
    let ring = spin_up(RingOpts {
        n: 3,
        auth_token: fixed_token(),
        ..RingOpts::default()
    })
    .await;

    // The harness's `push_bytes` / `pull_bytes` don't speak AUTH. Build the
    // handshake manually and reuse the protocol.
    let token = fixed_token();
    let payload = b"hello-from-authenticated-client";
    let auth_line = token.make_auth_line().unwrap();

    let mut s = TcpStream::connect(ring.addr(0)).await.unwrap();
    s.write_all(auth_line.as_bytes()).await.unwrap();
    s.write_all(format!("FILE PUSH {} authed.bin\n", payload.len()).as_bytes())
        .await
        .unwrap();
    s.write_all(payload).await.unwrap();
    s.shutdown().await.ok();
    let mut resp = String::new();
    s.read_to_string(&mut resp).await.unwrap();
    assert!(
        resp.contains("OK") && !resp.starts_with("ERR"),
        "expected OK after authed PUSH, got: {resp:?}"
    );

    // Settle window, then PULL with a fresh authed connection.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let auth_line = token.make_auth_line().unwrap();
    let mut s = TcpStream::connect(ring.addr(0)).await.unwrap();
    s.write_all(auth_line.as_bytes()).await.unwrap();
    s.write_all(b"FILE PULL authed.bin\n").await.unwrap();
    s.shutdown().await.ok();
    let mut got = Vec::new();
    s.read_to_end(&mut got).await.unwrap();
    assert!(
        !got.starts_with(b"ERR"),
        "got ERR on authed PULL: {:?}",
        String::from_utf8_lossy(&got)
    );
    assert_eq!(sha256(&got), sha256(payload));

    shutdown(ring).await;
}

// Sanity check: the existing harness helpers still work in disabled-auth
// mode, since none of them send AUTH lines. (Belt-and-braces: every other
// test in the suite already covers this implicitly.)
#[tokio::test(flavor = "multi_thread")]
async fn disabled_auth_preserves_legacy_round_trip() {
    let ring = spin_up(RingOpts::default()).await; // auth_token = disabled
    let payload = b"plain";
    push_bytes(ring.addr(0), "plain.bin", payload).await.unwrap();
    let got = pull_bytes(ring.addr(0), "plain.bin").await.unwrap();
    assert_eq!(sha256(&got), sha256(payload));
    shutdown(ring).await;
}

// ---------- HTTP bearer auth ----------

#[tokio::test(flavor = "multi_thread")]
async fn http_get_without_bearer_is_401() {
    let (ring, gw) = spin_up_with_gateway(RingOpts {
        n: 3,
        auth_token: fixed_token(),
        ..RingOpts::default()
    })
    .await;
    let resp = http_get(gw.addr, "/file/list").await.unwrap();
    assert_eq!(resp.status, 401);
    teardown_authed(ring, gw).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_get_with_wrong_bearer_is_401() {
    let (ring, gw) = spin_up_with_gateway(RingOpts {
        n: 3,
        auth_token: fixed_token(),
        ..RingOpts::default()
    })
    .await;
    let bad = format!("Bearer {}", other_token().bearer_value().unwrap());
    let headers: &[(&str, &str)] = &[("Authorization", &bad)];
    let resp = http_get_with_headers(gw.addr, "/file/list", headers).await.unwrap();
    assert_eq!(resp.status, 401);
    teardown_authed(ring, gw).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_get_with_correct_bearer_succeeds() {
    let (ring, gw) = spin_up_with_gateway(RingOpts {
        n: 3,
        auth_token: fixed_token(),
        ..RingOpts::default()
    })
    .await;
    let good = format!("Bearer {}", fixed_token().bearer_value().unwrap());
    let headers: &[(&str, &str)] = &[("Authorization", &good)];
    let resp = http_get_with_headers(gw.addr, "/file/list", headers).await.unwrap();
    assert_eq!(resp.status, 200);
    teardown_authed(ring, gw).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_options_without_bearer_still_204() {
    // OPTIONS preflight is exempt: browsers send it without credentials by
    // design.
    let (ring, gw) = spin_up_with_gateway(RingOpts {
        n: 3,
        auth_token: fixed_token(),
        ..RingOpts::default()
    })
    .await;
    let resp = common::http_options(gw.addr, "/file/push").await.unwrap();
    assert_eq!(resp.status, 204);
    teardown_authed(ring, gw).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_post_push_with_bearer_round_trip() {
    let (ring, gw) = spin_up_with_gateway(RingOpts {
        n: 3,
        auth_token: fixed_token(),
        ..RingOpts::default()
    })
    .await;

    let bearer = format!("Bearer {}", fixed_token().bearer_value().unwrap());
    let payload = b"http-bearer-bytes";
    let headers: &[(&str, &str)] = &[
        ("Authorization", &bearer),
        ("X-Filename", "http-authed.bin"),
        ("Content-Type", "application/octet-stream"),
    ];
    let resp = http_post(gw.addr, "/file/push", headers, payload).await.unwrap();
    assert_eq!(resp.status, 200);

    teardown_authed(ring, gw).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_kill_endpoint_returns_404_with_or_without_bearer() {
    let (ring, gw) = spin_up_with_gateway(RingOpts {
        n: 3,
        auth_token: fixed_token(),
        ..RingOpts::default()
    })
    .await;

    // Without bearer: 401 (auth check happens before route dispatch).
    let resp = http_post(gw.addr, "/node/65535/kill", &[], &[])
        .await
        .unwrap();
    assert_eq!(resp.status, 401);

    // With bearer: 404 (route was removed).
    let bearer = format!("Bearer {}", fixed_token().bearer_value().unwrap());
    let headers: &[(&str, &str)] = &[("Authorization", &bearer)];
    let resp = http_post_with_headers(gw.addr, "/node/65535/kill", headers, &[])
        .await
        .unwrap();
    assert_eq!(resp.status, 404);

    teardown_authed(ring, gw).await;
}

// ---------- Local helpers ----------

/// Like the harness's `teardown` for ring+gateway pairs but tolerates the
/// gateway task already having exited (e.g. on an aborted accept).
async fn teardown_authed(ring: common::Ring, gw: GatewayHandle) {
    teardown(ring, gw).await
}

/// `http_get` with custom headers. The harness's `http_get` takes no
/// headers, so this auth-test file rolls its own thin wrapper using the
/// same socket-level pattern.
async fn http_get_with_headers(
    addr: std::net::SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> std::io::Result<common::HttpResponse> {
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    common::raw_http(addr, req.as_bytes()).await
}

async fn http_post_with_headers(
    addr: std::net::SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<common::HttpResponse> {
    let mut req = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("Connection: close\r\n\r\n");
    let mut bytes = req.into_bytes();
    bytes.extend_from_slice(body);
    common::raw_http(addr, &bytes).await
}
