# Issue #118 Socket Availability Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound every unbounded resource on the `agenticd` socket: global + per-UID connection caps, per-UID request rate budgets, a `commit_lock` queue-depth bound with an observable gauge, and a write-idle deadline.

**Architecture:** A new `crates/agenticd/src/limits.rs` module owns all admission-control state (config, connection gate, token-bucket rate limiter). `DaemonState` gains limits fields and a commit-queue-slot helper. The accept loop (`main.rs`) consults the connection gate; the connection loop (`server.rs`) consults the rate limiter and enforces the write-idle deadline. Rejections that can be attributed to a request get structured `Concurrency`-class retryable errors; unattributable ones (connection cap, write stall) are log-and-close.

**Tech Stack:** Rust 1.95, tokio primitives only (Semaphore, Mutex, timeout), `tracing` for observability. **No new dependencies.**

**Spec:** `docs/superpowers/specs/2026-07-10-issue-118-socket-availability-design.md` (same worktree). Read it first.

## Global Constraints

- Work in the existing worktree `.worktrees/issue-118-socket-limits/` (branch `issue-118-socket-limits`). Never edit the main checkout.
- No new crate dependencies. `Cargo.toml` must not change.
- No wire-protocol changes. New error codes are `Response::concurrency(...)` (class `Concurrency`, always `retryable: true`): exactly `"rate_budget_exhausted"` and `"commit_queue_full"`.
- All limits tracing events use `target: "agenticd::limits"`. Warn level for rejections, debug level for the queue-depth gauge.
- Flag defaults, exact: `--max-connections 64`, `--max-connections-per-uid 16`, `--rate-per-uid 200` (burst = 2× rate), `--commit-queue-depth 8`, `--read-idle-secs 30`, `--write-idle-secs 30`.
- Per-UID accounting keys on the **observed** `SO_PEERCRED` UID, in both auth modes (insecure mode included).
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` must pass after every task. No `unwrap()` in non-test code without a `// SAFETY:` or `// INVARIANT:` comment.
- The 2PC staging order and the `Lifecycle::drain` path must not change. Limits gate entry only.
- Commit messages: plain prose, imperative mood, no conventional-commits prefixes (repo style — the `feat:` examples in the skill doc do NOT apply here).
- Run all commands from inside the worktree: `cd .worktrees/issue-118-socket-limits`.

---

### Task 1: `limits.rs` — `LimitsConfig` with defaults and validation

**Files:**
- Create: `crates/agenticd/src/limits.rs`
- Modify: `crates/agenticd/src/lib.rs` (add `pub mod limits;`)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub struct LimitsConfig { pub max_connections: usize, pub max_connections_per_uid: usize, pub rate_per_uid: u32, pub commit_queue_depth: usize, pub read_idle: Duration, pub write_idle: Duration }` with `Default` (64/16/200/8/30s/30s) and `pub fn validate(&self) -> anyhow::Result<()>`. Later tasks add `ConnGate` and `RateLimiter` to this same file.

- [ ] **Step 1: Write the failing tests**

Create `crates/agenticd/src/limits.rs`:

```rust
//! Socket admission control — issue #118.
//!
//! Everything that bounds a previously unbounded resource on the daemon
//! socket lives here: the limits configuration (CLI-flag backed), the
//! connection gate (global + per-UID caps), and the per-UID token-bucket
//! rate limiter. The commit-queue slot bound lives on `DaemonState`
//! because it wraps `commit_lock`, which lives there.
//!
//! Enforcement points:
//! * accept loop (`main.rs`) — `ConnGate`, log-and-close (no frame yet,
//!   nothing to attribute a structured reply to).
//! * connection loop (`server.rs`) — `RateLimiter`, structured
//!   `Concurrency`-class retryable reply; write-idle deadline.
//! * dispatch write path (`server.rs`) — commit-queue slots.

use std::time::Duration;

/// Tunable limits, one field per CLI flag. Static per process — reload
/// means bouncing the daemon, same as the ADR-0013 scanner allowlist.
#[derive(Debug, Clone)]
pub struct LimitsConfig {
    /// Global cap on concurrently open connections.
    pub max_connections: usize,
    /// Per-UID cap on concurrently open connections.
    pub max_connections_per_uid: usize,
    /// Per-UID request budget, requests/second. Burst capacity is 2×.
    pub rate_per_uid: u32,
    /// Max requests queued-or-executing on `commit_lock`.
    pub commit_queue_depth: usize,
    /// Deadline for reading one complete inbound frame (per-frame clock).
    pub read_idle: Duration,
    /// Deadline for writing one complete response frame.
    pub write_idle: Duration,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: 64,
            max_connections_per_uid: 16,
            rate_per_uid: 200,
            commit_queue_depth: 8,
            read_idle: Duration::from_secs(30),
            write_idle: Duration::from_secs(30),
        }
    }
}

impl LimitsConfig {
    /// Reject configurations that would deny all service. Zero anywhere
    /// is an operator mistake; refuse loudly at startup rather than run
    /// a daemon that drops every connection.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.max_connections >= 1, "--max-connections must be >= 1");
        anyhow::ensure!(
            self.max_connections_per_uid >= 1,
            "--max-connections-per-uid must be >= 1"
        );
        anyhow::ensure!(self.rate_per_uid >= 1, "--rate-per-uid must be >= 1");
        anyhow::ensure!(
            self.commit_queue_depth >= 1,
            "--commit-queue-depth must be >= 1"
        );
        anyhow::ensure!(
            !self.read_idle.is_zero(),
            "--read-idle-secs must be >= 1"
        );
        anyhow::ensure!(
            !self.write_idle.is_zero(),
            "--write-idle-secs must be >= 1"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = LimitsConfig::default();
        assert_eq!(c.max_connections, 64);
        assert_eq!(c.max_connections_per_uid, 16);
        assert_eq!(c.rate_per_uid, 200);
        assert_eq!(c.commit_queue_depth, 8);
        assert_eq!(c.read_idle, Duration::from_secs(30));
        assert_eq!(c.write_idle, Duration::from_secs(30));
        c.validate().expect("defaults must validate");
    }

