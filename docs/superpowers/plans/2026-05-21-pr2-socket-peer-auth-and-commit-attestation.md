# PR-2 — Socket peer authentication + Commit attestation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement [ADR-0012](../../adr/0012-socket-peer-authentication.md). Closes TM-001 (worker enumerates objects via `ReadObject`) and TM-003 (worker forges Commits) from `git.agentic-threat-model.md`. After this PR, every connection to `agenticd`'s Unix socket has its UID checked against a startup-configured allowlist, and every Commit blob carries the `peer_uid` of the connection that shaped it.

**Architecture:** Two intertwined changes. (1) The accept loop in `agenticd/src/main.rs` reads peer credentials via `tokio::net::UnixStream::peer_cred()` (returns `tokio::net::unix::UCred`, which exposes `uid()` and `pid()` portably across Linux/macOS) and rejects connections whose UID is not in the allowlist before `handle_connection` runs. (2) The Commit object in `agentic-core` gains an additive `peer_uid: Option<u32>` field, threaded from the connection through `DaemonState` → `commit::execute` → `agentic-core::stage_and_commit_with_now`.

**Tech Stack:** Rust (`crates/agentic-core`, `crates/agentic-proto`, `crates/agenticd`); `tokio::net::unix::UCred` (no new dependency); `clap` for new CLI flags (already in workspace).

**Spec reference:** [`docs/superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md`](../specs/2026-05-21-pre-public-hardening-sprint-design.md) §"PR-2"; [ADR-0012](../../adr/0012-socket-peer-authentication.md).

---

## File Structure

**Modified:**
- `crates/agentic-core/src/object.rs` — `Commit` struct gains `peer_uid: Option<u32>` (additive, `#[serde(default)]`).
- `crates/agentic-core/src/commit.rs` — `CommitInputs` struct gains `peer_uid: Option<u32>`; `stage_and_commit_with_now` propagates it into the new `Commit` blob.
- `crates/agenticd/src/main.rs` — new CLI flags `--allowed-uid <UID>` (repeatable) and `--insecure-allow-any-uid`; startup refusal logic; accept-loop peer-cred check + rejection; structured `tracing` events.
- `crates/agenticd/src/server.rs` — `DaemonState` gains an `Arc<PeerAuthPolicy>` (the parsed allowlist + insecure-mode flag); `handle_connection` signature grows a `peer_uid: Option<u32>` parameter; dispatch carries it.
- `crates/agenticd/src/commit.rs` — `execute_with_now` reads `peer_uid` from the dispatch context and writes it into `CommitInputs`.

**New:** none. All changes are in-place modifications.

**Test files modified:** `crates/agentic-core/src/object.rs` (existing tests), `crates/agentic-core/src/commit.rs` (existing tests), `crates/agenticd/src/commit.rs` (existing test module), and `crates/agenticd/tests/peer_auth_integration.rs` (new — but it lives under `tests/`, conventional location for integration tests; counts as a modification of the test surface, not a new module under `src/`).

---

## Branch + Setup

### Task 0: Create the working branch

- [ ] **Step 1: Confirm clean main**

```bash
git checkout main && git pull --ff-only
git status --short | grep -v '^??'
```

Expected: no `M ` lines (only untracked files, which the ignore-additions PR has already silenced).

- [ ] **Step 2: Branch**

```bash
git checkout -b feat/pr2-socket-peer-auth-and-commit-attestation
```

Expected: `Switched to a new branch …`.

---

## Task 1 — Add `peer_uid` to `agentic-core::Commit` and `CommitInputs`

**Files:**
- Modify: `crates/agentic-core/src/object.rs` (around line 90, the `pub struct Commit` definition)
- Modify: `crates/agentic-core/src/commit.rs` (the `pub struct CommitInputs` near line 39; the `build_commit_from_inputs`/`stage_and_commit_with_now` body that constructs the `Commit`)
- Test: `crates/agentic-core/src/commit.rs` (existing test module, extend)

