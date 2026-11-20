//! ADR-0019 Decision 4: `agentic rollback` attaches an approval token on its
//! own when it can read the approval key. The CLI binary runs against a fake
//! daemon that answers `ResolveRef` and records every `Rollback` request.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use agentic_core::approval::{verify_token, verify_token_any, ApprovalKey, ApprovalScope};
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
/// with `resolved` and reporting `approval_required = needs_approval`; returns every `Rollback` request the CLI sent.
fn fake_daemon(sock: &Path, resolved: String, needs_approval: bool) -> Seen {
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
                            executed: !dry_run,
                            new_head_hash: (!dry_run).then(target_hash),
                            approval_required: needs_approval,
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
    fixture_with(resolved, true)
}

/// A daemon whose previews report that the plan needs no approval.
fn fixture_no_approval() -> Fixture {
    fixture_with(target_hash(), false)
}

fn fixture_with(resolved: String, needs_approval: bool) -> Fixture {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir(repo.path().join(".agentic")).unwrap();
    let sock_dir = tempfile::Builder::new()
        .prefix("agc")
        .tempdir_in("/tmp")
        .unwrap();
    let seen = fake_daemon(&sock_dir.path().join("d.sock"), resolved, needs_approval);
    Fixture {
        repo,
        sock_dir,
        seen,
    }
}

/// Run `agentic` with `args`, answering the confirmation prompt (when there
/// is one) with `stdin`. A run that does not finish in 20s is killed and
/// fails the test, so a regression that blocks is a failure, not a hang.
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("agentic {args:?} did not finish in 20s");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
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

/// Write a key file with the mode the CLI requires (0600).
fn write_key(path: impl AsRef<Path>, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path.as_ref(), bytes).unwrap();
    std::fs::set_permissions(path.as_ref(), std::fs::Permissions::from_mode(0o600)).unwrap();
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
    write_key(f.repo.path().join(".agentic/approval.key"), &KEY);
    run_rollback(&f, &[]);

    let seen = f.seen.lock().unwrap();
    // --yes with a key: a silent preview, then the applied request.
    assert_eq!(seen.len(), 2);
    assert!(seen[0].dry_run && seen[0].token.is_none());
    let token = seen[1].token.as_deref().expect("token must be attached");
    let key = ApprovalKey::from_bytes(&KEY).unwrap();
    verify_token_any(
        &key,
        &[ApprovalScope::ReverseMigration],
        &target_hash(),
        euid(),
        token,
        now(),
    )
    .expect("token must verify for the resolved commit and the caller's uid");
    // A plain rollback's token is reverse-migration scope: it cannot
    // authorize accept_data_loss.
    assert!(verify_token(&key, &target_hash(), euid(), token, now()).is_err());
}

