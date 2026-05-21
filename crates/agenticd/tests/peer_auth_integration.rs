//! Integration test for ADR-0012 socket peer-auth.
//!
//! Linux-only: macOS-native development paths use `--insecure-allow-any-uid`
//! and don't exercise the SO_PEERCRED code path. macOS does support
//! `getpeereid()` via tokio's `peer_cred()` but the negative-rejection
//! path is intentionally tested only via the `peer_auth::peer_auth_tests`
//! unit tests; a real OS-level rejection test would need a second UID.

#![cfg(target_os = "linux")]

use agentic_proto::framing::{read_frame, write_frame};
use agentic_proto::{Envelope, Request, Response};
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::UnixStream;

fn agenticd_bin() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_agenticd").into()
}

fn current_uid() -> u32 {
    // SAFETY: getuid() is always safe and always returns the calling
    // process's UID; no FFI invariants to maintain.
    unsafe { libc::getuid() }
}

async fn ping(sock_path: &std::path::Path) -> anyhow::Result<()> {
    let sock = UnixStream::connect(sock_path).await?;
    let (read, write) = sock.into_split();
    let mut reader = tokio::io::BufReader::new(read);
    let mut writer = tokio::io::BufWriter::new(write);
    write_frame(
        &mut writer,
        &Envelope {
            correlation_id: "t1".into(),
            payload: Request::Ping,
        },
    )
    .await?;
    let reply: Envelope<Response> = read_frame(&mut reader).await?.expect("response frame");
    assert!(matches!(reply.payload, Response::Pong));
    Ok(())
}

#[tokio::test]
async fn allowlisted_uid_can_ping() {
    let dir = TempDir::new().unwrap();
    let sock = dir.path().join("agenticd.sock");
    let uid = current_uid();
    let mut child = Command::new(agenticd_bin())
        .arg("--repo")
        .arg(dir.path())
        .arg("--socket")
        .arg(&sock)
        .arg("--allowed-uid")
        .arg(uid.to_string())
        .spawn()
        .expect("spawn agenticd");
    // Wait for the socket to appear (bounded).
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sock.exists(), "agenticd never created its socket");
    ping(&sock)
        .await
        .expect("ping should succeed under allowlisted UID");
    child.kill().ok();
    let _ = child.wait();
}