### Task 1.1: Write the failing test

- [ ] **Step 1: Add a serde-roundtrip test in `crates/agentic-core/src/object.rs`**

Find the existing `#[cfg(test)] mod tests` block in `object.rs` (search for `mod tests`). Add this test:

```rust
#[test]
fn commit_peer_uid_serde_roundtrip() {
    use serde_json::{from_str, to_string};

    // A Commit with peer_uid set: hash must change vs the same Commit
    // without it, and the field must round-trip.
    let mut c = Commit {
        parent: None,
        author: "alice@example.com".into(),
        timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap(),
        message: "test".into(),
        code_sha: None,
        prompts: None,
        tools: None,
        model: None,
        memory_snapshot: None,
        schema_version: None,
        intent: None,
        plan: None,
        transcript: None,
        evals: None,
        cost_cents: 0,
        peer_uid: Some(1000),
    };
    let json = to_string(&c).unwrap();
    let back: Commit = from_str(&json).unwrap();
    assert_eq!(back.peer_uid, Some(1000));

    // Backwards compat: a Commit JSON missing the peer_uid field
    // deserializes with peer_uid: None (#[serde(default)]).
    let legacy_json = json.replace(r#","peer_uid":1000"#, "");
    assert!(!legacy_json.contains("peer_uid"));
    let legacy: Commit = from_str(&legacy_json).unwrap();
    assert_eq!(legacy.peer_uid, None);

    // peer_uid: None and peer_uid: Some(_) hash differently — the field
    // is part of the canonical bytes.
    c.peer_uid = None;
    let hash_none = Hash::of(&serde_json::to_vec(&c).unwrap());
    c.peer_uid = Some(1000);
    let hash_some = Hash::of(&serde_json::to_vec(&c).unwrap());
    assert_ne!(hash_none, hash_some);
}
```

If the existing `tests` module doesn't yet `use` items needed here (e.g., `Hash`), add the `use` lines at the top of the module. The test references `Hash::of(&bytes)` — confirm that's the public API by grepping `crates/agentic-core/src/hash.rs` for `pub fn of` before assuming.

- [ ] **Step 2: Run the test, expect it to fail**

```bash
cargo test -p agentic-core --lib commit_peer_uid_serde_roundtrip 2>&1 | tail -10
```

Expected: compile error — `Commit` has no field `peer_uid`. That's the red.

### Task 1.2: Add the field

- [ ] **Step 3: Extend `Commit` in `crates/agentic-core/src/object.rs`**

Locate the `pub struct Commit` (around line 90). After the existing platform-API extension fields (`intent`, `plan`, `transcript`, `evals`, `cost_cents`), add:

```rust
    // --- ADR-0012 attestation ---
    /// UID of the OS process that opened the socket connection which
    /// shaped this commit. `None` on commits shaped before ADR-0012's
    /// implementation landed (pre-attestation history). Additive
    /// `Option` per ADR-0002 D6.
    #[serde(default)]
    pub peer_uid: Option<u32>,
```

- [ ] **Step 4: Extend `CommitInputs` in `crates/agentic-core/src/commit.rs`**

Locate `pub struct CommitInputs` (around line 39). After all existing fields, add:

```rust
    /// UID of the daemon's socket peer; propagated into `Commit::peer_uid`.
    /// `None` when the daemon is running under `--insecure-allow-any-uid`
    /// or when commits originate from non-socket paths (e.g. unit tests).
    pub peer_uid: Option<u32>,
```

- [ ] **Step 5: Thread `peer_uid` into the `Commit` blob in `stage_and_commit_with_now`**

In `crates/agentic-core/src/commit.rs`, find the function `stage_and_commit_with_now` (or whichever helper actually builds the `Commit` struct from `CommitInputs`). The Commit-building site looks like `Commit { parent: ..., author: ..., ..., cost_cents: inputs.cost_cents }`. Add:

```rust
        peer_uid: inputs.peer_uid,
```