#[test]
fn attaches_a_token_from_the_key_file_flag() {
    let f = fixture();
    let key_path = f.repo.path().join("custom.key");
    write_key(&key_path, &KEY);
    run_rollback(&f, &["--key-file", key_path.to_str().unwrap()]);
    assert!(f.seen.lock().unwrap().last().unwrap().token.is_some());
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
    write_key(f.repo.path().join(".agentic/approval.key"), &KEY);
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
    write_key(f.repo.path().join(".agentic/approval.key"), &[1u8; 5]);
    run_rollback(&f, &[]);
    assert_eq!(f.seen.lock().unwrap()[0].token, None);
    f.seen.lock().unwrap().clear();

    let bad = f.repo.path().join(".agentic/approval.key");
    let out = rollback_cmd(&f, &["--key-file", bad.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "an explicit bad key must be an error"
    );
    assert!(f.seen.lock().unwrap().is_empty(), "nothing is sent");
}

#[test]
fn preview_and_applied_rollback_name_the_pinned_hash_and_only_the_applied_one_is_signed() {
    let f = fixture();
    write_key(f.repo.path().join(".agentic/approval.key"), &KEY);
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
    verify_token_any(
        &key,
        &[ApprovalScope::ReverseMigration],
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
    write_key(f.repo.path().join(".agentic/approval.key"), &KEY);
    let out = agentic(&f, &["rollback", "main", "--accept-data-loss"], "y\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = f.seen.lock().unwrap();
    assert!(seen[0].dry_run && seen[0].token.is_some());
    assert!(!seen[1].dry_run && seen[1].token.is_some());
    // --accept-data-loss tokens, preview and apply, are data-loss scope.
    let key = ApprovalKey::from_bytes(&KEY).unwrap();
    for r in seen.iter() {
        verify_token(&key, &r.target, euid(), r.token.as_deref().unwrap(), now())
            .expect("a destructive request carries a data-loss-scope token");
    }
}

#[test]
fn declining_the_prompt_sends_no_signed_apply() {
    let f = fixture();
    write_key(f.repo.path().join(".agentic/approval.key"), &KEY);
    let out = agentic(&f, &["rollback", "main"], "n\n");
    assert!(!out.status.success());
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].dry_run && seen[0].token.is_none());
}

#[test]
fn dry_run_flag_carries_no_auto_token() {
    let f = fixture();
    write_key(f.repo.path().join(".agentic/approval.key"), &KEY);
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
        write_key(f.repo.path().join(".agentic/approval.key"), &KEY);
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
    write_key(f.repo.path().join(".agentic/approval.key"), &[1u8; 5]);
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

#[test]
fn apply_is_not_signed_when_the_preview_reported_no_approval_needed() {
    // A forward migration could land between the preview and the apply; a
    // token minted for a plan that needed none would then authorize a
    // reverse migration nobody saw. The daemon's gate refuses it instead.
    for extra in [&[][..], &["--yes"][..]] {
        let f = fixture_no_approval();
        write_key(f.repo.path().join(".agentic/approval.key"), &KEY);
        let mut args = vec!["rollback", "main"];
        args.extend_from_slice(extra);
        let out = agentic(&f, &args, "y\n");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{extra:?}: {seen:?}");
        assert!(seen[0].dry_run && !seen[1].dry_run);
        assert_eq!(seen[1].token, None, "{extra:?}: apply must not be signed");
    }
}

#[test]
fn oversized_default_key_file_is_rejected_without_a_token() {
    let f = fixture();
    // Far longer than a key; only the first 33 bytes may be read.
    write_key(
        f.repo.path().join(".agentic/approval.key"),
        &vec![7u8; 1 << 20],
    );
    run_rollback(&f, &[]);
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "no key, so no pinning preview: {seen:?}");
    assert_eq!(seen[0].token, None);
}

#[test]
fn accept_data_loss_apply_is_signed_even_if_the_preview_omits_approval_required() {
    // An older daemon's reply has no approval_required (it reads as false).
    let f = fixture_no_approval();
    write_key(f.repo.path().join(".agentic/approval.key"), &KEY);
    let out = agentic(&f, &["rollback", "main", "--accept-data-loss"], "y\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(!seen[1].dry_run && seen[1].token.is_some(), "{seen:?}");
}

fn mkfifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path for the call's duration.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

#[test]
fn a_fifo_at_the_default_key_path_is_skipped_not_blocked_on() {
    let f = fixture();
    mkfifo(&f.repo.path().join(".agentic/approval.key"));
    let out = rollback_cmd(&f, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a regular file"));
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "no key, so no silent preview: {seen:?}");
    assert_eq!(seen[0].token, None);
}

#[test]
fn a_symlinked_default_key_is_skipped() {
    let f = fixture();
    let real = f.repo.path().join("real.key");
    write_key(&real, &KEY);
    std::os::unix::fs::symlink(&real, f.repo.path().join(".agentic/approval.key")).unwrap();
    run_rollback(&f, &[]);
    assert_eq!(f.seen.lock().unwrap()[0].token, None);
}

#[test]
fn an_explicit_key_that_is_not_a_regular_file_is_an_error() {
    let f = fixture();
    let fifo = f.repo.path().join("k.fifo");
    mkfifo(&fifo);
    let out = rollback_cmd(&f, &["--key-file", fifo.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a regular file"));
    assert!(f.seen.lock().unwrap().is_empty());
}

/// `agentic rollback --approval <hash> --uid 1000 --key-file <key> [extra]`
/// prints a token; returns it.
fn issue(extra: &[&str]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("k");
    write_key(&key, &KEY);
    let out = Command::new(env!("CARGO_BIN_EXE_agentic"))
        .arg("--repo")
        .arg(dir.path())
        .args(["rollback", "--approval", &target_hash(), "--uid", "1000"])
        .arg("--key-file")
        .arg(&key)
        .args(extra)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn issuer_scope_flag_selects_the_token_scope_and_defaults_to_data_loss() {
    let key = ApprovalKey::from_bytes(&KEY).unwrap();
    let h = target_hash();
    let rm = issue(&["--scope", "reverse-migration"]);
    // Verifies for a plain reverse-migration rollback, not for accept_data_loss.
    verify_token_any(
        &key,
        &[ApprovalScope::ReverseMigration],
        &h,
        1000,
        &rm,
        now(),
    )
    .unwrap();
    assert!(verify_token(&key, &h, 1000, &rm, now()).is_err());
    // No flag, or data-loss: the data-loss scope, as before.
    for dl in [issue(&[]), issue(&["--scope", "data-loss"])] {
        verify_token(&key, &h, 1000, &dl, now()).unwrap();
        assert!(verify_token_any(
            &key,
            &[ApprovalScope::ReverseMigration],
            &h,
            1000,
            &dl,
            now()
        )
        .is_err());
    }
}

#[test]
fn a_symlink_at_the_key_path_is_never_followed() {
    // O_NOFOLLOW: a symlink to a valid key is refused even when the path is
    // named explicitly, so a swap after any earlier check cannot be followed.
    let f = fixture();
    let real = f.repo.path().join("real.key");
    write_key(&real, &KEY);
    let link = f.repo.path().join("link.key");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let out = rollback_cmd(&f, &["--key-file", link.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "an explicit symlinked key is an error"
    );
    assert!(f.seen.lock().unwrap().is_empty());
}

#[test]
fn a_group_readable_key_is_skipped_by_default_and_refused_when_named() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    let default = f.repo.path().join(".agentic/approval.key");
    write_key(&default, &KEY);
    std::fs::set_permissions(&default, std::fs::Permissions::from_mode(0o644)).unwrap();
    // Default path: warning on stderr, no token, the rollback still runs.
    let out = rollback_cmd(&f, &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("chmod 600"));
    assert_eq!(f.seen.lock().unwrap()[0].token, None);
    f.seen.lock().unwrap().clear();
    // Named with --key-file: an error, nothing sent.
    let out = rollback_cmd(&f, &["--key-file", default.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("chmod 600"));
    assert!(f.seen.lock().unwrap().is_empty());
}