    #[test]
    fn zero_values_are_rejected() {
        for cfg in [
            LimitsConfig { max_connections: 0, ..Default::default() },
            LimitsConfig { max_connections_per_uid: 0, ..Default::default() },
            LimitsConfig { rate_per_uid: 0, ..Default::default() },
            LimitsConfig { commit_queue_depth: 0, ..Default::default() },
            LimitsConfig { read_idle: Duration::ZERO, ..Default::default() },
            LimitsConfig { write_idle: Duration::ZERO, ..Default::default() },
        ] {
            assert!(cfg.validate().is_err(), "must reject: {cfg:?}");
        }
    }
}
```

Add to `crates/agenticd/src/lib.rs` after `pub mod lifecycle;`:

```rust
pub mod limits;
```

- [ ] **Step 2: Run tests to verify they pass** (struct + tests land together in Rust; the "fail" state is the pre-file compile error)

Run: `cargo test -p agenticd --lib limits`
Expected: PASS, 2 tests.

- [ ] **Step 3: Gates**

Run: `cargo clippy -p agenticd --all-targets -- -D warnings && cargo fmt --check`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/agenticd/src/limits.rs crates/agenticd/src/lib.rs
git commit -m "Add LimitsConfig with defaults and startup validation (issue #118)"
```

---

### Task 2: `limits.rs` — `ConnGate` connection admission

**Files:**
- Modify: `crates/agenticd/src/limits.rs`

**Interfaces:**
- Consumes: `LimitsConfig` (Task 1).
- Produces:
  - `pub struct ConnGate` with `pub fn new(cfg: &LimitsConfig) -> Arc<Self>` and `pub fn try_admit(self: &Arc<Self>, uid: u32) -> Result<ConnGuard, ConnRejection>`.
  - `pub struct ConnGuard` (RAII; `Drop` releases the slot).
  - `#[derive(Debug, PartialEq, Eq)] pub enum ConnRejection { GlobalCap { current: usize, cap: usize }, PerUidCap { uid: u32, current: usize, cap: usize } }`.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `limits.rs`:

```rust
    #[test]
    fn global_cap_rejects_and_guard_drop_readmits() {
        let cfg = LimitsConfig {
            max_connections: 2,
            max_connections_per_uid: 10,
            ..Default::default()
        };
        let gate = ConnGate::new(&cfg);
        let g1 = gate.try_admit(1000).map_err(|e| format!("{e:?}")).expect("first admit");
        let _g2 = gate.try_admit(1001).map_err(|e| format!("{e:?}")).expect("second admit");
        let rej = gate.try_admit(1002).map(|_| ()).expect_err("third must be rejected");
        assert_eq!(rej, ConnRejection::GlobalCap { current: 2, cap: 2 });
        drop(g1);
        gate.try_admit(1002)
            .map(|_| ())
            .expect("guard drop must free the slot");
    }

    #[test]
    fn per_uid_cap_rejects_only_that_uid() {
        let cfg = LimitsConfig {
            max_connections: 10,
            max_connections_per_uid: 1,
            ..Default::default()
        };
        let gate = ConnGate::new(&cfg);
        // Bind the guard itself, not a mapped unit — dropping the guard
        // would release the slot and defeat the test.
        let _held = gate
            .try_admit(1000)
            .unwrap_or_else(|e| panic!("first admit: {e:?}"));
        let rej = gate.try_admit(1000).map(|_| ()).expect_err("same uid over cap");
        assert_eq!(
            rej,
            ConnRejection::PerUidCap { uid: 1000, current: 1, cap: 1 }
        );
        gate.try_admit(2000)
            .map(|_| ())
            .expect("different uid has its own budget");
    }

    #[test]
    fn per_uid_entry_is_removed_when_count_hits_zero() {
        let cfg = LimitsConfig::default();
        let gate = ConnGate::new(&cfg);
        let g = gate.try_admit(1000).unwrap_or_else(|e| panic!("admit: {e:?}"));
        drop(g);
        // Internal check: the per-UID map must not leak an entry per
        // ever-seen UID.
        assert!(gate.counts.lock().expect("gate mutex").per_uid.is_empty());
    }
```