at the end of the struct-literal field list. The compiler will tell you exactly where if you miss the spot.

### Task 1.3: Fix all CommitInputs construction sites

The new mandatory `peer_uid` field breaks every existing `CommitInputs { ... }` construction. Find them all:

- [ ] **Step 6: Locate construction sites**

```bash
grep -rn 'CommitInputs {' crates/
```

Expected: at least one site in `crates/agenticd/src/commit.rs::assemble_inputs` and possibly test fixtures.

- [ ] **Step 7: Fix each site**

For each `CommitInputs { ... }` literal, add `peer_uid: None,` to the field list (we'll wire actual UIDs through `agenticd::commit` in Task 5). In test fixtures, `None` is the right default — tests are not running over a real socket.

- [ ] **Step 8: Run the test green**

```bash
cargo test -p agentic-core --lib commit_peer_uid_serde_roundtrip 2>&1 | tail -10
```

Expected: `test result: ok. 1 passed`.

- [ ] **Step 9: Run all of agentic-core's tests**

```bash
cargo test -p agentic-core --lib 2>&1 | tail -10
```

Expected: all pre-existing tests still pass. If a commit-hash assertion in an existing test breaks because `peer_uid: None` was added to the canonical bytes — that's fine; `peer_uid: None` serializes to no field (because `#[serde(default)]` + Option default behavior depends on serde config). If serde IS emitting `"peer_uid":null`, the hash changes; update any pinned-hash assertions in the test suite to the new value (this is the "regenerate pinned hashes" risk called out in ADR-0012 Consequences).

Run `cargo test -p agentic-core --lib 2>&1 | grep FAILED` to find any breakage and address with the new hash literal.

### Task 1.4: Commit Task 1

- [ ] **Step 10: Run the full workspace**

```bash
cargo check --workspace 2>&1 | tail -5
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: both green.

- [ ] **Step 11: Commit**

```bash
git add crates/agentic-core/src/object.rs crates/agentic-core/src/commit.rs crates/agenticd/src/commit.rs
# include any other CommitInputs construction sites you fixed
git commit -m "agentic-core: add Commit::peer_uid and CommitInputs::peer_uid (ADR-0012)

Implements the schema half of ADR-0012's Decision 2: every Commit object
gains an additive Option<u32> peer_uid field, threaded from CommitInputs
through stage_and_commit_with_now into the canonical Commit blob bytes.

Backwards compatible per ADR-0002 D6 — older readers ignore the field;
older writers produce commits with peer_uid: None, which validators must
accept as 'pre-attestation history.'

The wire-side flow (reading peer_cred at accept time, refusing
connections from non-allowlisted UIDs) lands in the agenticd-side commit
of this PR.

Refs ADR-0012 Decision 2; closes part of TM-001 / TM-003."
```

---

## Task 2 — Add `agenticd` CLI flags + startup refusal

**Files:**
- Modify: `crates/agenticd/src/main.rs` — extend `struct Args`; add validation + refusal logic in `main()` before the listener binds.

### Task 2.1: Add the CLI flags

- [ ] **Step 1: Extend `struct Args`**

In `crates/agenticd/src/main.rs`, find `struct Args` (around line 35). After the existing fields, add:

```rust
    /// UID allowed to connect to the socket. Repeatable. Required in
    /// production deployments; the daemon refuses to start without at
    /// least one --allowed-uid unless --insecure-allow-any-uid is
    /// explicitly passed. Per ADR-0012.
    #[arg(long = "allowed-uid")]
    allowed_uids: Vec<u32>,

    /// Disable peer-UID enforcement on the socket. Demo and macOS-
    /// native development only — production deployments MUST NOT use
    /// this flag. Logged loudly at startup.
    #[arg(long)]
    insecure_allow_any_uid: bool,
```

- [ ] **Step 2: Define the PeerAuthPolicy type**

At the top of `crates/agenticd/src/main.rs` (or in a small helper module), add:

```rust
/// What the accept loop does with peer UIDs on each connection.
/// Constructed once at startup from CLI flags; held in `DaemonState`.
#[derive(Clone, Debug)]
pub enum PeerAuthPolicy {
    /// Reject any connection whose UID is not in this set.
    Allowlist(std::collections::BTreeSet<u32>),
    /// Accept every connection. Set by --insecure-allow-any-uid.
    InsecureAllowAny,
}

impl PeerAuthPolicy {
    pub fn is_allowed(&self, uid: u32) -> bool {
        match self {
            PeerAuthPolicy::Allowlist(set) => set.contains(&uid),
            PeerAuthPolicy::InsecureAllowAny => true,
        }
    }
}
```

(If the type already exists from earlier exploration, skip; if it's better placed under a new `crates/agenticd/src/peer_auth.rs`, do that — but keep it tiny.)

### Task 2.2: Refuse-to-start logic in `main()`

- [ ] **Step 3: Add the startup refusal**

Immediately after `let args = Args::parse();`, before `agentic_dir` is computed, add:

```rust
let peer_auth = match (args.allowed_uids.is_empty(), args.insecure_allow_any_uid) {
    (true, false) => {
        return Err(anyhow::anyhow!(
            "agenticd refuses to start without peer-UID enforcement.\n\
             Pass --allowed-uid <UID> (repeatable) to enable the allowlist,\n\
             or pass --insecure-allow-any-uid explicitly to disable enforcement\n\
             (demo and macOS-native development only — never in production)."
        ));
    }
    (false, true) => {
        return Err(anyhow::anyhow!(
            "--allowed-uid and --insecure-allow-any-uid are mutually exclusive.\n\
             Pass one or the other, not both."
        ));
    }
    (true, true) => PeerAuthPolicy::InsecureAllowAny,
    (false, false) => PeerAuthPolicy::Allowlist(args.allowed_uids.iter().copied().collect()),
};

if matches!(peer_auth, PeerAuthPolicy::InsecureAllowAny) {
    tracing::warn!(
        target: "agenticd::accept",
        "running with --insecure-allow-any-uid; every socket connection is \
         accepted regardless of peer UID. Production deployments MUST set \
         --allowed-uid instead."
    );
}
```

### Task 2.3: Test startup refusal behavior

- [ ] **Step 4: Add an integration-style test for refusal**

This is tricky as a unit test because it involves `Args::parse()` and process-level startup. Instead, validate the refusal logic via a unit test on the `PeerAuthPolicy` construction:

In `crates/agenticd/src/main.rs` (or wherever `PeerAuthPolicy` lives), add a `#[cfg(test)] mod` block:

```rust
#[cfg(test)]
mod peer_auth_tests {
    use super::*;

    #[test]
    fn allowlist_admits_listed_uids_only() {
        let policy = PeerAuthPolicy::Allowlist([1000, 65532].into_iter().collect());
        assert!(policy.is_allowed(1000));
        assert!(policy.is_allowed(65532));
        assert!(!policy.is_allowed(0));
        assert!(!policy.is_allowed(99));
    }

    #[test]
    fn insecure_mode_admits_everything() {
        let policy = PeerAuthPolicy::InsecureAllowAny;
        assert!(policy.is_allowed(0));
        assert!(policy.is_allowed(u32::MAX));
    }
}
```

- [ ] **Step 5: Run the tests**

```bash
cargo test -p agenticd --lib peer_auth_tests 2>&1 | tail -5
```

Expected: 2 passed.

### Task 2.4: Commit Task 2

- [ ] **Step 6: Commit**

```bash
git add crates/agenticd/src/main.rs
git commit -m "agenticd: CLI flags + startup refusal for peer-UID allowlist (ADR-0012)

Implements ADR-0012 Decisions 1 and 3 (CLI surface only — the
accept-loop SO_PEERCRED check lands in the next commit).

- New --allowed-uid <UID> repeatable flag.
- New --insecure-allow-any-uid escape hatch (mutually exclusive with
  --allowed-uid).
- Daemon refuses to start with no policy configured; refuses to start
  if both flags are passed; logs a tracing::warn at startup when
  running in insecure mode.

Two unit tests cover allowlist admission and insecure-mode behavior.

Refs ADR-0012 Decisions 1, 3; closes part of TM-001."
```

---

## Task 3 — Wire accept-loop peer-cred check + DaemonState propagation

**Files:**
- Modify: `crates/agenticd/src/main.rs` — accept loop reads peer_cred, gates on `PeerAuthPolicy`.
- Modify: `crates/agenticd/src/server.rs` — `DaemonState` gains `peer_auth: Arc<PeerAuthPolicy>`; `handle_connection` grows a `peer_uid: Option<u32>` parameter.
- Modify: `crates/agenticd/src/commit.rs` — `execute_with_now` reads `peer_uid` from the dispatch context.

### Task 3.1: Extend DaemonState

- [ ] **Step 1: Add `peer_auth` to `DaemonState`**

In `crates/agenticd/src/server.rs`, find `pub struct DaemonState` (around line 28). Add a field:

```rust
    /// Peer-UID policy applied at socket-accept time. Constructed at
    /// startup from CLI flags; carried here so `DaemonState::open`
    /// callers in integration tests can construct one explicitly.
    pub peer_auth: std::sync::Arc<crate::PeerAuthPolicy>,
```

(Adjust the path to wherever `PeerAuthPolicy` lives; if it's in `main.rs`, you'll want to move it to a module like `crates/agenticd/src/peer_auth.rs` so it's importable from `server.rs`. Refactor as part of this step. Keep the type < 30 lines including tests; it doesn't warrant a large module.)

Update `DaemonState::open` to accept the policy. Update every caller in the test suite and in `main.rs`.

### Task 3.2: Extend `handle_connection`

- [ ] **Step 2: Add `peer_uid` to the handler signature**

In `crates/agenticd/src/server.rs`:

```rust
pub async fn handle_connection(
    state: Arc<DaemonState>,
    sock: UnixStream,
    peer_uid: Option<u32>,
) -> anyhow::Result<()> {
    // ... existing body ...
}
```

Then thread `peer_uid` to `dispatch`:

```rust
async fn dispatch(state: Arc<DaemonState>, request: Request, peer_uid: Option<u32>) -> anyhow::Result<Response> {
    match request {
        // ...
        Request::Commit(input) => {
            state.check_shutdown()?;
            let _guard = Arc::clone(&state.commit_lock).lock_owned().await;
            state.check_shutdown()?;
            let out = crate::commit::execute(Arc::clone(&state), input, peer_uid).await?;
            Ok(Response::Commit(out))
        }
        // ... and for Rollback similarly, pass peer_uid into rollback::execute (no-op there for now; PR-4 uses it).
    }
}
```

### Task 3.3: Extend `commit::execute` to thread peer_uid

- [ ] **Step 3: Update signature + body**

In `crates/agenticd/src/commit.rs`:

```rust
pub async fn execute(
    state: Arc<DaemonState>,
    input: CommitInput,
    peer_uid: Option<u32>,
) -> anyhow::Result<CommitOutput> {
    execute_with_now(state, input, peer_uid, chrono::Utc::now()).await
}

pub async fn execute_with_now(
    state: Arc<DaemonState>,
    input: CommitInput,
    peer_uid: Option<u32>,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<CommitOutput> {
    // ... existing body until assemble_inputs ...
    let inputs = assemble_inputs(input, parent, memory_snapshot, schema_version, tools, peer_uid);
    // ...
}
```

Update `assemble_inputs` to take `peer_uid: Option<u32>` and write it into the returned `CommitInputs`.

Update all existing test sites in `crates/agenticd/src/commit.rs::tests` that call `execute` / `execute_with_now` to pass `peer_uid: None`. Tests don't run over a socket; `None` is correct.

### Task 3.4: Wire the accept loop

- [ ] **Step 4: Read peer_cred and gate on policy**

In `crates/agenticd/src/main.rs`'s accept loop (inside the `tokio::select!` arm that handles `accept = listener.accept()`):

```rust
accept = listener.accept() => {
    let (sock, _addr) = match accept {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(error = %e, "accept failed");
            continue;
        }
    };
    // Read peer credentials before any I/O.
    let cred = match sock.peer_cred() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "peer_cred() failed; closing connection");
            continue;
        }
    };
    let peer_uid: u32 = cred.uid();
    let peer_pid: Option<i32> = cred.pid();

    if !state.peer_auth.is_allowed(peer_uid) {
        tracing::warn!(
            target: "agenticd::accept",
            peer_uid,
            peer_pid = ?peer_pid,
            "connection rejected: UID not in allowlist"
        );
        drop(sock);
        continue;
    }
    tracing::debug!(
        target: "agenticd::accept",
        peer_uid,
        peer_pid = ?peer_pid,
        "connection accepted"
    );

    let carried_uid = match &*state.peer_auth {
        PeerAuthPolicy::InsecureAllowAny => None,  // don't attest with insecure-mode UID
        PeerAuthPolicy::Allowlist(_) => Some(peer_uid),
    };

    let state = state.clone();
    tokio::task::spawn_local(async move {
        if let Err(e) = handle_connection(state, sock, carried_uid).await {
            tracing::warn!(error = %format!("{e:#}"), "connection error");
        }
    });
}
```

Note the `carried_uid` selection: under `--insecure-allow-any-uid` we deliberately do NOT attest commits with the connection's UID, because that UID has no security meaning. Under the allowlist policy, we attest.

### Task 3.5: Test the accept-loop behavior

- [ ] **Step 5: Add an integration test**

Create `crates/agenticd/tests/peer_auth_integration.rs`. This is a Linux-only test (gated with `#[cfg(target_os = "linux")]`) that:

1. Spawns `agenticd` as a child process bound to a tempdir socket.
2. Connects to the socket from the test process; sends a `Ping`; expects `Pong`.
3. (The test runs under the same UID as the daemon, so the connection should be allowed if that UID is in `--allowed-uid`.)

A minimal skeleton:

```rust
//! Integration test for ADR-0012 socket peer-auth.
//!
//! Linux-only: macOS-native development paths use --insecure-allow-any-uid
//! and don't exercise the SO_PEERCRED code path.

#![cfg(target_os = "linux")]

use agentic_proto::framing::{read_frame, write_frame};
use agentic_proto::{Envelope, Request, Response};
use std::process::{Child, Command};
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
    let mut sock = UnixStream::connect(sock_path).await?;
    let (read, write) = sock.split();
    let mut reader = tokio::io::BufReader::new(read);
    let mut writer = tokio::io::BufWriter::new(write);
    write_frame(&mut writer, &Envelope { correlation_id: "t1".into(), payload: Request::Ping }).await?;
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
        .arg("--repo").arg(dir.path())
        .arg("--socket").arg(&sock)
        .arg("--allowed-uid").arg(uid.to_string())
        .spawn().expect("spawn agenticd");
    // Wait for the socket to appear (bounded).
    for _ in 0..50 {
        if sock.exists() { break; }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sock.exists(), "agenticd never created its socket");
    ping(&sock).await.expect("ping should succeed under allowlisted UID");
    child.kill().ok();
    let _ = child.wait();
}
```

A second test case for "non-allowlisted UID is rejected" requires running the test process under a different UID, which is difficult without setuid privileges or a child-process workaround. Skip the negative test in this PR and instead rely on the `PeerAuthPolicy::is_allowed` unit tests from Task 2 to cover the rejection logic.

- [ ] **Step 6: Add `libc` to the dev-dependencies in `crates/agenticd/Cargo.toml`**

```toml
[dev-dependencies]
libc = "0.2"
# ... other dev deps
```

(Only as a `dev-dependency`. The daemon proper does not need `libc` because `tokio::net::UnixStream::peer_cred()` already handles the syscall.)

- [ ] **Step 7: Run the integration test**

```bash
cargo test -p agenticd --test peer_auth_integration 2>&1 | tail -5
```

Expected on Linux: `1 passed`. Expected on macOS: 0 tests collected (whole file gated out by `#[cfg(target_os = "linux")]`).

### Task 3.6: Run the full workspace check

- [ ] **Step 8: Full sweep**

```bash
cargo check --workspace 2>&1 | tail -5
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5
cargo test --workspace --lib 2>&1 | tail -5
```

Expected: all green.

### Task 3.7: Commit Task 3

- [ ] **Step 9: Commit**

```bash
git add crates/agenticd/src/main.rs crates/agenticd/src/server.rs crates/agenticd/src/commit.rs crates/agenticd/tests/peer_auth_integration.rs crates/agenticd/Cargo.toml
git commit -m "agenticd: SO_PEERCRED accept-loop check + peer_uid threading (ADR-0012)

Implements ADR-0012 Decisions 1, 2, 5, 6 (the wire side).

- Accept loop calls UnixStream::peer_cred() before any I/O on each
  accepted connection. Rejects with a tracing::warn line when the UID
  is not in --allowed-uid. Accepts with a tracing::debug line otherwise.
- DaemonState carries Arc<PeerAuthPolicy>; handle_connection takes the
  peer_uid through to dispatch; commit::execute writes it into the
  CommitInputs that go to agentic-core::stage_and_commit_with_now.
- Under --insecure-allow-any-uid the UID is deliberately NOT attested
  on commits (the UID has no security meaning in that mode).
- New peer_auth_integration test (Linux-only) spawns agenticd, pings
  from the same UID, expects success. The rejection path is unit-
  tested via PeerAuthPolicy::is_allowed in main.rs::peer_auth_tests.

Refs ADR-0012 Decisions 1, 2, 5, 6; closes TM-001 and TM-003."
```

---

## Task 4 — Pre-flight + push + open PR

### Task 4.1: Final verification

- [ ] **Step 1: Full workspace verification**

```bash
cargo check --workspace 2>&1 | tail -3
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3
cargo test --workspace --lib 2>&1 | tail -5
cargo test -p agenticd --test peer_auth_integration 2>&1 | tail -3
cargo fmt --check 2>&1 | tail -3
```

Expected: all green.

- [ ] **Step 2: Demo path sanity check**

```bash
docker compose -f examples/langgraph-rollback/docker-compose.yml up -d 2>&1 | tail -3
DATABASE_URL=postgres://agentic:agentic@localhost:54322/agentic \
    examples/langgraph-rollback/scripts/run-demo.sh 2>&1 | tail -10
```

Note: the demo's `run-demo.sh` will need an `--allowed-uid $(id -u)` flag OR `--insecure-allow-any-uid`. Update the script in this PR if the demo breaks; the simplest fix is to add `--insecure-allow-any-uid` to the daemon-invocation line in the script (the demo is by definition the operator-trusted local path; the flag's name is self-documenting about that).

