//! ADR-0019 Decision 4: `agentic rollback` attaches an approval token on its
//! own when it can read the approval key. The CLI binary runs against a fake
//! daemon that answers `ResolveRef` and records every `Rollback` request.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use agentic_core::approval::{verify_token, ApprovalKey};
use agentic_proto::framing::{read_frame, write_frame};
use agentic_proto::{Envelope, Request, Response, RollbackOutput};
use tokio::net::UnixListener;

const KEY: [u8; 32] = [9u8; 32];

fn target_hash() -> String {
    "ab".repeat(32)
}

type Seen = Arc<Mutex<Vec<Option<String>>>>;

/// Serve forever on `sock` in a background thread; returns the approval
/// tokens (one entry per `Rollback` request) the CLI sent.
fn fake_daemon(sock: &Path) -> Seen {
    let seen: Seen = Arc::default();
    let recorded = Arc::clone(&seen);
    let sock = sock.to_path_buf();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = UnixListener::bind(&sock).unwrap();
            ready_tx.send(()).unwrap();
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let (r, w) = s.split();
                let mut r = tokio::io::BufReader::new(r);
                let mut w = tokio::io::BufWriter::new(w);
                let Some(env): Option<Envelope<Request>> = read_frame(&mut r).await.unwrap() else {
                    continue;
                };
                let reply = match env.payload {
                    Request::ResolveRef { .. } => Response::ResolveRef {
                        hash: target_hash(),
                    },
                    Request::Rollback { approval_token, .. } => {
                        recorded.lock().unwrap().push(approval_token);
                        Response::Rollback(RollbackOutput {
                            planned_steps: Vec::new(),
                            executed: true,
                            new_head_hash: Some(target_hash()),
                        })
                    }
                    other => panic!("unexpected request {other:?}"),
                };
                write_frame(&mut w, &Envelope::new(env.correlation_id, reply))
                    .await
                    .unwrap();
            }
        });
    });
    ready_rx.recv().unwrap();
    seen
}

struct Fixture {
    repo: tempfile::TempDir,
    sock_dir: tempfile::TempDir,
    seen: Seen,
}

fn fixture() -> Fixture {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".agentic")).unwrap();
    let sock_dir = tempfile::Builder::new()
        .prefix("agc")
        .tempdir_in("/tmp")
        .unwrap();
    let seen = fake_daemon(&sock_dir.path().join("d.sock"));
    Fixture {
        repo,
        sock_dir,
        seen,
    }
}

fn run_rollback(f: &Fixture, extra: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_agentic"))
        .arg("--repo")
        .arg(f.repo.path())
        .args(["rollback", "main", "--yes"])
        .args(extra)
        .env("AGENTIC_SOCKET", f.sock_dir.path().join("d.sock"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "agentic rollback failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn attaches_a_token_from_the_default_key_file() {
    let f = fixture();
    std::fs::write(f.repo.path().join(".agentic/approval.key"), KEY).unwrap();
    run_rollback(&f, &[]);

    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let token = seen[0].as_deref().expect("token must be attached");
    let key = ApprovalKey::from_bytes(&KEY).unwrap();
    verify_token(&key, &target_hash(), euid(), token, now())
        .expect("token must verify for the resolved commit and the caller's uid");
}

#[test]
fn attaches_a_token_from_the_key_file_flag() {
    let f = fixture();
    let key_path = f.repo.path().join("custom.key");
    std::fs::write(&key_path, KEY).unwrap();
    run_rollback(&f, &["--key-file", key_path.to_str().unwrap()]);
    assert!(f.seen.lock().unwrap()[0].is_some());
}

#[test]
fn sends_no_token_without_a_key() {
    let f = fixture();
    run_rollback(&f, &[]);
    assert_eq!(*f.seen.lock().unwrap(), vec![None]);
}

#[test]
fn explicit_token_is_not_replaced() {
    let f = fixture();
    std::fs::write(f.repo.path().join(".agentic/approval.key"), KEY).unwrap();
    run_rollback(&f, &["--approval-token", "1:deadbeef"]);
    assert_eq!(
        *f.seen.lock().unwrap(),
        vec![Some("1:deadbeef".to_string())]
    );
}
