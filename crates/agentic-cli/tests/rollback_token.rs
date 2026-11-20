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

/// One `Rollback` request as the fake daemon saw it.
#[derive(Debug, PartialEq)]
struct Seen1 {
    target: String,
    dry_run: bool,
    token: Option<String>,
}

type Seen = Arc<Mutex<Vec<Seen1>>>;

/// Serve forever on `sock` in a background thread, answering `ResolveRef`
/// with `resolved`; returns every `Rollback` request the CLI sent.
fn fake_daemon(sock: &Path, resolved: String) -> Seen {
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
                        hash: resolved.clone(),
                    },
                    Request::Rollback {
                        target,
                        dry_run,
                        approval_token,
                        ..
                    } => {
                        recorded.lock().unwrap().push(Seen1 {
                            target,
                            dry_run,
                            token: approval_token,
                        });
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
    fixture_resolving(target_hash())
}

fn fixture_resolving(resolved: String) -> Fixture {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".agentic")).unwrap();
    let sock_dir = tempfile::Builder::new()
        .prefix("agc")
        .tempdir_in("/tmp")
        .unwrap();
    let seen = fake_daemon(&sock_dir.path().join("d.sock"), resolved);
    Fixture {
        repo,
        sock_dir,
        seen,
    }
}

/// Run `agentic rollback main` with `args`, answering the confirmation
/// prompt (when there is one) with `stdin`.
fn agentic(f: &Fixture, args: &[&str], stdin: &str) -> std::process::Output {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentic"))
        .arg("--repo")
        .arg(f.repo.path())
        .args(args)
        .env("AGENTIC_SOCKET", f.sock_dir.path().join("d.sock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn rollback_cmd(f: &Fixture, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["rollback", "main", "--yes"];
    args.extend_from_slice(extra);
    agentic(f, &args, "")
}

fn run_rollback(f: &Fixture, extra: &[&str]) {
    let out = rollback_cmd(f, extra);
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
    let token = seen[0].token.as_deref().expect("token must be attached");
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
    assert!(f.seen.lock().unwrap()[0].token.is_some());
}

#[test]
fn sends_no_token_without_a_key() {
    let f = fixture();
    run_rollback(&f, &[]);
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].token, None);
    assert_eq!(
        seen[0].target, "main",
        "no key: the target is sent as given"
    );
}

#[test]
fn explicit_token_is_not_replaced() {
    let f = fixture();
    std::fs::write(f.repo.path().join(".agentic/approval.key"), KEY).unwrap();
    run_rollback(&f, &["--approval-token", "1:deadbeef"]);
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].token.as_deref(), Some("1:deadbeef"));
    assert_eq!(
        seen[0].target, "main",
        "a user token leaves the target alone"
    );
}

#[test]
fn unusable_default_key_sends_no_token_but_a_bad_explicit_key_fails() {
    let f = fixture();
    // Wrong length: the daemon would run keyless, so the CLI must not fail.
    std::fs::write(f.repo.path().join(".agentic/approval.key"), [1u8; 5]).unwrap();
    run_rollback(&f, &[]);
    assert_eq!(f.seen.lock().unwrap()[0].token, None);

    let bad = f.repo.path().join(".agentic/approval.key");
    let out = rollback_cmd(&f, &["--key-file", bad.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "an explicit bad key must be an error"
    );
    assert_eq!(f.seen.lock().unwrap().len(), 1, "no second request sent");
}

#[test]
fn preview_and_applied_rollback_name_the_pinned_hash_and_only_the_applied_one_is_signed() {
    let f = fixture();
    std::fs::write(f.repo.path().join(".agentic/approval.key"), KEY).unwrap();
    // No --yes: preview, then confirm.
    let out = agentic(&f, &["rollback", "main"], "y\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "preview then apply: {seen:?}");
    let (preview, applied) = (&seen[0], &seen[1]);
    assert!(preview.dry_run && !applied.dry_run);
    assert_eq!(preview.target, target_hash(), "preview must be pinned");
    assert_eq!(applied.target, target_hash(), "apply must be pinned");
    assert_eq!(preview.token, None, "a preview carries no auto token");
    let key = ApprovalKey::from_bytes(&KEY).unwrap();
    verify_token(
        &key,
        &applied.target,
        euid(),
        applied.token.as_deref().expect("applied request is signed"),
        now(),
    )
    .unwrap();
}

#[test]
fn destructive_preview_gets_its_own_token_and_the_apply_is_minted_fresh() {
    let f = fixture();
    std::fs::write(f.repo.path().join(".agentic/approval.key"), KEY).unwrap();
    let out = agentic(&f, &["rollback", "main", "--accept-data-loss"], "y\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = f.seen.lock().unwrap();
    assert!(seen[0].dry_run && seen[0].token.is_some());
    assert!(!seen[1].dry_run && seen[1].token.is_some());
}

#[test]
fn declining_the_prompt_sends_no_signed_apply() {
    let f = fixture();
    std::fs::write(f.repo.path().join(".agentic/approval.key"), KEY).unwrap();
    let out = agentic(&f, &["rollback", "main"], "n\n");
    assert!(!out.status.success());
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].dry_run && seen[0].token.is_none());
}

#[test]
fn dry_run_flag_carries_no_auto_token() {
    let f = fixture();
    std::fs::write(f.repo.path().join(".agentic/approval.key"), KEY).unwrap();
    run_rollback(&f, &["--dry-run"]);
    let seen = f.seen.lock().unwrap();
    assert!(seen[0].dry_run);
    assert_eq!(seen[0].token, None);
}

#[test]
fn refuses_to_sign_a_resolved_hash_that_is_not_64_lowercase_hex() {
    for bad in [
        "AB".repeat(32),
        "ab".repeat(31),
        format!("{}:x", "ab".repeat(31)),
        "ab".repeat(32) + "\n",
    ] {
        let f = fixture_resolving(bad.clone());
        std::fs::write(f.repo.path().join(".agentic/approval.key"), KEY).unwrap();
        let out = rollback_cmd(&f, &[]);
        assert!(!out.status.success(), "{bad:?} must be refused");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("refusing to sign"),
            "{bad:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(f.seen.lock().unwrap().is_empty(), "nothing is sent");
    }
}

#[test]
fn key_warning_goes_to_stderr_and_json_stdout_stays_parseable() {
    let f = fixture();
    std::fs::write(f.repo.path().join(".agentic/approval.key"), [1u8; 5]).unwrap();
    let out = agentic(&f, &["--json", "rollback", "main", "--yes"], "");
    assert!(out.status.success());
    serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    assert!(String::from_utf8_lossy(&out.stderr).contains("warning"));
}