If you have to modify `examples/langgraph-rollback/scripts/run-demo.sh`, include it in the Task 3 commit.

### Task 4.2: Push and open PR

- [ ] **Step 3: Push**

```bash
git push -u origin feat/pr2-socket-peer-auth-and-commit-attestation 2>&1 | tail -3
```

- [ ] **Step 4: Open the PR**

```bash
gh pr create --title "agenticd: PR-2 socket peer auth + Commit attestation (ADR-0012; closes TM-001/003)" --body "$(cat <<'EOF'
## Summary

Second PR of the pre-public-release hardening sprint. Implements [ADR-0012](docs/adr/0012-socket-peer-authentication.md). Closes TM-001 (worker enumerates objects via \`ReadObject\`) and TM-003 (worker forges Commits) from \`git.agentic-threat-model.md\`.

- \`agenticd\` reads peer credentials via \`UnixStream::peer_cred()\` on every accept. Rejects connections whose UID is not in the \`--allowed-uid\` allowlist before \`handle_connection\` runs.
- New CLI flags: \`--allowed-uid <UID>\` (repeatable) and \`--insecure-allow-any-uid\` (mutually exclusive). Daemon refuses to start without a policy configured.
- \`Commit\` object gains additive \`peer_uid: Option<u32>\`. Threaded from the accept loop through \`DaemonState\` → \`handle_connection\` → \`dispatch\` → \`commit::execute\` → \`agentic-core::stage_and_commit_with_now\` into the BLAKE3-committed canonical bytes.
- Under \`--insecure-allow-any-uid\` the UID is deliberately NOT attested on commits.
- Every peer-auth decision emits a structured \`tracing\` event.

## Test plan

- [x] \`cargo test -p agentic-core --lib commit_peer_uid_serde_roundtrip\` — passes
- [x] \`cargo test -p agenticd --lib peer_auth_tests\` — 2 passed
- [x] \`cargo test -p agenticd --test peer_auth_integration\` — 1 passed (Linux only)
- [x] \`cargo test --workspace --lib\` — green
- [x] \`cargo clippy --workspace --all-targets -- -D warnings\` — green
- [x] \`cargo fmt --check\` — clean
- [x] Broken-prompt demo runs end-to-end (script updated to pass \`--insecure-allow-any-uid\` for the demo path).

Sprint design: [docs/superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md](docs/superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md).
Plan for this PR: [docs/superpowers/plans/2026-05-21-pr2-socket-peer-auth-and-commit-attestation.md](docs/superpowers/plans/2026-05-21-pr2-socket-peer-auth-and-commit-attestation.md).

## Wire-compat note

The \`Commit\` object's JSON schema gains an additive \`Option<u32>\` field (\`#[serde(default)]\`). Per ADR-0002 D6: older readers ignore it; older writers produce commits with \`peer_uid: null\`, which validators treat as pre-attestation history. No client breakage.

## macOS note

\`SO_PEERCRED\` is Linux-specific. macOS-native development uses \`--insecure-allow-any-uid\` per ADR-0012 D5. A future ADR may add \`LOCAL_PEERCRED\` / \`getpeereid()\` support; v1.0 does not.
EOF
)" 2>&1 | tail -3
```

