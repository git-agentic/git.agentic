//! Cross-platform startup-validation tests for ADR-0012 peer-auth.
//!
//! These tests exercise the daemon's CLI-flag validation, which runs
//! BEFORE any I/O (socket binding, object-store opening, etc.) and is
//! therefore platform-agnostic. Split out from
//! `peer_auth_integration.rs` because that file is gated on
//! `#[cfg(target_os = "linux")]` for SO_PEERCRED-based cred tests.

use std::process::Command;

fn agenticd_bin() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_agenticd").into()
}

#[test]
fn startup_refuses_without_policy() {
    // Use a unique socket path under /tmp so even if the daemon got far
    // enough to try to bind (it shouldn't — validation runs before any
    // I/O), parallel test runs wouldn't collide.
    let unique = std::process::id();
    let socket = std::env::temp_dir().join(format!("agenticd-startup-no-policy-{unique}.sock"));
    let out = Command::new(agenticd_bin())
        .arg("--repo")
        .arg(".")
        .arg("--socket")
        .arg(&socket)
        .output()
        .expect("run agenticd");
    assert!(
        !out.status.success(),
        "expected non-zero exit when no policy is configured"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refuses to start"),
        "stderr should mention refusal; got: {stderr}"
    );
}

#[test]
fn startup_refuses_when_both_flags_set() {
    let unique = std::process::id();
    let socket = std::env::temp_dir().join(format!("agenticd-startup-both-flags-{unique}.sock"));
    let out = Command::new(agenticd_bin())
        .arg("--repo")
        .arg(".")
        .arg("--socket")
        .arg(&socket)
        .arg("--allowed-uid")
        .arg("1000")
        .arg("--insecure-allow-any-uid")
        .output()
        .expect("run agenticd");
    assert!(
        !out.status.success(),
        "expected non-zero exit when both flags are set"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("mutually exclusive"),
        "stderr should mention mutual exclusivity; got: {stderr}"
    );
}

/// A repo dir with mode pinned to 0700. `tempfile::tempdir()` follows the
/// umask (0775 under umask 002), which the daemon refuses as group-writable.
#[cfg(unix)]
fn private_repo() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let repo = tempfile::tempdir().unwrap();
    std::fs::set_permissions(repo.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    repo
}

