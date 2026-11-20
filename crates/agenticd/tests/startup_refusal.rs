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

#[cfg(unix)]
mod state_dir {
    use super::agenticd_bin;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn set_mode(p: &std::path::Path, mode: u32) {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn refuses_group_writable_repo() {
        let repo = tempfile::tempdir().unwrap();
        let sock = tempfile::tempdir().unwrap();
        set_mode(repo.path(), 0o777);
        let out = Command::new(agenticd_bin())
            .arg("--repo")
            .arg(repo.path())
            .arg("--socket")
            .arg(sock.path().join("d.sock"))
            .arg("--insecure-allow-any-uid")
            .output()
            .expect("run agenticd");
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("writable") && stderr.contains(&*repo.path().to_string_lossy()),
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

    #[test]
    fn startup_leaves_state_dir_0700() {
        let repo = tempfile::tempdir().unwrap();
        let sock_dir = tempfile::tempdir().unwrap();
        let sock = sock_dir.path().join("d.sock");
        let state = repo.path().join(".agentic");
        std::fs::create_dir(&state).unwrap();
        set_mode(&state, 0o777);
        let mut child = Command::new(agenticd_bin())
            .arg("--repo")
            .arg(repo.path())
            .arg("--socket")
            .arg(&sock)
            .arg("--insecure-allow-any-uid")
            .spawn()
            .expect("run agenticd");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !sock.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let bound = sock.exists();
        let mode = std::fs::metadata(&state).unwrap().permissions().mode() & 0o7777;
        let _ = child.kill();
        let _ = child.wait();
        assert!(bound, "daemon did not bind the socket");
        assert_eq!(mode, 0o700);
    }
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