Expected: PR URL printed.

---

## Self-Review

Run after the plan is fully drafted (i.e., now):

**Spec coverage:** Spec PR-2 specifies 6 files to change (main.rs accept + flags, commit.rs schema, object.rs schema, server.rs DaemonState, commit.rs execute, agentic-core/commit.rs CommitInputs). Tasks 1–3 cover all of them. ✓

**Placeholder scan:** No TBD/TODO/"appropriate" placeholders. Every step has either concrete code or a concrete command. ✓

**Type consistency:** `PeerAuthPolicy` is defined once in Task 2 with the same enum variant names (`Allowlist`, `InsecureAllowAny`) referenced in Task 3. The `peer_uid: Option<u32>` field name is consistent across `CommitInputs`, `Commit`, the `handle_connection` parameter, and the integration test. `is_allowed(uid: u32) -> bool` signature is consistent between the unit tests and the accept-loop call site. ✓

**Scope:** plan implements only ADR-0012 (no scanner, no rollback gate, no GCS). No drift. ✓

---

## Done definition for this plan

- Branch `feat/pr2-socket-peer-auth-and-commit-attestation` pushed.
- PR opened against `main`.
- 3 commits on the branch (schema, CLI/refusal, accept-loop wiring) — or 2 if Tasks 2 and 3 bundle naturally.
- `cargo test --workspace --lib` green.
- `cargo test -p agenticd --test peer_auth_integration` green on Linux, 0-skipped on macOS.
- `cargo clippy --workspace --all-targets -- -D warnings` green.
- Broken-prompt demo runs end-to-end.
- TM-001 and TM-003 from the threat model file get a "Status: shipped in PR-2" note in the "Existing controls" column.
