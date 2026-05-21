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
