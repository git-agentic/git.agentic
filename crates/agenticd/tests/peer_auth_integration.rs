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
async fn non_allowlisted_uid_is_rejected() {
    let dir = TempDir::new().unwrap();
    let sock = dir.path().join("agenticd.sock");
    // Use a UID we are NOT — pick a large bogus UID to avoid accidentally
    // matching another running user.
    let bogus_uid: u32 = 999_999;
    assert_ne!(
        current_uid(),
        bogus_uid,
        "test assumes the test process is not running as uid 999999"
    );

    let mut child = Command::new(agenticd_bin())
        .arg("--repo")
        .arg(dir.path())
        .arg("--socket")
        .arg(&sock)
        .arg("--allowed-uid")
        .arg(bogus_uid.to_string())
        .spawn()
        .expect("spawn agenticd");
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sock.exists(), "agenticd never created its socket");

    // Connect from the test process (current_uid != bogus_uid). The daemon
    // should drop the connection without responding. We expect either the
    // connect to succeed and the subsequent read to hit EOF immediately,
    // OR the write to fail when we send the Ping (depends on timing).
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        let sock_conn = UnixStream::connect(&sock).await?;
        let (read, write) = sock_conn.into_split();
        let mut reader = tokio::io::BufReader::new(read);
        let mut writer = tokio::io::BufWriter::new(write);
        // Try to send a Ping; if write succeeds, expect read to hit EOF.
        let _ = write_frame(
            &mut writer,
            &Envelope {
                correlation_id: "rej".into(),
                payload: Request::Ping,
            },
        )
        .await;
        // Read should fail or return None (EOF) because the daemon dropped
        // the socket.
        let res: Result<Option<Envelope<Response>>, _> = read_frame(&mut reader).await;
        Ok::<_, anyhow::Error>(res)
    })
    .await;

    child.kill().ok();
    let _ = child.wait();

    let read_result =
        result.expect("connection attempt timed out — daemon hung instead of dropping");
    let frame =
        read_result.expect("read_frame returned a transport error rather than a clean drop");
    assert!(
        frame.is_none(),
        "daemon responded to a non-allowlisted UID; rejection path is broken. Got: {frame:?}"
    );
}

#[tokio::test]
async fn insecure_mode_does_not_attest_commits() {
    let dir = TempDir::new().unwrap();
    let sock = dir.path().join("agenticd.sock");
    std::fs::create_dir_all(dir.path().join(".agentic")).unwrap();

    let mut child = Command::new(agenticd_bin())
        .arg("--repo")
        .arg(dir.path())
        .arg("--socket")
        .arg(&sock)
        .arg("--insecure-allow-any-uid")
        .spawn()
        .expect("spawn agenticd");
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sock.exists(), "agenticd never created its socket");

    // Send a minimal Commit. no_memory=true skips the Postgres dependency.
    let commit_input = agentic_proto::CommitInput {
        message: "insecure-mode test".to_string(),
        author: Some("test".to_string()),
        code_sha: None,
        branch: Some("main".to_string()),
        prompts: std::collections::BTreeMap::new(),
        mcp_servers: Vec::new(),
        model: None,
        no_memory: true,
    };

    let conn = UnixStream::connect(&sock).await.unwrap();
    let (read, write) = conn.into_split();
    let mut reader = tokio::io::BufReader::new(read);
    let mut writer = tokio::io::BufWriter::new(write);
    write_frame(
        &mut writer,
        &Envelope {
            correlation_id: "c1".into(),
            payload: Request::Commit(commit_input),
        },
    )
    .await
    .unwrap();
    let reply: Envelope<Response> = read_frame(&mut reader)
        .await
        .unwrap()
        .expect("commit response");
    let commit_hash = match reply.payload {
        Response::Commit(o) => o.commit_hash,
        other => panic!("expected Commit response, got {other:?}"),
    };

    // Read back the Commit object.
    write_frame(
        &mut writer,
        &Envelope {
            correlation_id: "r1".into(),
            payload: Request::ReadObject { hash: commit_hash },
        },
    )
    .await
    .unwrap();
    let read_reply: Envelope<Response> = read_frame(&mut reader)
        .await
        .unwrap()
        .expect("read response");
    let (kind, data) = match read_reply.payload {
        Response::ObjectData {
            object_kind, data, ..
        } => (object_kind, data),
        other => panic!("expected ObjectData, got {other:?}"),
    };
    assert_eq!(kind, "commit");
    let commit_json: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert!(
        commit_json
            .get("peer_uid")
            .map(|v| v.is_null())
            .unwrap_or(true),
        "under --insecure-allow-any-uid the Commit blob must have peer_uid: null or omit it; got {}",
        commit_json["peer_uid"]
    );

    child.kill().ok();
    let _ = child.wait();
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