(Clean up the first two lines of `per_uid_cap_rejects_only_that_uid` when writing it — the guard must be held in a binding, as the corrected lines show. Final test body keeps only the corrected version.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p agenticd --lib limits`
Expected: COMPILE FAIL — `ConnGate` not found.

- [ ] **Step 3: Implement `ConnGate`**

Add to `limits.rs` (above the tests module):

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Why a connection was refused at the gate. Carried back to the accept
/// loop so the rejection log names the cap that tripped.
#[derive(Debug, PartialEq, Eq)]
pub enum ConnRejection {
    GlobalCap { current: usize, cap: usize },
    PerUidCap { uid: u32, current: usize, cap: usize },
}

#[derive(Default)]
struct GateCounts {
    global: usize,
    per_uid: HashMap<u32, usize>,
}

/// Global + per-UID connection admission. Checked in the accept loop
/// immediately after the ADR-0012 UID-allowlist check. Keys on the
/// observed `SO_PEERCRED` UID in both auth modes.
pub struct ConnGate {
    max_global: usize,
    max_per_uid: usize,
    counts: Mutex<GateCounts>,
}

impl ConnGate {
    pub fn new(cfg: &LimitsConfig) -> Arc<Self> {
        Arc::new(Self {
            max_global: cfg.max_connections,
            max_per_uid: cfg.max_connections_per_uid,
            counts: Mutex::new(GateCounts::default()),
        })
    }

    /// Admit a connection or say why not. The returned guard releases
    /// both counters on drop — hold it for the connection's lifetime.
    pub fn try_admit(self: &Arc<Self>, uid: u32) -> Result<ConnGuard, ConnRejection> {
        // INVARIANT: only plain arithmetic runs under this lock; no
        // panic path exists while it is held, so poisoning is
        // unreachable in practice. `expect` documents that.
        let mut counts = self.counts.lock().expect("ConnGate mutex poisoned");
        if counts.global >= self.max_global {
            return Err(ConnRejection::GlobalCap {
                current: counts.global,
                cap: self.max_global,
            });
        }
        let uid_count = counts.per_uid.get(&uid).copied().unwrap_or(0);
        if uid_count >= self.max_per_uid {
            return Err(ConnRejection::PerUidCap {
                uid,
                current: uid_count,
                cap: self.max_per_uid,
            });
        }
        counts.global += 1;
        *counts.per_uid.entry(uid).or_insert(0) += 1;
        Ok(ConnGuard {
            gate: Arc::clone(self),
            uid,
        })
    }
}

/// RAII admission token. Dropping it releases the global and per-UID
/// slots taken by `try_admit`.
pub struct ConnGuard {
    gate: Arc<ConnGate>,
    uid: u32,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        // INVARIANT: see try_admit — no panic path under this lock.
        let mut counts = self.gate.counts.lock().expect("ConnGate mutex poisoned");
        counts.global = counts.global.saturating_sub(1);
        if let Some(n) = counts.per_uid.get_mut(&self.uid) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.per_uid.remove(&self.uid);
            }
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p agenticd --lib limits`
Expected: PASS, 5 tests.

- [ ] **Step 5: Gates and commit**

```bash
cargo clippy -p agenticd --all-targets -- -D warnings && cargo fmt --check
git add crates/agenticd/src/limits.rs
git commit -m "Add ConnGate: global and per-UID connection caps (issue #118)"
```

---

### Task 3: `limits.rs` — `RateLimiter` per-UID token bucket

**Files:**
- Modify: `crates/agenticd/src/limits.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `pub struct RateLimiter` with `pub fn new(rate_per_uid: u32) -> Self` (burst = 2× rate) and `pub fn try_consume(&self, uid: u32, now: std::time::Instant) -> bool`. The caller passes `now` — production passes `Instant::now()`, tests pass constructed instants, so no clock mocking is needed.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module:

```rust
    use std::time::Instant;

    #[test]
    fn burst_then_exhaustion_then_refill() {
        let rl = RateLimiter::new(1); // 1 req/s, burst 2
        let t0 = Instant::now();
        assert!(rl.try_consume(1000, t0));
        assert!(rl.try_consume(1000, t0));
        assert!(!rl.try_consume(1000, t0), "burst of 2 must be exhausted");
        let t1 = t0 + Duration::from_millis(1100);
        assert!(rl.try_consume(1000, t1), "1.1s at 1/s refills >= 1 token");
        assert!(!rl.try_consume(1000, t1), "only ~1.1 tokens refilled");
    }

    #[test]
    fn buckets_are_per_uid() {
        let rl = RateLimiter::new(1);
        let t0 = Instant::now();
        assert!(rl.try_consume(1, t0));
        assert!(rl.try_consume(1, t0));
        assert!(!rl.try_consume(1, t0));
        assert!(rl.try_consume(2, t0), "uid 2 has its own bucket");
    }

    #[test]
    fn refill_caps_at_burst() {
        let rl = RateLimiter::new(1); // burst 2
        let t0 = Instant::now();
        assert!(rl.try_consume(1, t0));
        let t1 = t0 + Duration::from_secs(100);
        assert!(rl.try_consume(1, t1));
        assert!(rl.try_consume(1, t1));
        assert!(
            !rl.try_consume(1, t1),
            "100s idle must refill to burst (2), not accumulate 100 tokens"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p agenticd --lib limits`
Expected: COMPILE FAIL — `RateLimiter` not found.

- [ ] **Step 3: Implement `RateLimiter`**

Add to `limits.rs`:

```rust
use std::time::Instant;

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Per-UID token bucket. `try_consume` takes `now` as a parameter so
/// tests drive the clock with plain `Instant` arithmetic — no mock
/// clock machinery, no sleeps in unit tests.
pub struct RateLimiter {
    rate_per_sec: f64,
    burst: f64,
    buckets: Mutex<HashMap<u32, Bucket>>,
}

impl RateLimiter {
    /// Burst capacity is fixed at 2× the sustained rate (spec §flags).
    pub fn new(rate_per_uid: u32) -> Self {
        let rate = f64::from(rate_per_uid);
        Self {
            rate_per_sec: rate,
            burst: rate * 2.0,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take one token from `uid`'s bucket. Returns false when the
    /// budget is exhausted. A new UID starts with a full burst.
    pub fn try_consume(&self, uid: u32, now: Instant) -> bool {
        // INVARIANT: only arithmetic under this lock; no panic path,
        // so poisoning is unreachable in practice.
        let mut buckets = self.buckets.lock().expect("RateLimiter mutex poisoned");
        let b = buckets.entry(uid).or_insert(Bucket {
            tokens: self.burst,
            last_refill: now,
        });
        let elapsed = now.saturating_duration_since(b.last_refill).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.rate_per_sec).min(self.burst);
        b.last_refill = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}
```

Note: `use std::time::Instant;` at module level replaces the test-local `use` from Step 1 if rustc flags a duplicate — keep exactly one, at module level.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p agenticd --lib limits`
Expected: PASS, 8 tests.

- [ ] **Step 5: Gates and commit**

```bash
cargo clippy -p agenticd --all-targets -- -D warnings && cargo fmt --check
git add crates/agenticd/src/limits.rs
git commit -m "Add per-UID token-bucket rate limiter (issue #118)"
```

---

### Task 4: `DaemonState` — limits fields, commit-queue slots, dispatch-arm rejection

**Files:**
- Modify: `crates/agenticd/src/server.rs` (struct `DaemonState` around line 45–174; dispatch arms `Request::Commit` ~line 493, `Request::Diff` ~line 525, `Request::Rollback` ~line 587; tests module)

**Interfaces:**
- Consumes: `LimitsConfig`, `RateLimiter` (Tasks 1, 3).
- Produces (used by Tasks 5–6):
  - `DaemonState` fields: `pub limits: crate::limits::LimitsConfig`, `pub rate: crate::limits::RateLimiter`, `pub commit_slots: Arc<tokio::sync::Semaphore>`, `pub commit_queue_depth: Arc<std::sync::atomic::AtomicUsize>`.
  - `pub fn with_limits(self, cfg: crate::limits::LimitsConfig) -> Self` (builder, like `with_approval_key`).
  - `pub fn try_commit_slot(&self, peer_uid: Option<u32>) -> Option<CommitSlot>` and `pub struct CommitSlot` (RAII; drop releases permit + decrements gauge).
  - Test helper `minimal_state_with_limits(cfg) -> (Arc<DaemonState>, TempDir)`; existing `minimal_state()` delegates to it with defaults.

- [ ] **Step 1: Write the failing test**

In the `tests` module of `server.rs`, first refactor the helper (replace the body of `minimal_state` with a delegation):

```rust
    /// Build a minimal daemon state with explicit limits. Returns the
    /// `TempDir` too — the caller keeps it alive for the test.
    async fn minimal_state_with_limits(
        cfg: crate::limits::LimitsConfig,
    ) -> (std::sync::Arc<DaemonState>, tempfile::TempDir) {
        use agentic_core::{FsObjectStore, ObjectStore};
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let agentic_dir = dir.path().join(".agentic");
        std::fs::create_dir_all(&agentic_dir).unwrap();
        let store: Arc<dyn ObjectStore + Send + Sync> =
            Arc::new(FsObjectStore::open(agentic_dir.join("objects")).unwrap());
        let state = Arc::new(
            DaemonState::open(
                dir.path().to_path_buf(),
                agentic_dir,
                store,
                None,
                Vec::new(),
                Vec::new(),
                Arc::new(crate::peer_auth::PeerAuthPolicy::InsecureAllowAny),
            )
            .await
            .unwrap()
            .with_limits(cfg),
        );
        (state, dir)
    }

    async fn minimal_state() -> (std::sync::Arc<DaemonState>, tempfile::TempDir) {
        minimal_state_with_limits(crate::limits::LimitsConfig::default()).await
    }
```

Then add the new test:

```rust
    /// Issue #118: with the commit queue bounded at 1, a second
    /// lock-taking request is rejected with a structured retryable
    /// Concurrency error instead of parking unboundedly, and the depth
    /// gauge tracks the queued occupant.
    #[tokio::test]
    async fn commit_queue_full_rejects_instead_of_parking() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let cfg = crate::limits::LimitsConfig {
            commit_queue_depth: 1,
            ..Default::default()
        };
        let (state, _dir) = minimal_state_with_limits(cfg).await;

        // Two commits so Diff has refs to resolve.
        let first = make_commit(&state, "main", "first", b"v1".to_vec()).await;
        let second = make_commit(&state, "main", "second", b"v2".to_vec()).await;
        assert_ne!(first, second);

        // Hold commit_lock from the test side so the occupier parks.
        let guard = Arc::clone(&state.commit_lock).lock_owned().await;

        // Occupier: takes the single queue slot, parks on the lock.
        let occupier = dispatch(
            Arc::clone(&state),
            Request::Diff {
                from: first.to_hex(),
                to: "main".to_string(),
            },
            None,
        );
        tokio::pin!(occupier);
        tokio::select! {
            res = &mut occupier => panic!("occupier must park on commit_lock, got {res:?}"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        assert_eq!(
            state.commit_queue_depth.load(Ordering::Relaxed),
            1,
            "gauge must count the parked occupant"
        );

        // Queue is full: the next lock-taking request is rejected NOW,
        // not parked.
        let mut prompts = std::collections::BTreeMap::new();
        prompts.insert("system.md".to_string(), b"v3".to_vec());
        let rejected = tokio::time::timeout(
            Duration::from_secs(1),
            dispatch(
                Arc::clone(&state),
                Request::Commit(agentic_proto::CommitInput {
                    message: "rejected".to_string(),
                    author: Some("tester".to_string()),
                    code_sha: None,
                    branch: Some("main".to_string()),
                    prompts,
                    mcp_servers: Vec::new(),
                    model: None,
                    no_memory: true,
                }),
                None,
            ),
        )
        .await
        .expect("rejection must be immediate, not queued")
        .expect("dispatch returns Ok(Response::Error), not Err");
        match rejected {
            Response::Error {
                class,
                code,
                retryable,
                ..
            } => {
                assert_eq!(class, agentic_proto::ErrorClass::Concurrency);
                assert_eq!(code, "commit_queue_full");
                assert!(retryable, "queue-full must be retryable");
            }
            other => panic!("expected Concurrency error, got {other:?}"),
        }

        // Release the lock: the occupier completes and the gauge drains.
        drop(guard);
        let response = tokio::time::timeout(Duration::from_secs(2), &mut occupier)
            .await
            .expect("occupier completes once lock is free")
            .expect("occupier diff succeeds");
        assert!(matches!(response, Response::Diff(_)));
        assert_eq!(state.commit_queue_depth.load(Ordering::Relaxed), 0);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p agenticd --lib commit_queue_full`
Expected: COMPILE FAIL — `with_limits`, `commit_queue_depth`, `try_commit_slot` not found.

- [ ] **Step 3: Implement the state fields and slot helper**

In `DaemonState` (after the `approval_key` field), add:

```rust
    /// Static limits in force (issue #118). Set at startup via
    /// [`DaemonState::with_limits`]; defaults are spec values.
    pub limits: crate::limits::LimitsConfig,
    /// Per-UID request rate budget, keyed on the observed peer UID.
    pub rate: crate::limits::RateLimiter,
    /// Bound on requests queued-or-executing on `commit_lock`. A
    /// dispatch arm that would take the lock first takes a slot here;
    /// `try_acquire` failure is an immediate structured rejection
    /// instead of a silent unbounded queue.
    pub commit_slots: Arc<tokio::sync::Semaphore>,
    /// Observable commit-queue depth (queued + executing). Mirrors the
    /// semaphore purely for logging — the semaphore enforces, this
    /// reports.
    pub commit_queue_depth: Arc<std::sync::atomic::AtomicUsize>,
```

In `DaemonState::open`, initialise in the `Ok(Self { ... })` literal (after `approval_key: None,`):

```rust
            limits: crate::limits::LimitsConfig::default(),
            rate: crate::limits::RateLimiter::new(
                crate::limits::LimitsConfig::default().rate_per_uid,
            ),
            commit_slots: Arc::new(tokio::sync::Semaphore::new(
                crate::limits::LimitsConfig::default().commit_queue_depth,
            )),
            commit_queue_depth: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
```

After `with_approval_key`, add the builder and the slot helper:

```rust
    /// Attach the limits configuration (issue #118). Builder-style like
    /// `with_approval_key`; rebuilds the rate limiter and the commit
    /// queue semaphore to match. Call before serving traffic.
    pub fn with_limits(mut self, cfg: crate::limits::LimitsConfig) -> Self {
        self.rate = crate::limits::RateLimiter::new(cfg.rate_per_uid);
        self.commit_slots = Arc::new(tokio::sync::Semaphore::new(cfg.commit_queue_depth));
        self.commit_queue_depth = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        self.limits = cfg;
        self
    }

    /// Take a commit-queue slot, or say the queue is full. Callers turn
    /// `None` into a `Response::concurrency("commit_queue_full", ..)`
    /// reply. The slot is held for the queued + lock-held duration, so
    /// the bound covers everything that can queue on `commit_lock`.
    pub fn try_commit_slot(&self, peer_uid: Option<u32>) -> Option<CommitSlot> {
        let permit = Arc::clone(&self.commit_slots).try_acquire_owned().ok()?;
        let depth = self
            .commit_queue_depth
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        tracing::debug!(
            target: "agenticd::limits",
            depth,
            peer_uid = ?peer_uid,
            "commit queue slot acquired"
        );
        Some(CommitSlot {
            _permit: permit,
            depth: Arc::clone(&self.commit_queue_depth),
            peer_uid,
        })
    }
```

Below `DaemonState`'s impl block, add:

```rust
/// RAII commit-queue slot (issue #118). Dropping it releases the
/// semaphore permit and decrements the observable depth gauge.
pub struct CommitSlot {
    _permit: tokio::sync::OwnedSemaphorePermit,
    depth: Arc<std::sync::atomic::AtomicUsize>,
    peer_uid: Option<u32>,
}

impl Drop for CommitSlot {
    fn drop(&mut self) {
        let depth = self
            .depth
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(1);
        tracing::debug!(
            target: "agenticd::limits",
            depth,
            peer_uid = ?self.peer_uid,
            "commit queue slot released"
        );
    }
}
```

- [ ] **Step 4: Gate the three lock-taking dispatch arms**

`Request::Commit` arm — insert between `state.check_shutdown()?;` and `let _guard = ...`:

```rust
            let Some(_slot) = state.try_commit_slot(peer_uid) else {
                tracing::warn!(
                    target: "agenticd::limits",
                    peer_uid = ?peer_uid,
                    depth = state
                        .commit_queue_depth
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "commit queue full; rejecting Commit"
                );
                return Ok(Response::concurrency(
                    "commit_queue_full",
                    "commit queue is full; retry shortly",
                ));
            };
```

`Request::Rollback` arm — identical block between its `state.check_shutdown()?;` and `let _guard = ...`, with the log message `"commit queue full; rejecting Rollback"`.

`Request::Diff` arm — insert the same block (message `"commit queue full; rejecting Diff"`) immediately before `let snapshot = { ... }`. The Diff arm has no `check_shutdown` today; do not add one — read paths stay serviceable during drain.

Note: the queue-full warn cannot carry `correlation_id` — `dispatch` never sees it (the envelope is peeled in the connection loop). The client's error reply still carries it in the envelope. This is a recorded, accepted deviation from the spec's observability table.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p agenticd --lib`
Expected: PASS — new test plus all existing server tests (`dispatch_diff_blocks_on_commit_lock` still passes: default queue depth 8 admits its single occupier).

- [ ] **Step 6: Gates and commit**

```bash
cargo clippy -p agenticd --all-targets -- -D warnings && cargo fmt --check
git add crates/agenticd/src/server.rs
git commit -m "Bound the commit_lock queue with slots and an observable depth gauge (issue #118)"
```

---

### Task 5: Connection loop — rate budget, write-idle deadline, config-driven read-idle

**Files:**
- Modify: `crates/agenticd/src/server.rs` (constant at ~line 42, `handle_connection` / `handle_connection_with_idle_timeout` at ~lines 178–324, tests)
- Modify: `crates/agenticd/src/main.rs` (single `handle_connection` call site in the accept loop, ~line 359)

**Interfaces:**
- Consumes: `DaemonState.rate`, `DaemonState.limits` (Task 4).
- Produces (used by Tasks 6–7):
  - `pub async fn handle_connection(state: Arc<DaemonState>, sock: UnixStream, observed_uid: u32, peer_uid: Option<u32>) -> anyhow::Result<()>` — `observed_uid` is the raw `SO_PEERCRED` UID (limits accounting); `peer_uid` stays the ADR-0012 attestation identity (None under insecure mode).
  - `pub async fn handle_connection_with_deadlines(state, sock, observed_uid: u32, peer_uid: Option<u32>, read_idle: Duration, write_idle: Duration) -> anyhow::Result<()>` — replaces `handle_connection_with_idle_timeout`.
  - Wire code `"rate_budget_exhausted"` observable by SDK clients.

- [ ] **Step 1: Write the failing tests**

Replace the two existing idle tests' calls to `handle_connection_with_idle_timeout(state, server, None, dur)` with `handle_connection_with_deadlines(state, server, 0, None, dur, std::time::Duration::from_secs(30))` (both `idle_connection_is_dropped_after_deadline` and `well_spaced_frames_are_not_dropped`). Then add:

```rust
    /// Issue #118: a request over the per-UID rate budget gets a
    /// structured retryable Concurrency reply and the connection
    /// SURVIVES — after refill the next request succeeds.
    #[tokio::test]
    async fn rate_exhausted_request_is_rejected_and_connection_survives() {
        use agentic_proto::framing::{read_frame, write_frame};
        use std::time::Duration;

        let cfg = crate::limits::LimitsConfig {
            rate_per_uid: 1, // burst 2
            ..Default::default()
        };
        let (state, _dir) = minimal_state_with_limits(cfg).await;
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();

        let local = tokio::task::LocalSet::new();
        let handle = local.spawn_local(handle_connection_with_deadlines(
            state,
            server,
            1000, // observed uid — any value; keys the bucket
            None,
            Duration::from_secs(30),
            Duration::from_secs(30),
        ));
        local
            .run_until(async move {
                // The burst of 2 passes.
                for i in 0..2 {
                    write_frame(&mut client, &Envelope::new(format!("ok-{i}"), Request::Ping))
                        .await
                        .unwrap();
                    let reply: Envelope<Response> =
                        read_frame(&mut client).await.unwrap().expect("reply");
                    assert!(matches!(reply.payload, Response::Pong));
                }
                // The third rapid request trips the budget.
                write_frame(&mut client, &Envelope::new("limited", Request::Ping))
                    .await
                    .unwrap();
                let reply: Envelope<Response> =
                    read_frame(&mut client).await.unwrap().expect("reply");
                match reply.payload {
                    Response::Error {
                        class,
                        code,
                        retryable,
                        ..
                    } => {
                        assert_eq!(class, agentic_proto::ErrorClass::Concurrency);
                        assert_eq!(code, "rate_budget_exhausted");
                        assert!(retryable);
                    }
                    other => panic!("expected rate rejection, got {other:?}"),
                }
                // Connection survived: after >1s the bucket refills.
                tokio::time::sleep(Duration::from_millis(1200)).await;
                write_frame(
                    &mut client,
                    &Envelope::new("after-refill", Request::Ping),
                )
                .await
                .unwrap();
                let reply: Envelope<Response> =
                    read_frame(&mut client).await.unwrap().expect("reply");
                assert!(matches!(reply.payload, Response::Pong));
                drop(client);
                let outcome = handle.await.expect("handler must not panic");
                assert!(outcome.is_ok(), "clean close expected, got {outcome:?}");
            })
            .await;
    }

    /// Issue #118: a response write that stalls (peer stops reading)
    /// hits the write-idle deadline instead of pinning the task. Unit
    /// test of the write helper via a tiny duplex buffer — a real
    /// UnixStream's kernel buffer is too large to fill with a Pong.
    #[tokio::test]
    async fn stalled_response_write_hits_write_idle_deadline() {
        use std::time::Duration;
        // 16-byte pipe: the serialized envelope exceeds it, so write_all
        // pends until someone reads. Nobody reads.
        let (client, mut server_side) = tokio::io::duplex(16);
        let reply = Envelope::new("w1".to_string(), Response::Pong);
        let err = write_reply(
            &mut server_side,
            &reply,
            Duration::from_millis(100),
            None,
            "w1",
        )
        .await
        .expect_err("stalled write must error out at the deadline");
        assert!(
            format!("{err:#}").contains("write-idle"),
            "error should name the write-idle deadline; got: {err:#}"
        );
        drop(client);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p agenticd --lib`
Expected: COMPILE FAIL — `handle_connection_with_deadlines`, `write_reply` not found.

- [ ] **Step 3: Implement the connection-loop changes**

3a. Delete the `READ_IDLE_TIMEOUT` constant (lines ~33–42). The default now lives in `LimitsConfig::default()`. Keep its doc-comment knowledge by moving the per-frame-clock explanation onto `LimitsConfig.read_idle` if not already there.

3b. Replace `handle_connection` and rename `handle_connection_with_idle_timeout`:

```rust
/// Handle a single accepted connection. Runs the read/dispatch/write
/// loop until the peer closes, misses the read-idle deadline, stalls a
/// response write past the write-idle deadline, or exhausts budgets in
/// a way that closes the connection.
///
/// `observed_uid` is the raw SO_PEERCRED UID — the key for limits
/// accounting in BOTH auth modes. `peer_uid` is the ADR-0012
/// attestation identity (None under --insecure-allow-any-uid) and is
/// what dispatch stamps into commits.
pub async fn handle_connection(
    state: Arc<DaemonState>,
    sock: UnixStream,
    observed_uid: u32,
    peer_uid: Option<u32>,
) -> anyhow::Result<()> {
    let read_idle = state.limits.read_idle;
    let write_idle = state.limits.write_idle;
    handle_connection_with_deadlines(state, sock, observed_uid, peer_uid, read_idle, write_idle)
        .await
}

/// [`handle_connection`] with both I/O deadlines injected, so tests can
/// exercise them without waiting the production windows.
pub async fn handle_connection_with_deadlines(
    state: Arc<DaemonState>,
    sock: UnixStream,
    observed_uid: u32,
    peer_uid: Option<u32>,
    read_idle: std::time::Duration,
    write_idle: std::time::Duration,
) -> anyhow::Result<()> {
```

The body keeps the existing read-idle logic verbatim. Make these three changes inside the loop:

3c. Add the rate check between the `parse_envelope_with_v0_shim` match and the `dispatch` call:

```rust
        // Issue #118: per-UID request rate budget, keyed on the
        // observed UID. Checked after envelope parse so the rejection
        // carries the correlation_id. The connection survives — a
        // rate-limited client backs off and retries on the same socket.
        if !state
            .rate
            .try_consume(observed_uid, std::time::Instant::now())
        {
            tracing::warn!(
                target: "agenticd::limits",
                peer_uid = ?peer_uid,
                observed_uid,
                correlation_id = %correlation_id,
                "per-UID rate budget exhausted; rejecting request"
            );
            let reply = Envelope::new(
                correlation_id.clone(),
                Response::concurrency(
                    "rate_budget_exhausted",
                    "per-UID request rate budget exhausted; retry shortly",
                ),
            );
            write_reply(&mut writer, &reply, write_idle, peer_uid, &correlation_id).await?;
            continue;
        }
```

3d. Add the `write_reply` helper (module level, above `handle_connection`), and route BOTH existing `write_frame(&mut writer, &reply)` call sites through it (the attributable parse-error reply and the dispatch reply), preserving their existing surrounding `tracing::warn!` + `return Err` structure — pass the error through, e.g. `if let Err(e) = write_reply(...).await { tracing::warn!(...existing fields..., write_error = %e, ...); return Err(e); }`:

```rust
/// Write one reply frame under the write-idle deadline (issue #118). A
/// peer that stops reading fills the socket buffer; without a deadline
/// the pended `write_all` would pin this task forever. On elapse we
/// log-and-close — mid-write there is no way to send anything else.
async fn write_reply<W>(
    writer: &mut W,
    reply: &Envelope<Response>,
    write_idle: std::time::Duration,
    peer_uid: Option<u32>,
    correlation_id: &str,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match tokio::time::timeout(write_idle, write_frame(writer, reply)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.into()),
        Err(_elapsed) => {
            tracing::warn!(
                target: "agenticd::limits",
                peer_uid = ?peer_uid,
                correlation_id = %correlation_id,
                write_idle_secs = write_idle.as_secs(),
                "response write stalled beyond write-idle deadline; closing"
            );
            Err(anyhow!(
                "response write stalled beyond {}s write-idle deadline",
                write_idle.as_secs()
            ))
        }
    }
}
```

3e. Update the single call site in `main.rs` (inside `spawn_local`):

```rust
                        tokio::task::spawn_local(async move {
                            if let Err(e) =
                                handle_connection(state, sock, peer_uid, carried_uid).await
                            {
```

(`peer_uid` is the raw `u32` from `cred.uid()`, already in scope; `carried_uid` is the attestation `Option<u32>`.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p agenticd --lib`
Expected: PASS — both new tests, both updated idle tests, all prior tests. The rate test takes ~1.3s (real-clock refill); that is accepted.

- [ ] **Step 5: Gates and commit**

```bash
cargo clippy -p agenticd --all-targets -- -D warnings && cargo fmt --check
git add crates/agenticd/src/server.rs crates/agenticd/src/main.rs
git commit -m "Enforce per-UID rate budget and write-idle deadline in the connection loop (issue #118)"
```

---

### Task 6: CLI flags, startup validation, and the connection gate in the accept loop

**Files:**
- Modify: `crates/agenticd/src/main.rs` (Args struct ~lines 34–100, startup sequence ~lines 250–270, accept loop ~lines 306–368)

**Interfaces:**
- Consumes: `LimitsConfig::validate`, `ConnGate::try_admit`, `ConnRejection` (Tasks 1–2), `DaemonState::with_limits` (Task 4).
- Produces: the six operator-facing flags; startup refusal on invalid limits; per-connection gating. Task 7's integration tests drive these through the real binary.

- [ ] **Step 1: Add the flags to `Args`** (after `approval_key_file`):

```rust
    /// Global cap on concurrently open socket connections. Issue #118.
    #[arg(long, default_value_t = 64)]
    max_connections: usize,

    /// Per-UID cap on concurrently open socket connections. Keys on the
    /// observed SO_PEERCRED UID in both auth modes. Issue #118.
    #[arg(long, default_value_t = 16)]
    max_connections_per_uid: usize,

    /// Per-UID request rate budget in requests/second (burst capacity
    /// is 2x). Exhaustion gets a retryable Concurrency-class reply;
    /// the connection survives. Issue #118.
    #[arg(long, default_value_t = 200)]
    rate_per_uid: u32,

    /// Max requests queued-or-executing on the daemon's commit lock.
    /// Overflow gets a retryable Concurrency-class reply. Issue #118.
    #[arg(long, default_value_t = 8)]
    commit_queue_depth: usize,

    /// Seconds a connection may go without completing an inbound frame
    /// before it is closed. Per-frame clock. Issue #118 (promotes the
    /// previously hardcoded 30s read-idle timeout).
    #[arg(long, default_value_t = 30)]
    read_idle_secs: u64,

    /// Seconds a response write may stall (peer not reading) before the
    /// connection is closed. Issue #118.
    #[arg(long, default_value_t = 30)]
    write_idle_secs: u64,
```

- [ ] **Step 2: Build, validate, and log the config** — in `main()`, right after the peer-auth policy block (before any I/O, same "fail before serving" discipline):

```rust
    // Issue #118: limits are validated before any I/O so a zeroed-out
    // flag aborts startup instead of running a daemon that rejects
    // everything.
    let limits = agenticd::limits::LimitsConfig {
        max_connections: args.max_connections,
        max_connections_per_uid: args.max_connections_per_uid,
        rate_per_uid: args.rate_per_uid,
        commit_queue_depth: args.commit_queue_depth,
        read_idle: std::time::Duration::from_secs(args.read_idle_secs),
        write_idle: std::time::Duration::from_secs(args.write_idle_secs),
    };
    limits.validate().context("validating socket limit flags")?;
    tracing::info!(
        target: "agenticd::limits",
        max_connections = limits.max_connections,
        max_connections_per_uid = limits.max_connections_per_uid,
        rate_per_uid = limits.rate_per_uid,
        commit_queue_depth = limits.commit_queue_depth,
        read_idle_secs = limits.read_idle.as_secs(),
        write_idle_secs = limits.write_idle.as_secs(),
        "socket limits in force"
    );
```

- [ ] **Step 3: Wire the state and the gate.** Chain onto the existing state construction:

```rust
        .with_approval_key(approval_key)
        .with_limits(limits.clone()),
```

Before the `LocalSet` block:

```rust
    let conn_gate = agenticd::limits::ConnGate::new(&limits);
```

- [ ] **Step 4: Gate admissions in the accept loop.** After the allowlist rejection block (`if !state.peer_auth.is_allowed(peer_uid) { ... continue; }`) and before the `tracing::debug!("connection accepted")`, insert:

```rust
                        // Issue #118: connection caps, keyed on the
                        // observed UID. No frame has been read, so a
                        // structured reply is impossible — log-and-close,
                        // same shape as the allowlist rejection above.
                        let conn_guard = match conn_gate.try_admit(peer_uid) {
                            Ok(g) => g,
                            Err(rej) => {
                                use agenticd::limits::ConnRejection;
                                match rej {
                                    ConnRejection::GlobalCap { current, cap } => {
                                        tracing::warn!(
                                            target: "agenticd::limits",
                                            peer_uid,
                                            peer_pid = ?peer_pid,
                                            current,
                                            cap,
                                            "connection rejected: global connection cap"
                                        );
                                    }
                                    ConnRejection::PerUidCap { uid, current, cap } => {
                                        tracing::warn!(
                                            target: "agenticd::limits",
                                            peer_uid = uid,
                                            peer_pid = ?peer_pid,
                                            current,
                                            cap,
                                            "connection rejected: per-UID connection cap"
                                        );
                                    }
                                }
                                drop(sock);
                                continue;
                            }
                        };
```

And move the guard into the connection task so it lives exactly as long as the handler:

```rust
                        tokio::task::spawn_local(async move {
                            let _conn_guard = conn_guard;
                            if let Err(e) =
                                handle_connection(state, sock, peer_uid, carried_uid).await
                            {
```

- [ ] **Step 5: Verify the workspace still builds and all tests pass**

Run: `cargo test -p agenticd --lib && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS / clean. (Flag defaults themselves are proven end-to-end in Task 7.)

- [ ] **Step 6: Commit**

```bash
git add crates/agenticd/src/main.rs
git commit -m "Add socket limit flags and gate connections in the accept loop (issue #118)"
```

---

### Task 7: Integration tests — caps over a real socket, invalid config refuses startup

**Files:**
- Create: `crates/agenticd/tests/socket_limits_integration.rs`

**Interfaces:**
- Consumes: the real `agenticd` binary (`CARGO_BIN_EXE_agenticd`), flags from Task 6.
- Produces: nothing downstream; this is the end-to-end proof.

- [ ] **Step 1: Write the tests**

```rust
//! Integration tests for issue #118 socket availability limits, driven
//! through the real binary over a real Unix socket. Uses
//! --insecure-allow-any-uid so no UID fixtures are needed; limits key
//! on the observed UID regardless of auth mode, and every connection
//! here shares the test process's UID.

use agentic_proto::framing::{read_frame, write_frame};
use agentic_proto::{Envelope, Request, Response};
use std::process::{Child, Command};
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::UnixStream;

fn agenticd_bin() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_agenticd").into()
}

/// Spawn the daemon with `extra_args`, wait for the socket. Panics if
/// the socket never appears.
async fn spawn_daemon(dir: &TempDir, extra_args: &[&str]) -> (Child, std::path::PathBuf) {
    let sock = dir.path().join("agenticd.sock");
    let mut cmd = Command::new(agenticd_bin());
    cmd.arg("--repo")
        .arg(dir.path())
        .arg("--socket")
        .arg(&sock)
        .arg("--insecure-allow-any-uid");
    for a in extra_args {
        cmd.arg(a);
    }
    let child = cmd.spawn().expect("spawn agenticd");
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sock.exists(), "agenticd never created its socket");
    (child, sock)
}

/// Open a connection and prove it is live with a Ping/Pong round trip.
async fn connect_and_ping(sock: &std::path::Path, tag: &str) -> UnixStream {
    let mut conn = UnixStream::connect(sock).await.expect("connect");
    write_frame(&mut conn, &Envelope::new(tag.to_string(), Request::Ping))
        .await
        .expect("write ping");
    let reply: Envelope<Response> = read_frame(&mut conn)
        .await
        .expect("read pong")
        .expect("daemon must reply on a live connection");
    assert!(matches!(reply.payload, Response::Pong));
    conn
}

/// Assert the daemon dropped this connection: the next read hits EOF
/// (or an I/O reset), never a well-formed reply. Same tri-state
/// discrimination as peer_auth_integration.rs.
async fn assert_dropped(sock: &std::path::Path) {
    let outcome = tokio::time::timeout(Duration::from_secs(2), async {
        let mut conn = UnixStream::connect(sock).await?;
        let _ = write_frame(&mut conn, &Envelope::new("dropped", Request::Ping)).await;
        let frame: Option<Envelope<Response>> = match read_frame(&mut conn).await {
            Ok(opt) => opt,
            Err(agentic_proto::framing::FrameError::Io(_)) => None,
            Err(other) => {
                return Err(anyhow::Error::new(other)
                    .context("daemon sent a malformed frame on a capped connection"));
            }
        };
        Ok::<_, anyhow::Error>(frame)
    })
    .await
    .expect("connection attempt timed out — daemon hung instead of dropping")
    .expect("transport-level failure other than a drop");
    assert!(
        outcome.is_none(),
        "daemon replied on a connection past the cap; got {outcome:?}"
    );
}

#[tokio::test]
async fn global_connection_cap_drops_excess_and_spares_existing() {
    let dir = TempDir::new().unwrap();
    let (mut child, sock) = spawn_daemon(&dir, &["--max-connections", "2"]).await;

    let mut c1 = connect_and_ping(&sock, "c1").await;
    let _c2 = connect_and_ping(&sock, "c2").await;

    // Third connection is over the global cap: dropped at accept.
    assert_dropped(&sock).await;

    // Existing connections are unaffected.
    write_frame(&mut c1, &Envelope::new("c1-again", Request::Ping))
        .await
        .expect("c1 write");
    let reply: Envelope<Response> = read_frame(&mut c1)
        .await
        .expect("c1 read")
        .expect("c1 must still be served");
    assert!(matches!(reply.payload, Response::Pong));

    child.kill().ok();
    let _ = child.wait();
}

#[tokio::test]
async fn per_uid_connection_cap_drops_excess() {
    let dir = TempDir::new().unwrap();
    // Global cap far above the per-UID cap so the per-UID path is the
    // one that trips: every connection here shares the test's UID.
    let (mut child, sock) = spawn_daemon(
        &dir,
        &["--max-connections", "64", "--max-connections-per-uid", "1"],
    )
    .await;

    let _c1 = connect_and_ping(&sock, "c1").await;
    assert_dropped(&sock).await;

    child.kill().ok();
    let _ = child.wait();
}

#[tokio::test]
async fn zero_limit_refuses_startup() {
    let dir = TempDir::new().unwrap();
    let sock = dir.path().join("agenticd.sock");
    let mut child = Command::new(agenticd_bin())
        .arg("--repo")
        .arg(dir.path())
        .arg("--socket")
        .arg(&sock)
        .arg("--insecure-allow-any-uid")
        .arg("--max-connections")
        .arg("0")
        .spawn()
        .expect("spawn agenticd");
    // The daemon must exit non-zero without ever creating the socket.
    let mut status = None;
    for _ in 0..100 {
        if let Some(s) = child.try_wait().expect("try_wait") {
            status = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let status = status.unwrap_or_else(|| {
        child.kill().ok();
        panic!("daemon did not exit on an invalid --max-connections 0");
    });
    assert!(!status.success(), "startup must fail loudly on zero limits");
    assert!(
        !sock.exists(),
        "socket must never be created when limits are invalid"
    );
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p agenticd --test socket_limits_integration`
Expected: PASS, 3 tests. (First run builds the binary; allow a minute.)

Note: `anyhow` and `tempfile` are already dev-dependencies of `agenticd` (used by `peer_auth_integration.rs`); no Cargo.toml change. If the compiler flags `anyhow` as missing in the integration-test context, it is a real signal that the plan's no-new-deps constraint was violated somewhere — stop and check, do not add a dependency.

- [ ] **Step 3: Confirm the existing integration tests still pass**

Run: `cargo test -p agenticd --test startup_refusal` (and on Linux: `--test peer_auth_integration`)
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/agenticd/tests/socket_limits_integration.rs
git commit -m "Prove connection caps and startup validation over a real socket (issue #118)"
```

---

### Task 8: Full gates, OpenWolf bookkeeping

**Files:**
- Modify: `.wolf/anatomy.md`, `.wolf/memory.md` (in the MAIN checkout — narrow OpenWolf metadata exception; everything else stays in the worktree)

- [ ] **Step 1: Full workspace gates from inside the worktree**

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p agenticd -p agentic-proto -p agentic-core -p agentic-cli
```

Expected: all clean/PASS. (Postgres-backed suites in `agentic-memory` and `agenticd`'s memory integration tests run in CI; do not block on them locally unless the Docker fixture is already up.)

- [ ] **Step 2: Update OpenWolf indexes** — append to `.wolf/anatomy.md` under the `## crates/agenticd/src/` section pattern used for worktrees: an entry for `limits.rs` (`Socket admission control — ConnGate, RateLimiter, LimitsConfig (issue #118)`) and for `tests/socket_limits_integration.rs`. Append a one-line entry to `.wolf/memory.md`:

```
| HH:MM | Implemented issue #118 socket limits (conn caps, rate budget, commit-queue bound, write-idle) on issue-118-socket-limits worktree | crates/agenticd/src/limits.rs + server.rs + main.rs | all gates green | ~Ntok |
```

- [ ] **Step 3: Verify the branch is coherent**

```bash
git log --oneline main..HEAD
```

Expected: the spec commit plus one commit per task (7 code commits), each message imperative prose referencing issue #118.

---

## Self-review notes (already applied)

- **Spec coverage:** conn caps (T2+T6+T7), rate budget (T3+T5), queue bound + gauge (T4), write-idle (T5), read-idle flag promotion (T5+T6), observability events (T4/T5/T6), config validation (T1+T7), no-new-deps (global constraint), SDK untouched (design property, asserted by rate test checking class/code/retryable only).
- **Recorded deviation from spec:** the `commit_queue_full` warn event carries `peer_uid` + `depth` but not `correlation_id` — `dispatch` never sees the envelope. The client's error reply still carries the correlation id. Accepted 2026-07-10.
- **Type consistency:** `try_admit` takes `u32`; `handle_connection` takes `observed_uid: u32` + `peer_uid: Option<u32>`; `try_commit_slot` takes `Option<u32>` (attestation identity, matching dispatch's `peer_uid`). Rate keys on `observed_uid`, gate keys on observed `peer_uid` in `main.rs` (the raw `cred.uid()` value — same thing).