#[cfg(unix)]
mod state_dir {
    use super::{agenticd_bin, private_repo};
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn set_mode(p: &std::path::Path, mode: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Run the daemon expecting it to refuse to start; returns stderr. A
    /// daemon that starts instead is killed and fails the test, so a
    /// regression is a failure and not a hang.
    fn expect_refusal(repo: &std::path::Path) -> String {
        use std::process::Stdio;
        let sock = tempfile::tempdir().unwrap();
        let mut child = Command::new(agenticd_bin())
            .arg("--repo")
            .arg(repo)
            .arg("--socket")
            .arg(sock.path().join("d.sock"))
            .arg("--insecure-allow-any-uid")
            .stderr(Stdio::piped())
            .spawn()
            .expect("run agenticd");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let status = loop {
            if let Some(st) = child.try_wait().unwrap() {
                break Some(st);
            }
            if std::time::Instant::now() > deadline {
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        if status.is_none() {
            let _ = child.kill();
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            status.is_some_and(|st| !st.success()),
            "daemon should have refused to start"
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    #[test]
    fn refuses_group_writable_repo() {
        let repo = tempfile::tempdir().unwrap();
        set_mode(repo.path(), 0o777);
        let stderr = expect_refusal(repo.path());
        let canonical = std::fs::canonicalize(repo.path()).unwrap();
        assert!(
            stderr.contains("writable") && stderr.contains(&*canonical.to_string_lossy()),
            "stderr should name the repo path; got: {stderr}"
        );
        assert!(
            !repo.path().join(".agentic").exists(),
            "no state may be created under a refused repo"
        );
    }

    #[test]
    fn refuses_repo_owned_by_another_uid() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root owns everything; the check cannot fire
        }
        let sock = tempfile::tempdir().unwrap();
        let out = Command::new(agenticd_bin())
            .arg("--repo")
            .arg("/usr")
            .arg("--socket")
            .arg(sock.path().join("d.sock"))
            .arg("--insecure-allow-any-uid")
            .output()
            .expect("run agenticd");
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("/usr") && stderr.contains("owned by uid"),
            "got: {stderr}"
        );
    }

    /// Spawn the daemon (umask pinned to 022), wait for the socket, return
    /// the socket and state-dir modes, then kill it.
    fn run_and_stat(repo: &std::path::Path, extra: &[&str]) -> (u32, u32) {
        use std::os::unix::process::CommandExt;
        let sock_dir = tempfile::tempdir().unwrap();
        let sock = sock_dir.path().join("d.sock");
        let mut cmd = Command::new(agenticd_bin());
        cmd.arg("--repo")
            .arg(repo)
            .arg("--socket")
            .arg(&sock)
            .args(extra);
        // SAFETY: umask is async-signal-safe and runs in the child only.
        unsafe {
            cmd.pre_exec(|| {
                libc::umask(0o022);
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("run agenticd");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !sock.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let bound = sock.exists();
        let sock_mode =
            bound.then(|| std::fs::metadata(&sock).unwrap().permissions().mode() & 0o7777);
        let state_mode = std::fs::metadata(repo.join(".agentic"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        let _ = child.kill();
        let _ = child.wait();
        (
            sock_mode.expect("daemon did not bind the socket"),
            state_mode,
        )
    }

    #[test]
    fn startup_tightens_0755_state_dir_to_0700() {
        let repo = private_repo();
        let state = repo.path().join(".agentic");
        std::fs::create_dir(&state).unwrap();
        set_mode(&state, 0o755);
        let (_, state_mode) = run_and_stat(repo.path(), &["--insecure-allow-any-uid"]);
        assert_eq!(state_mode, 0o700);
    }

    #[test]
    fn refuses_existing_writable_state_dir() {
        for m in [0o775, 0o777] {
            let repo = private_repo();
            let state = repo.path().join(".agentic");
            std::fs::create_dir(&state).unwrap();
            set_mode(&state, m);
            let stderr = expect_refusal(repo.path());
            assert!(
                stderr.contains(".agentic") && stderr.contains("writable"),
                "{m:o}: {stderr}"
            );
        }
    }

    #[test]
    fn long_canonical_repo_path_gets_socket_flag_hint() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("a".repeat(120));
        let out = Command::new(agenticd_bin())
            .arg("--repo")
            .arg(&repo)
            .arg("--insecure-allow-any-uid")
            .output()
            .expect("run agenticd");
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("--socket <shorter path>"), "{stderr}");
    }

    #[test]
    fn startup_removes_dangling_symlink_at_socket_path() {
        let repo = private_repo();
        let sock_dir = private_repo();
        let sock = sock_dir.path().join("d.sock");
        let target = sock_dir.path().join("does-not-exist");
        std::os::unix::fs::symlink(&target, &sock).unwrap();
        let mut child = Command::new(agenticd_bin())
            .arg("--repo")
            .arg(repo.path())
            .arg("--socket")
            .arg(&sock)
            .arg("--insecure-allow-any-uid")
            .spawn()
            .expect("run agenticd");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let is_socket = |p: &std::path::Path| {
            use std::os::unix::fs::FileTypeExt;
            std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_socket())
        };
        while !is_socket(&sock) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let bound = is_socket(&sock);
        let target_created = target.exists();
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            bound,
            "daemon should replace the dangling symlink with its socket"
        );
        assert!(!target_created, "the link target must never be created");
    }

    #[test]
    fn socket_is_0666_only_when_allowlist_names_another_uid() {
        // SAFETY: geteuid cannot fail.
        let euid = unsafe { libc::geteuid() };
        let other = (euid + 1).to_string();
        let own = euid.to_string();

        let repo = private_repo();
        let (sock_mode, _) = run_and_stat(repo.path(), &["--allowed-uid", &other]);
        assert_eq!(
            sock_mode, 0o666,
            "worker uid listed: socket must be connectable"
        );

        let repo = private_repo();
        let (sock_mode, _) = run_and_stat(repo.path(), &["--allowed-uid", &own]);
        assert_eq!(sock_mode, 0o755, "own uid only: default umask applies");

        let repo = private_repo();
        let (sock_mode, _) = run_and_stat(repo.path(), &["--insecure-allow-any-uid"]);
        assert_eq!(sock_mode, 0o755);
    }
}

#[cfg(unix)]
#[test]
fn startup_requires_socket_flag_when_allowlist_names_another_uid() {
    // SAFETY: geteuid cannot fail.
    let other = (unsafe { libc::geteuid() } + 1).to_string();
    let repo = private_repo();
    let out = Command::new(agenticd_bin())
        .arg("--repo")
        .arg(repo.path())
        .arg("--allowed-uid")
        .arg(&other)
        .output()
        .expect("run agenticd");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("pass --socket"), "got: {stderr}");
}

#[test]
fn startup_refuses_postgres_url_and_file_together() {
    let out = Command::new(agenticd_bin())
        .arg("--insecure-allow-any-uid")
        .arg("--postgres")
        .arg("postgres://x")
        .arg("--postgres-url-file")
        .arg("/nonexistent")
        .output()
        .expect("run agenticd");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot be used with"), "got: {stderr}");
}
