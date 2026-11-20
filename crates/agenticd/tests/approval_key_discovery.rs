//! ADR-0019 Decision 3: without `--approval-key-file`, the daemon uses
//! `<repo>/.agentic/approval.key` when that file exists, with the same mode
//! check as an explicit key. Runs the real daemon binary and talks to it over
//! its socket.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agentic_proto::framing::{read_frame, write_frame};
use agentic_proto::{CommitInput, Envelope, Request, Response};
use tokio::net::UnixStream;

fn uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Create the state dir the way an operator would: the daemon refuses a
/// group-writable repo or state dir, and `create_dir_all` follows the umask
/// (0775 under umask 002), so pin both to 0700.
fn state_dir(repo: &std::path::Path) -> std::path::PathBuf {
    let state = repo.join(".agentic");
    std::fs::create_dir_all(&state).unwrap();
    for dir in [repo, state.as_path()] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    state
}

fn write_key(repo: &std::path::Path, mode: u32) {
    let key = state_dir(repo).join("approval.key");
    std::fs::write(&key, [5u8; 32]).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn mkfifo(path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path for the call's duration.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

fn spawn_daemon(repo: &std::path::Path, sock: &std::path::Path) -> Child {
    spawn_daemon_with(repo, sock, &[])
}

fn spawn_daemon_with(
    repo: &std::path::Path,
    sock: &std::path::Path,
    extra: &[&std::ffi::OsStr],
) -> Child {
    Command::new(env!("CARGO_BIN_EXE_agenticd"))
        .args(extra)
        .arg("--repo")
        .arg(repo)
        .arg("--socket")
        .arg(sock)
        .arg("--allowed-uid")
        .arg(uid().to_string())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agenticd")
}

async fn send(sock: &std::path::Path, request: Request) -> Response {
    let conn = UnixStream::connect(sock).await.unwrap();
    let (r, w) = conn.into_split();
    let mut r = tokio::io::BufReader::new(r);
    let mut w = tokio::io::BufWriter::new(w);
    write_frame(&mut w, &Envelope::new("t", request))
        .await
        .unwrap();
    let reply: Envelope<Response> = read_frame(&mut r).await.unwrap().expect("reply");
    reply.payload
}

/// Commit once, then send `Rollback { accept_data_loss: true }` with no
/// token. Which error comes back says whether the daemon holds a key.
async fn rollback_error_code(repo: &std::path::Path) -> String {
    rollback_error_code_with(repo, &[]).await
}

async fn rollback_error_code_with(repo: &std::path::Path, extra: &[&std::ffi::OsStr]) -> String {
    let sock_dir = tempfile::Builder::new()
        .prefix("agd")
        .tempdir_in("/tmp")
        .unwrap();
    let sock = sock_dir.path().join("d.sock");
    let mut child = spawn_daemon_with(repo, &sock, extra);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !sock.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !sock.exists() {
        // A daemon blocked on a FIFO never binds; do not leave it behind.
        let _ = child.kill();
        let _ = child.wait();
        panic!("daemon did not bind its socket");
    }

    let Response::Commit(c) = send(
        &sock,
        Request::Commit(CommitInput {
            message: "c".into(),
            author: None,
            code_sha: None,
            branch: None,
            prompts: Default::default(),
            mcp_servers: Vec::new(),
            model: None,
            no_memory: true,
        }),
    )
    .await
    else {
        panic!("commit failed");
    };
    let reply = send(
        &sock,
        Request::Rollback {
            target: c.commit_hash,
            dry_run: true,
            accept_data_loss: true,
            approval_token: None,
        },
    )
    .await;
    let _ = child.kill();
    let _ = child.wait();
    match reply {
        Response::Error { code, .. } => code,
        other => panic!("expected an approval error, got {other:?}"),
    }
}

#[tokio::test]
async fn default_key_file_is_picked_up() {
    let repo = tempfile::tempdir().unwrap();
    write_key(repo.path(), 0o600);
    assert_eq!(rollback_error_code(repo.path()).await, "approval_required");
}

#[tokio::test]
async fn no_key_file_means_no_key() {
    let repo = tempfile::tempdir().unwrap();
    state_dir(repo.path());
    assert_eq!(
        rollback_error_code(repo.path()).await,
        "approval_key_not_configured"
    );
}

#[test]
fn group_or_other_readable_default_key_aborts_startup() {
    let repo = tempfile::tempdir().unwrap();
    write_key(repo.path(), 0o644);
    let sock_dir = tempfile::Builder::new()
        .prefix("agd")
        .tempdir_in("/tmp")
        .unwrap();
    let mut child = spawn_daemon(repo.path(), &sock_dir.path().join("d.sock"));
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break Some(st);
        }
        if Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if status.is_none() {
        let _ = child.kill();
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        status.is_some_and(|s| !s.success()),
        "daemon should refuse to start with a 0644 default key"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("chmod 600"), "stderr: {stderr}");
}

#[tokio::test]
async fn a_fifo_at_the_default_key_path_does_not_block_startup() {
    let repo = tempfile::tempdir().unwrap();
    mkfifo(&state_dir(repo.path()).join("approval.key"));
    assert_eq!(
        rollback_error_code(repo.path()).await,
        "approval_key_not_configured"
    );
}

#[tokio::test]
async fn a_fifo_named_with_the_flag_does_not_block_startup_either() {
    let repo = tempfile::tempdir().unwrap();
    let fifo = state_dir(repo.path()).join("k.fifo");
    mkfifo(&fifo);
    let flag = std::ffi::OsString::from("--approval-key-file");
    assert_eq!(
        rollback_error_code_with(repo.path(), &[flag.as_os_str(), fifo.as_os_str()]).await,
        "approval_key_not_configured"
    );
}

#[tokio::test]
async fn a_symlink_to_a_valid_key_is_not_followed_default_or_explicit() {
    // Default path: treated as no key.
    let repo = tempfile::tempdir().unwrap();
    let state = state_dir(repo.path());
    let real = state.join("real.key");
    std::fs::write(&real, [5u8; 32]).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&real, state.join("approval.key")).unwrap();
    assert_eq!(
        rollback_error_code(repo.path()).await,
        "approval_key_not_configured"
    );

    // Explicit flag: O_NOFOLLOW makes it unreadable, so fail closed.
    let repo = tempfile::tempdir().unwrap();
    let state = state_dir(repo.path());
    let real = state.join("real.key");
    std::fs::write(&real, [5u8; 32]).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = state.join("link.key");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let flag = std::ffi::OsString::from("--approval-key-file");
    assert_eq!(
        rollback_error_code_with(repo.path(), &[flag.as_os_str(), link.as_os_str()]).await,
        "approval_key_not_configured"
    );
}
