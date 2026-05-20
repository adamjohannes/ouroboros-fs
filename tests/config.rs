//! §4.5 — `--config` flag tests. Spawn the binary with a TOML config and
//! verify it reads the values; pass an explicit CLI flag and verify it
//! overrides.

use std::io::Write;
use std::process::Command;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn release_bin() -> std::path::PathBuf {
    let p = std::path::PathBuf::from("target/release/ouroboros_fs");
    if !p.exists() {
        // Try debug as fallback so `cargo test` works without --release.
        let d = std::path::PathBuf::from("target/debug/ouroboros_fs");
        if d.exists() {
            return d;
        }
    }
    p
}

/// Pick a port unlikely to collide with parallel test runs.
fn pick_port() -> u16 {
    20_000 + (std::process::id() % 10_000) as u16 + 5
}

#[tokio::test(flavor = "multi_thread")]
async fn config_file_supplies_addr_when_cli_omits_it() {
    let exe = release_bin();
    if !exe.exists() {
        eprintln!("skipping: {} not built", exe.display());
        return;
    }

    let port = pick_port();
    let storage = tempfile::tempdir().unwrap();
    let mut cfg = NamedTempFile::new().unwrap();
    writeln!(
        cfg,
        r#"
addr = "127.0.0.1:{port}"
storage_root = "{}"
wait_time = 200
fsync_mode = "none"
"#,
        storage.path().display()
    )
    .unwrap();

    let mut child = Command::new(&exe)
        .arg("run")
        .arg("--config")
        .arg(cfg.path())
        .spawn()
        .expect("spawn");

    // Wait until the configured port is listening.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut bound = false;
    while tokio::time::Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Probe NODE PING to confirm it's our binary.
    if bound {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(b"NODE PING\n").await.unwrap();
        s.shutdown().await.ok();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        assert_eq!(resp.trim_end(), "PONG");
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(bound, "child did not bind {port} from config");
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_flag_overrides_config_file() {
    let exe = release_bin();
    if !exe.exists() {
        eprintln!("skipping: {} not built", exe.display());
        return;
    }

    let cli_port = pick_port() + 1;
    let cfg_port = cli_port + 1;
    let storage = tempfile::tempdir().unwrap();
    let mut cfg = NamedTempFile::new().unwrap();
    writeln!(
        cfg,
        r#"
addr = "127.0.0.1:{cfg_port}"
storage_root = "{}"
wait_time = 200
fsync_mode = "none"
"#,
        storage.path().display()
    )
    .unwrap();

    let mut child = Command::new(&exe)
        .arg("run")
        .arg("--config")
        .arg(cfg.path())
        .arg("--addr")
        .arg(format!("127.0.0.1:{cli_port}"))
        .spawn()
        .expect("spawn");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut bound_cli = false;
    while tokio::time::Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", cli_port)).await.is_ok() {
            bound_cli = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The cfg_port must NOT be bound — CLI wins.
    let cfg_bound = TcpStream::connect(("127.0.0.1", cfg_port)).await.is_ok();

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        bound_cli,
        "CLI --addr should have won; child not on {cli_port}"
    );
    assert!(
        !cfg_bound,
        "config addr {cfg_port} should NOT be bound when CLI overrides"
    );
}
