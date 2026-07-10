# Issue #124 — Demo Green From Clean State — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `examples/langgraph-rollback/scripts/run-demo.sh` run green from a clean state by fixing the ADR-0013 scanner ↔ LangGraph-checkpointer conflict (ADR-0017 path-scoped entropy exemption), adding SDK socket timeouts, fixing the four portability gaps, and adding two CI jobs so this breakage class can't go invisible again.

**Architecture:** The entropy exemption is decided in 2PC commit staging (`agentic-core/src/commit.rs`), the only layer that knows blob tree-paths; the scanner and object store gain a `ScanPolicy` that skips only the entropy heuristic (pattern rules always run). The daemon threads a configurable prefix list (default `__langgraph__/`) from a CLI flag through `DaemonState` into both the commit and rollback orchestrators (rollback re-stages blobs, so it needs the exemption too). Spec: `docs/superpowers/specs/2026-07-10-issue-124-demo-green-design.md`.

**Tech Stack:** Rust 1.95 workspace (thiserror in libs, anyhow in bins, clap 4, tracing), Python 3.10+ SDK (stdlib `socket`), bash demo scripts, GitHub Actions.

## Global Constraints

- All work happens in the `.worktrees/issue-124-demo-green/` worktree (branch `issue-124-demo-green`). Never touch the main checkout.
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` must pass after every task.
- No `unwrap()` in non-test Rust code without a `// SAFETY:` or `// INVARIANT:` comment.
- Python: `ruff check sdk/python` and `mypy --strict --config-file sdk/python/pyproject.toml sdk/python/agentic` must pass; type-hint everything in `agentic/`.
- Do NOT reorder the 2PC staging steps in `agentic-core/src/commit.rs` (ADR-0002 Decision 3).
- Docs use semantic line breaks. The ADR needs a numeric prefix, a `Status:` line, an owner, and a date (ADR-0001 format).
- Commit messages: plain prose, imperative mood, no conventional-commits prefixes.
- Run all commands from the worktree root `/Users/tonibergholm/Developer/github/git.agentic/.worktrees/issue-124-demo-green` unless a step says otherwise.

**Reference facts** (verified 2026-07-10):
- High-entropy sample that trips the scanner: `b"data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi"` (used by `store.rs::put_raw_rejects_blob_with_high_entropy_run`).
- AWS-pattern sample: `b"hello\nAKIAIOSFODNN7EXAMPLE\nworld"` (pattern name `aws_access_key_id`).
- `ObjectStore` implementors: `FsObjectStore` (`agentic-core/src/store.rs:105`), `GcsObjectStore` (`agentic-core/src/gcs_store.rs:349`), test double `SlowStore` (`agenticd/src/store_async.rs:122`).
- `CommitInputs` struct literals that will need the new field: `agentic-core/src/commit.rs` (struct + tests), `agentic-core/src/diff.rs` (tests), `agentic-core/benches/store.rs`, `agenticd/src/commit.rs::assemble_inputs`, `agenticd/src/rollback/mod.rs:402`. Let `cargo check` list them (E0063).
- `agentic-core` already depends on `tracing` (workspace dep, `crates/agentic-core/Cargo.toml:22`).

---

### Task 1: `ScanPolicy` and `Scanner::scan_with`

**Files:**
- Modify: `crates/agentic-core/src/scanner.rs`

**Interfaces:**
- Produces: `pub struct ScanPolicy { pub skip_entropy: bool }` (derives `Debug, Clone, Copy, Default, PartialEq, Eq`) and `pub fn scan_with(&self, bytes: &[u8], policy: ScanPolicy) -> Vec<Hit>` on `Scanner`. Existing `pub fn scan(&self, bytes: &[u8]) -> Vec<Hit>` behavior unchanged.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `crates/agentic-core/src/scanner.rs`:

```rust
#[test]
fn scan_with_skip_entropy_suppresses_only_entropy_hits() {
    let s = Scanner::new();
    // Contains BOTH a pattern hit (AWS key) and a high-entropy base64 run.
    let blob = b"AKIAIOSFODNN7EXAMPLE data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi";

    let full = s.scan(blob);
    assert!(
        full.iter().any(|h| h.kind == HitKind::HighEntropy),
        "sanity: full scan must contain an entropy hit; got {full:?}"
    );
    assert!(
        full.iter()
            .any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "aws_access_key_id")),
        "sanity: full scan must contain the AWS pattern hit; got {full:?}"
    );

    let skipped = s.scan_with(blob, ScanPolicy { skip_entropy: true });
    assert!(
        skipped.iter().all(|h| h.kind != HitKind::HighEntropy),
        "skip_entropy must suppress every entropy hit; got {skipped:?}"
    );
    assert!(
        skipped
            .iter()
            .any(|h| matches!(&h.kind, HitKind::Pattern(n) if n == "aws_access_key_id")),
        "pattern rules must still run under skip_entropy; got {skipped:?}"
    );
}

#[test]
fn scan_with_default_policy_equals_scan() {
    let s = Scanner::new();
    let blob = b"AKIAIOSFODNN7EXAMPLE data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi";
    assert_eq!(s.scan(blob), s.scan_with(blob, ScanPolicy::default()));
}
```

Note: `assert_eq!` on `Vec<Hit>` requires `Hit: PartialEq + Debug` — both already derived (the store tests compare `h.kind` with `==`). If `Hit` lacks `PartialEq`, add it to its derive list.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p agentic-core scan_with -- --nocapture`
Expected: FAIL to compile with "cannot find type `ScanPolicy`" / "no method named `scan_with`".

- [ ] **Step 3: Implement `ScanPolicy` and `scan_with`**

In `crates/agentic-core/src/scanner.rs`, near the top (after the constants):

```rust
/// Per-call scanning policy (ADR-0017).
///
/// `skip_entropy` disables ONLY the Shannon-entropy heuristic — used by
/// commit staging for declared checkpoint-path blobs, where encoded
/// payloads make entropy hits 100% false positives. Pattern rules always
/// run regardless of policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanPolicy {
    pub skip_entropy: bool,
}
```

Change `Scanner::scan` to delegate, and add `scan_with` that runs the existing scan logic then filters entropy hits when the policy says so (post-filtering is deliberately chosen over threading the flag into `maybe_emit_entropy_hit`: it is provably equivalent and keeps the hot path single-shaped; the wasted entropy computation on exempt blobs is negligible at demo blob sizes):

```rust
pub fn scan(&self, bytes: &[u8]) -> Vec<Hit> {
    self.scan_with(bytes, ScanPolicy::default())
}

pub fn scan_with(&self, bytes: &[u8], policy: ScanPolicy) -> Vec<Hit> {
    let mut hits = self.scan_impl(bytes); // rename the existing scan body to scan_impl
    if policy.skip_entropy {
        hits.retain(|h| h.kind != HitKind::HighEntropy);
    }
    hits
}
```

Mechanically: rename the current `pub fn scan(&self, bytes: &[u8]) -> Vec<Hit>` body to a private `fn scan_impl(&self, bytes: &[u8]) -> Vec<Hit>`, then add the two public methods above.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p agentic-core --lib scanner -- --nocapture`
Expected: PASS, including all pre-existing scanner tests.

- [ ] **Step 5: Commit**

```bash
git add crates/agentic-core/src/scanner.rs
git commit -m "Add ScanPolicy with an entropy-skip mode to the secret scanner (ADR-0017 groundwork)"
```

---

### Task 2: `ObjectStore::put_with_policy`

**Files:**
- Modify: `crates/agentic-core/src/store.rs`
- Modify: `crates/agentic-core/src/gcs_store.rs`
- Modify: `crates/agenticd/src/store_async.rs` (test double `SlowStore`)

**Interfaces:**
- Consumes: `ScanPolicy` from Task 1.
- Produces: trait method `fn put_with_policy(&self, object: &Object, policy: ScanPolicy) -> Result<Hash>` (required), with `fn put(&self, object: &Object) -> Result<Hash>` becoming a provided default that delegates with `ScanPolicy::default()`. Every existing `store.put(...)` call site keeps compiling unchanged.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/agentic-core/src/store.rs`:

```rust
#[test]
fn put_with_policy_skip_entropy_accepts_high_entropy_blob() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsObjectStore::open(dir.path()).unwrap();
    let bytes = b"data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi".to_vec();
    let obj = Object::Blob(Blob::new(bytes));
    let hash = store
        .put_with_policy(&obj, crate::scanner::ScanPolicy { skip_entropy: true })
        .expect("entropy-exempt blob must be accepted");
    assert!(store.has(&hash));
}

#[test]
fn put_with_policy_skip_entropy_still_rejects_pattern_hits() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsObjectStore::open(dir.path()).unwrap();
    let bytes = b"hello\nAKIAIOSFODNN7EXAMPLE\nworld".to_vec();
    let obj = Object::Blob(Blob::new(bytes));
    match store.put_with_policy(&obj, crate::scanner::ScanPolicy { skip_entropy: true }) {
        Err(Error::SecretDetected { hits }) => {
            assert!(hits.iter().any(|h| matches!(
                &h.kind,
                crate::scanner::HitKind::Pattern(n) if n == "aws_access_key_id"
            )));
        }
        other => panic!("expected SecretDetected, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p agentic-core --lib store -- --nocapture`
Expected: FAIL to compile with "no method named `put_with_policy`".

- [ ] **Step 3: Implement the trait change**

In `crates/agentic-core/src/store.rs`:

1. Import: `use crate::scanner::{Allowlist, ScanPolicy, Scanner};`
2. Change the trait (`put` becomes a provided method; `put_with_policy` is the required one):

```rust
pub trait ObjectStore: Send + Sync {
    /// `put` with a per-call scan policy (ADR-0017). Policy only affects
    /// `Object::Blob` — Trees and Commits are never scanned.
    fn put_with_policy(&self, object: &Object, policy: ScanPolicy) -> Result<Hash>;

    fn put(&self, object: &Object) -> Result<Hash> {
        self.put_with_policy(object, ScanPolicy::default())
    }

    fn put_raw(&self, kind: ObjectKind, bytes: &[u8]) -> Result<Hash>;
    fn get(&self, hash: &Hash) -> Result<Object>;
    fn get_raw(&self, hash: &Hash) -> Result<Vec<u8>>;
    fn has(&self, hash: &Hash) -> bool;
}
```

3. In `impl ObjectStore for FsObjectStore`, rename `fn put` to `fn put_with_policy(&self, object: &Object, policy: ScanPolicy)` and change the scan line from `self.scanner.scan(&blob.bytes)` to `self.scanner.scan_with(&blob.bytes, policy)`. Delete the local `put` implementation (the trait default now provides it). Everything else in the method body stays byte-identical.

4. In `crates/agentic-core/src/gcs_store.rs`, apply the same mechanical change to `impl ObjectStore for GcsObjectStore` (line ~349): rename `put` → `put_with_policy(&self, object: &Object, policy: ScanPolicy)`, scan via `scan_with(&blob.bytes, policy)` (the scan is at line ~357), add the `ScanPolicy` import.

5. In `crates/agenticd/src/store_async.rs`, the test double `SlowStore` (line ~122) implements `put`; rename that impl to `put_with_policy(&self, object: &Object, _policy: agentic_core::scanner::ScanPolicy)` with the body unchanged.

- [ ] **Step 4: Run the workspace tests**

Run: `cargo test --workspace --all-targets`
Expected: PASS — all pre-existing store/scanner/staging tests keep passing because `put` now routes through `put_with_policy(ScanPolicy::default())`, which is behavior-identical.

- [ ] **Step 5: Commit**

```bash
git add crates/agentic-core/src/store.rs crates/agentic-core/src/gcs_store.rs crates/agenticd/src/store_async.rs
git commit -m "Route ObjectStore::put through a policy-aware put_with_policy on both backends"
```

---

### Task 3: Staging-level exemption in `stage_blob_tree`

**Files:**
- Modify: `crates/agentic-core/src/commit.rs`
- Modify (compile fixes, `exempt_entropy_prefixes: Vec::new(),` in `CommitInputs` literals): `crates/agentic-core/src/diff.rs`, `crates/agentic-core/benches/store.rs`, `crates/agenticd/src/commit.rs`, `crates/agenticd/src/rollback/mod.rs` (the real values for the last two land in Task 4)

**Interfaces:**
- Consumes: `put_with_policy` from Task 2.
- Produces: `CommitInputs` gains `pub exempt_entropy_prefixes: Vec<String>`; `stage_blob_tree` gains an `exempt_prefixes: &[String]` parameter. A tree-entry name matching `name.starts_with(prefix)` for any prefix is staged with `ScanPolicy { skip_entropy: true }` and emits a tracing event. Applies to BOTH the prompts and tools trees (paths are opaque; uniform treatment).

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `crates/agentic-core/src/commit.rs`:

```rust
const HIGH_ENTROPY_CHECKPOINT: &[u8] = b"data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi";

#[test]
fn staging_exempts_entropy_for_matching_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let agentic = dir.path().join(".agentic");
    let store = FsObjectStore::open(agentic.join("objects")).unwrap();
    let refs = Refs::open(&agentic).unwrap();

    let mut inputs = fresh_inputs("checkpoint commit");
    inputs.prompts.insert(
        "__langgraph__/abc123/checkpoint.json".to_string(),
        HIGH_ENTROPY_CHECKPOINT.to_vec(),
    );
    inputs.exempt_entropy_prefixes = vec!["__langgraph__/".to_string()];

    stage_and_commit(&store, &refs, "main", inputs)
        .expect("high-entropy blob under an exempt prefix must commit");
}

#[test]
fn staging_rejects_entropy_outside_exempt_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let agentic = dir.path().join(".agentic");
    let store = FsObjectStore::open(agentic.join("objects")).unwrap();
    let refs = Refs::open(&agentic).unwrap();

    let mut inputs = fresh_inputs("not a checkpoint");
    inputs.prompts.insert(
        "notes.txt".to_string(),
        HIGH_ENTROPY_CHECKPOINT.to_vec(),
    );
    inputs.exempt_entropy_prefixes = vec!["__langgraph__/".to_string()];

    match stage_and_commit(&store, &refs, "main", inputs) {
        Err(Error::SecretDetected { .. }) => {}
        other => panic!("expected SecretDetected outside the exempt prefix, got {other:?}"),
    }
}

#[test]
fn staging_rejects_pattern_hits_even_under_exempt_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let agentic = dir.path().join(".agentic");
    let store = FsObjectStore::open(agentic.join("objects")).unwrap();
    let refs = Refs::open(&agentic).unwrap();

    let mut inputs = fresh_inputs("checkpoint with a real secret");
    inputs.prompts.insert(
        "__langgraph__/abc123/checkpoint.json".to_string(),
        b"hello\nAKIAIOSFODNN7EXAMPLE\nworld".to_vec(),
    );
    inputs.exempt_entropy_prefixes = vec!["__langgraph__/".to_string()];

    match stage_and_commit(&store, &refs, "main", inputs) {
        Err(Error::SecretDetected { hits }) => {
            assert!(hits.iter().any(|h| matches!(
                &h.kind,
                crate::scanner::HitKind::Pattern(n) if n == "aws_access_key_id"
            )));
        }
        other => panic!("expected SecretDetected for the AWS pattern, got {other:?}"),
    }
}
```

Note: `fresh_inputs` builds a full `CommitInputs` literal — Step 3 adds the new field there (`exempt_entropy_prefixes: Vec::new(),`).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p agentic-core --lib commit -- --nocapture`
Expected: FAIL to compile — `CommitInputs` has no field `exempt_entropy_prefixes`.

- [ ] **Step 3: Implement the field and the policy decision**

In `crates/agentic-core/src/commit.rs`:

1. Add to `CommitInputs` (after `peer_uid`):

```rust
/// Prompt/tool tree-path prefixes whose blobs skip the scanner's
/// entropy heuristic (ADR-0017). Pattern rules still run. Populated
/// by the daemon from `--scanner-exempt-entropy-prefix`; empty means
/// full scanning for every blob.
pub exempt_entropy_prefixes: Vec<String>,
```

2. Thread it into both tree-staging calls in `stage_and_commit_with_now`:

```rust
let prompts_hash = stage_blob_tree(store, &inputs.prompts, &inputs.exempt_entropy_prefixes)?;
let tools_hash = stage_blob_tree(store, &inputs.tools, &inputs.exempt_entropy_prefixes)?;
```

3. Change `stage_blob_tree`:

```rust
use crate::scanner::ScanPolicy;

fn stage_blob_tree<S: ObjectStore + ?Sized>(
    store: &S,
    entries: &BTreeMap<String, Vec<u8>>,
    exempt_prefixes: &[String],
) -> Result<Option<Hash>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut tree = Tree::new();
    for (name, bytes) in entries {
        let policy = if exempt_prefixes.iter().any(|p| name.starts_with(p.as_str())) {
            tracing::info!(
                target: "agentic_core::scanner",
                path = %name,
                "entropy heuristic exempted for declared checkpoint path (ADR-0017)"
            );
            ScanPolicy { skip_entropy: true }
        } else {
            ScanPolicy::default()
        };
        let blob_hash = store.put_with_policy(&Object::Blob(Blob::new(bytes.clone())), policy)?;
        tree.insert(
            name.clone(),
            TypedRef {
                kind: ObjectKind::Blob,
                hash: blob_hash,
            },
        );
    }
    let tree_hash = store.put(&Object::Tree(tree))?;
    Ok(Some(tree_hash))
}
```

4. Fix the two existing `stage_blob_tree(&store, ...)` calls in this file's tests (`idempotent_blob_tree`) by passing `&[]` as the third argument.

5. Run `cargo check --workspace --all-targets` and add `exempt_entropy_prefixes: Vec::new(),` to every `CommitInputs` literal the compiler reports (E0063): the test literals in this file, `crates/agentic-core/src/diff.rs`, `crates/agentic-core/benches/store.rs`, `crates/agenticd/src/commit.rs::assemble_inputs`, `crates/agenticd/src/rollback/mod.rs:402`. (The daemon files get the real value in Task 4 — `Vec::new()` here keeps the workspace compiling between commits.)

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --workspace --all-targets && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A crates/
git commit -m "Skip the entropy heuristic in 2PC staging for exempt checkpoint-path prefixes"
```

---

### Task 4: Daemon flag, `DaemonState` plumbing, commit + rollback threading

**Files:**
- Modify: `crates/agenticd/src/main.rs`
- Modify: `crates/agenticd/src/server.rs`
- Modify: `crates/agenticd/src/commit.rs`
- Modify: `crates/agenticd/src/rollback/mod.rs`
- Test: `crates/agenticd/src/commit.rs` (unit tests), `crates/agenticd/tests/rollback_in_memory.rs`

**Interfaces:**
- Consumes: `CommitInputs.exempt_entropy_prefixes` from Task 3.
- Produces: `DaemonState` field `pub exempt_entropy_prefixes: Vec<String>` (default empty in `DaemonState::open`); builder `pub fn with_exempt_entropy_prefixes(mut self, prefixes: Vec<String>) -> Self`; CLI flag `--scanner-exempt-entropy-prefix` (repeatable, default `__langgraph__/`), validated at startup.

- [ ] **Step 1: Write the failing daemon-level tests**

Add to `mod tests` in `crates/agenticd/src/commit.rs` (alongside `make_state`):

```rust
const HIGH_ENTROPY_CHECKPOINT: &[u8] = b"data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi";

async fn make_state_with_prefixes(
    repo: &std::path::Path,
    prefixes: Vec<String>,
) -> Arc<DaemonState> {
    let agentic_dir = repo.join(".agentic");
    std::fs::create_dir_all(&agentic_dir).unwrap();
    let store: Arc<dyn ObjectStore + Send + Sync> =
        Arc::new(FsObjectStore::open(agentic_dir.join("objects")).unwrap());
    Arc::new(
        DaemonState::open(
            repo.to_path_buf(),
            agentic_dir,
            store,
            None,       // no postgres
            Vec::new(), // no tracked tables
            Vec::new(), // no MCP servers
            Arc::new(crate::peer_auth::PeerAuthPolicy::InsecureAllowAny),
        )
        .await
        .unwrap()
        .with_exempt_entropy_prefixes(prefixes),
    )
}

#[tokio::test]
async fn execute_exempts_entropy_under_configured_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let state =
        make_state_with_prefixes(dir.path(), vec!["__langgraph__/".to_string()]).await;
    let mut input = commit_input("langgraph checkpoint");
    input.prompts.insert(
        "__langgraph__/abc123/checkpoint.json".to_string(),
        HIGH_ENTROPY_CHECKPOINT.to_vec(),
    );
    execute(state, input, None)
        .await
        .expect("high-entropy blob under the configured exempt prefix must commit");
}

#[tokio::test]
async fn execute_rejects_entropy_outside_configured_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let state =
        make_state_with_prefixes(dir.path(), vec!["__langgraph__/".to_string()]).await;
    let mut input = commit_input("hostile prompt");
    input
        .prompts
        .insert("notes.txt".to_string(), HIGH_ENTROPY_CHECKPOINT.to_vec());
    let err = execute(state, input, None)
        .await
        .expect_err("entropy hit outside the exempt prefix must still reject");
    assert!(
        err.chain().any(|e| matches!(
            e.downcast_ref::<agentic_core::Error>(),
            Some(agentic_core::Error::SecretDetected { .. })
        )),
        "rejection must carry the typed SecretDetected; got: {err:#}"
    );
}
```

Add to `crates/agenticd/tests/rollback_in_memory.rs` (rollback re-stages prompt blobs via `read_text_blobs` → `stage_and_commit`, so an unexempted rollback across a checkpoint commit would fail — this is demo step 10):

```rust
// ADR-0017: rollback forward-records by re-staging the target commit's
// prompt blobs. A baseline containing a high-entropy checkpoint blob
// under an exempt prefix must therefore roll back cleanly — without the
// exemption threading, the scanner rejects the rollback commit itself.
#[tokio::test]
async fn rollback_restages_exempt_checkpoint_blob() {
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let agentic_dir = repo_root.join(".agentic");
    std::fs::create_dir_all(agentic_dir.join("objects")).unwrap();

    let store: Arc<dyn ObjectStore + Send + Sync> =
        Arc::new(FsObjectStore::open(agentic_dir.join("objects")).unwrap());
    let refs = Refs::open(&agentic_dir).unwrap();

    let state = Arc::new(DaemonState {
        repo_root: repo_root.clone(),
        store: Arc::clone(&store),
        refs,
        commit_lock: Arc::new(Mutex::new(())),
        shutdown: tokio_util::sync::CancellationToken::new(),
        memory: None,
        mcp_servers: Vec::new(),
        http: reqwest::Client::builder()
            .user_agent("agenticd-test")
            .build()
            .unwrap(),
        peer_auth: Arc::new(agenticd::peer_auth::PeerAuthPolicy::InsecureAllowAny),
        approval_key: None,
        limits: agenticd::limits::LimitsConfig::default(),
        rate: agenticd::limits::RateLimiter::new(
            agenticd::limits::LimitsConfig::default().rate_per_uid,
        ),
        commit_slots: Arc::new(tokio::sync::Semaphore::new(
            agenticd::limits::LimitsConfig::default().commit_queue_depth,
        )),
        commit_queue_depth: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        exempt_entropy_prefixes: vec!["__langgraph__/".to_string()],
    });

    // Baseline commit carrying a high-entropy checkpoint blob.
    let mut baseline_input = commit_input_no_memory("baseline with checkpoint");
    baseline_input.prompts.insert(
        "__langgraph__/abc123/checkpoint.json".to_string(),
        b"data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi".to_vec(),
    );
    let baseline = commit::execute(Arc::clone(&state), baseline_input, None)
        .await
        .expect("baseline with exempt checkpoint blob must commit");

    // Second commit so rollback has somewhere to come back from.
    commit::execute(
        Arc::clone(&state),
        commit_input_no_memory("second"),
        None,
    )
    .await
    .expect("second commit");

    // Roll back to the baseline: re-stages the checkpoint blob.
    let out = rollback::execute(
        Arc::clone(&state),
        RollbackArgs {
            target: baseline.commit_hash.clone(),
            dry_run: false,
            accept_data_loss: false,
            approval_token: None,
            repo: repo_root,
        },
        None,
    )
    .await
    .expect("rollback across an exempt checkpoint blob must succeed");
    assert!(out.executed);
}

fn commit_input_no_memory(message: &str) -> CommitInput {
    let mut input = commit_input_with_memory(message);
    input.no_memory = true;
    input
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p agenticd -- --nocapture 2>&1 | head -40`
Expected: FAIL to compile — `DaemonState` has no field/method `exempt_entropy_prefixes` / `with_exempt_entropy_prefixes`.

- [ ] **Step 3: Implement the daemon plumbing**

1. `crates/agenticd/src/server.rs` — add the field to `DaemonState` (after `commit_queue_depth`):

```rust
/// Prompt/tool tree-path prefixes whose blobs skip the scanner's
/// entropy heuristic (ADR-0017). From --scanner-exempt-entropy-prefix.
pub exempt_entropy_prefixes: Vec<String>,
```

Initialize it as `exempt_entropy_prefixes: Vec::new(),` in `DaemonState::open`'s construction, and add the builder next to `with_limits`:

```rust
/// Attach the ADR-0017 entropy-exempt path prefixes. Builder-style
/// like `with_limits`; call before serving traffic.
pub fn with_exempt_entropy_prefixes(mut self, prefixes: Vec<String>) -> Self {
    self.exempt_entropy_prefixes = prefixes;
    self
}
```

2. `crates/agenticd/src/main.rs` — add the flag to the args struct (after `scanner_allowlist`):

```rust
/// Prompt-tree path prefix whose blobs skip the scanner's entropy
/// heuristic (pattern rules still run). ADR-0017. Repeatable.
/// Default covers the LangGraph checkpointer's blob namespace.
#[arg(long = "scanner-exempt-entropy-prefix", default_values_t = vec!["__langgraph__/".to_string()])]
scanner_exempt_entropy_prefixes: Vec<String>,
```

Validate at startup (place next to the scanner-allowlist load, before `DaemonState::open`):

```rust
// ADR-0017: exempt prefixes are relative prompt-tree paths.
for p in &args.scanner_exempt_entropy_prefixes {
    anyhow::ensure!(
        !p.trim().is_empty(),
        "--scanner-exempt-entropy-prefix must not be empty"
    );
    anyhow::ensure!(
        !p.starts_with('/'),
        "--scanner-exempt-entropy-prefix {p:?} must be a relative prompt-tree prefix (no leading '/')"
    );
}
```

Chain the builder where `DaemonState::open(...)` is called (main.rs:310), after `.with_limits(limits.clone())`:

```rust
.with_exempt_entropy_prefixes(args.scanner_exempt_entropy_prefixes.clone()),
```

3. `crates/agenticd/src/commit.rs` — pass the state's prefixes through `assemble_inputs`. Change the signature and the literal:

```rust
fn assemble_inputs(
    input: CommitInput,
    parent: Option<agentic_core::Hash>,
    memory_snapshot: Option<agentic_core::Hash>,
    schema_version: Option<String>,
    tools: BTreeMap<String, Vec<u8>>,
    peer_uid: Option<u32>,
    exempt_entropy_prefixes: Vec<String>,
) -> CommitInputs {
```

with `exempt_entropy_prefixes,` in the returned literal (replacing Task 3's `Vec::new()`), and at the call site in `execute_with_now`:

```rust
let inputs = assemble_inputs(
    input,
    parent,
    memory_snapshot,
    schema_version,
    tools,
    peer_uid,
    state.exempt_entropy_prefixes.clone(),
);
```

4. `crates/agenticd/src/rollback/mod.rs` — in the forward-record `CommitInputs` literal (line ~402), replace Task 3's `exempt_entropy_prefixes: Vec::new(),` with:

```rust
exempt_entropy_prefixes: state.exempt_entropy_prefixes.clone(),
```

5. Fix the `DaemonState` struct literal in `crates/agenticd/tests/rollback_in_memory.rs`'s existing test (add `exempt_entropy_prefixes: Vec::new(),`) and any other struct-literal constructions `cargo check` reports.

- [ ] **Step 4: Run the full gate**

Run: `cargo test --workspace --all-targets && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all -- --check`
Expected: PASS, including the three new tests.

- [ ] **Step 5: Commit**

```bash
git add crates/agenticd/
git commit -m "Thread ADR-0017 entropy-exempt prefixes from a daemon flag into commit and rollback staging"
```

---

### Task 5: ADR-0017 document

**Files:**
- Create: `docs/adr/0017-entropy-exemption-for-checkpoint-paths.md`

**Interfaces:** none (documentation). If another ADR claims 0017 before this lands, renumber to the next free slot and update the references in Tasks 1–4's doc comments plus the spec.

- [ ] **Step 1: Write the ADR**

Create `docs/adr/0017-entropy-exemption-for-checkpoint-paths.md` with exactly this content (semantic line breaks):

```markdown
# ADR-0017: Entropy-Exemption for Declared Checkpoint Paths

Status: Accepted
Owner: Toni Bergholm
Date: 2026-07-10
Amends: ADR-0013 (secret scanner)

## Context

The ADR-0013 secret scanner runs a pattern detector and a Shannon-entropy
heuristic (threshold 4.5 bits over base64-alphabet runs ≥ 20 bytes) on every
Blob before it touches the object store.
The LangGraph checkpointer (`sdk/python/agentic/langgraph.py`) serialises
checkpoints as msgpack and base64-wraps them into a JSON envelope stored
under the commit's prompts tree at `__langgraph__/<thread-hash>/checkpoint.json`.
Base64 payloads sit near 6 bits of entropy, so the heuristic fires on every
checkpoint commit — the broken-prompt demo has been unable to commit a single
LangGraph step since the scanner landed on 2026-05-21 (issue #124 gap 5).

ADR-0013 Decision 4's blob-hash allowlist cannot address this:
checkpoint content changes every run, so there is no stable hash to allowlist.
The scanner runs inside the object store's `put`/`put_raw`, which has no path
context — blob paths live in the Tree object, known only to commit staging.

## Decision 1: skip only the entropy heuristic, only for declared prefixes

The entropy heuristic yields 100 % false positives on encoded checkpoint
payloads, so it carries zero signal for them.
Commit staging (`agentic-core::commit::stage_blob_tree`) — the layer that
knows each blob's tree path — stages blobs whose path matches a configured
exempt prefix with `ScanPolicy { skip_entropy: true }`.
Pattern rules (AWS keys, PEM blocks, and the rest of
`scanner_patterns.rs`) still run on every blob, exempt or not.
The exemption applies uniformly to the prompts and tools trees; paths are
opaque strings and get no framework-specific meaning in the daemon.

## Decision 2: configuration and default

The prefix list is daemon configuration:
`--scanner-exempt-entropy-prefix` (repeatable), defaulting to
`__langgraph__/`, following the `--scanner-allowlist` precedent.
Prefixes must be non-empty relative paths; the daemon refuses to start
otherwise.
An empty list (passing the flag zero times is not possible once a default
exists; operators can pass a never-matching prefix to disable) — rather,
operators who want full scanning everywhere run with
`--scanner-exempt-entropy-prefix "__disabled__/"`.

## Decision 3: observability

Every applied exemption emits a structured tracing event
(`target: "agentic_core::scanner"`, level INFO, with the blob path),
keeping the daemon's tracing-only observability discipline (issue #118).

## Consequences and accepted risk

A secret embedded inside serialised agent state under an exempt prefix is no
longer entropy-caught (pattern rules still apply).
Accepted because the scanner is a guardrail against accidental secret
commits by a trusted, peer-authenticated client (ADR-0012), not an
adversarial control — a hostile same-UID client could already bypass it by
naming any path under the exempt prefix, or by not using the daemon at all.
The SDK contract does not change: no wire changes, no framework-specific
Commit fields (ADR-0003 Decision 3 holds).
Rollback forward-recording re-stages target-commit blobs and therefore
applies the same exemption; without it, rolling back across a checkpoint
commit would be rejected by the scanner.
```

- [ ] **Step 2: Self-check the ADR**

Run: `grep -E 'Status:|Owner:|Date:' docs/adr/0017-entropy-exemption-for-checkpoint-paths.md`
Expected: all three lines present.

- [ ] **Step 3: Commit**

```bash
git add docs/adr/0017-entropy-exemption-for-checkpoint-paths.md
git commit -m "Add ADR-0017: entropy-exemption for declared checkpoint paths"
```

---

### Task 6: SDK socket timeouts

**Files:**
- Modify: `sdk/python/agentic/client.py`
- Test: `sdk/python/tests/test_client.py`

**Interfaces:**
- Produces: `AgenticClient.__init__(self, socket_path=DEFAULT_SOCKET_PATH, *, connect_timeout: float = 5.0, request_timeout: float = 30.0)`. Timeout expiry raises `AgenticProtocolError` with `code="timeout"`, `retryable=True`.

- [ ] **Step 1: Write the failing test**

Add to `sdk/python/tests/test_client.py` (uses the existing `short_tmp` fixture and `_spawn_mock_daemon` helper; add `import time` to the file's imports if absent):

```python
def test_request_timeout_raises_retryable_protocol_error(short_tmp: Path):
    """A daemon that accepts but never replies must fail loudly within the
    configured deadline instead of hanging forever (issue #124)."""
    sock_path = short_tmp / "stall.sock"

    def handler(conn: socket.socket) -> None:
        conn.recv(4)  # swallow the frame header, never reply
        time.sleep(1.0)

    _spawn_mock_daemon(sock_path, handler)
    client = AgenticClient(socket_path=sock_path, request_timeout=0.2)

    start = time.monotonic()
    with pytest.raises(AgenticProtocolError) as excinfo:
        client.ping()
    elapsed = time.monotonic() - start

    assert excinfo.value.code == "timeout"
    assert excinfo.value.retryable is True
    assert elapsed < 0.9, f"must fail within the deadline, took {elapsed:.2f}s"
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `sdk/python/.venv/bin/python -m pytest sdk/python/tests/test_client.py::test_request_timeout_raises_retryable_protocol_error -v` — or, if no venv exists yet in the worktree, `python3 -m venv sdk/python/.venv && sdk/python/.venv/bin/pip install -e "sdk/python[langgraph,dev]"` first.
Expected: FAIL with `TypeError: __init__() got an unexpected keyword argument 'request_timeout'`.

- [ ] **Step 3: Implement the timeouts**

In `sdk/python/agentic/client.py`:

1. Replace `__init__`:

```python
def __init__(
    self,
    socket_path: Path | str = DEFAULT_SOCKET_PATH,
    *,
    connect_timeout: float = 5.0,
    request_timeout: float = 30.0,
) -> None:
    self.socket_path = Path(socket_path)
    self.connect_timeout = connect_timeout
    self.request_timeout = request_timeout
```

2. In `_request`, set the timeouts around the connect (currently `sock.connect(...)` at line ~327):

```python
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
    sock.settimeout(self.connect_timeout)
    sock.connect(str(self.socket_path))
    sock.settimeout(self.request_timeout)
    write_frame(sock, envelope)
    reply = read_frame(sock)
```

3. Add a `TimeoutError` arm to the `except` chain — it MUST come before the `OSError` arm (`TimeoutError` is a subclass of `OSError`, and `socket.timeout` is an alias of `TimeoutError` since Python 3.10):

```python
except TimeoutError as e:
    raise AgenticProtocolError(
        f"daemon did not respond within {self.request_timeout}s at "
        f"{self.socket_path}; it may be stalled — see its log",
        code="timeout",
        retryable=True,
        class_token="protocol",
    ) from e
```

- [ ] **Step 4: Run the Python gate**

Run:
```bash
sdk/python/.venv/bin/python -m pytest sdk/python/tests -q
sdk/python/.venv/bin/python -m ruff check sdk/python
sdk/python/.venv/bin/python -m mypy --strict --config-file sdk/python/pyproject.toml sdk/python/agentic
```
Expected: all PASS. (Install `ruff`/`mypy` into the venv via the `[dev]` extra if missing.)

- [ ] **Step 5: Commit**

```bash
git add sdk/python/agentic/client.py sdk/python/tests/test_client.py
git commit -m "Give the SDK client connect and request socket timeouts that fail loudly"
```

---

### Task 7: Demo script fixes (gaps 2, 3, 4)

**Files:**
- Modify: `examples/langgraph-rollback/scripts/run-demo.sh`
- Modify: `examples/langgraph-rollback/scripts/ask.sh`
- Modify: `examples/langgraph-rollback/.gitignore`

**Interfaces:**
- Produces: `run-demo.sh` exports `AGENTIC_SOCKET` (short `/tmp` path) and `PYTHON` (venv interpreter); `ask.sh` honors `${PYTHON:-python3}`.

- [ ] **Step 1: Edit `run-demo.sh`**

Apply these changes (line references are to the current file):

1. Replace line 32 (`export AGENTIC_SOCKET="${DEMO_DIR}/.agentic/agenticd.sock"`) with a short socket path — macOS `SUN_LEN` caps Unix socket paths at ~104 bytes, so a worktree-nested `DEMO_DIR` overflows it:

```bash
# Unix socket paths are capped at ~104 bytes on macOS (SUN_LEN); a
# checkout nested under .worktrees/ overflows that. Bind under /tmp.
SOCKET_DIR="$(mktemp -d /tmp/agentic-demo.XXXXXX)"
export AGENTIC_SOCKET="${SOCKET_DIR}/agenticd.sock"
```

2. In `cleanup()` (line ~51), after the `compose ... down` line add:

```bash
    rm -rf "${SOCKET_DIR}"
```

3. After the `compose()`/`container_run()` definitions (line ~48), add the venv bootstrap as step 0:

```bash
step "0. python environment (venv + SDK deps)"
VENV_DIR="${DEMO_DIR}/.venv"
if ! "${VENV_DIR}/bin/python" -c "import agentic, langgraph, psycopg" >/dev/null 2>&1; then
    echo "creating ${VENV_DIR} and installing the agentic SDK (+langgraph, psycopg)..."
    python3 -m venv "${VENV_DIR}"
    "${VENV_DIR}/bin/pip" install --quiet --upgrade pip
    "${VENV_DIR}/bin/pip" install --quiet -e "${REPO_ROOT}/sdk/python[langgraph]" "psycopg[binary]>=3.1" \
        || { echo "error: pip install of demo deps failed; retry with: ${VENV_DIR}/bin/pip install -e '${REPO_ROOT}/sdk/python[langgraph]' 'psycopg[binary]>=3.1'" >&2; exit 1; }
fi
export PYTHON="${VENV_DIR}/bin/python"
```

(`step` is defined at line 59 — move the `step()` definition above this block, next to `compose()`.)

4. In step 2 (seeding, line ~78), after the `git checkout -- ...system.txt` restore line add:

```bash
# Stale checkpoint envelopes from prior (possibly failed) runs would be
# swept into the baseline commit by read_prompt_dir; start clean.
rm -rf "${DEMO_DIR}/prompts/__langgraph__"
```

5. In step 3 (daemon start, line ~87), pass the socket explicitly — add `--socket "${AGENTIC_SOCKET}"` to the `agenticd` invocation:

```bash
"${AGENTICD_BIN}" --repo "${DEMO_DIR}" --postgres "${DATABASE_URL}" --tables episodes:id \
    --socket "${AGENTIC_SOCKET}" \
    --insecure-allow-any-uid \
    > "${DEMO_DIR}/.agentic/daemon.log" 2>&1 &
```

(The `rm -rf "${DEMO_DIR}/.agentic"` on line 85 runs before the daemon writes its log there; `agentic init` on line 86 recreates the directory — order is already correct.)

- [ ] **Step 2: Edit `ask.sh`**

Replace line 18 (`python "${here}/agent.py" "$@"`) with:

```bash
"${PYTHON:-python3}" "${here}/agent.py" "$@"
```

- [ ] **Step 3: Ignore the demo venv**

Ensure `examples/langgraph-rollback/.gitignore` contains a `.venv/` line (add it if absent).

- [ ] **Step 4: Syntax-check both scripts**

Run: `bash -n examples/langgraph-rollback/scripts/run-demo.sh && bash -n examples/langgraph-rollback/scripts/ask.sh && echo OK`
Expected: `OK`. (Full end-to-end verification is Task 10, after the release profile is fixed.)

Note (macOS bash 3.2 + `set -u`): the added code uses only scalar variables — no arrays — per the repo's known portability constraint.

- [ ] **Step 5: Commit**

```bash
git add examples/langgraph-rollback/scripts/run-demo.sh examples/langgraph-rollback/scripts/ask.sh examples/langgraph-rollback/.gitignore
git commit -m "Make the demo self-contained: short socket path, venv bootstrap, python3, stale checkpoint cleanup"
```

---

### Task 8: Release profile strip fix (gap 1)

**Files:**
- Modify: `Cargo.toml` (workspace root, line 73)

**Interfaces:** none.

- [ ] **Step 1: Change the strip level**

In `[profile.release]`, replace `strip = "symbols"` with:

```toml
# "symbols" corrupts proc-macro dylibs (sqlx-macros) under macOS ld
# 27031+ — "mis-aligned LINKEDIT string pool" at dlopen (issue #124
# gap 1, bug-132). "debuginfo" keeps most of the size win without
# touching the symbol table the new linker trips on.
strip = "debuginfo"
```

- [ ] **Step 2: Verify with a real release build (the whole point — CI can't see this)**

Run: `unset CARGO_PROFILE_RELEASE_STRIP; cargo build --release -p agenticd -p agentic-cli && ./target/release/agenticd --help >/dev/null && ./target/release/agentic --help >/dev/null && echo RELEASE-OK`
Expected: `RELEASE-OK` (build takes several minutes). If the sqlx-macros dlopen error reappears, fall back to deleting the `strip` line entirely, re-run, and note the substitution in the commit message.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml
git commit -m "Relax release strip to debuginfo so macOS ld stops corrupting proc-macro dylibs"
```

---

### Task 9: CI jobs — macOS release build + demo smoke

**Files:**
- Modify: `.github/workflows/ci.yml`

**Interfaces:** none.

- [ ] **Step 1: Add the two jobs**

Append to `.github/workflows/ci.yml` under `jobs:` (sibling of `rust`, `python`, `gcs`, `postgres`):

```yaml
  # Release-profile build on macOS. CI otherwise builds debug only, which
  # made the strip="symbols" proc-macro corruption (issue #124 gap 1)
  # invisible: the new macOS linker corrupts stripped proc-macro dylibs
  # at dlopen time, which only a release build exercises.
  release-build:
    name: release build (macos)
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.95.0
      - uses: Swatinem/rust-cache@v2
      - name: build release binaries
        run: cargo build --release -p agenticd -p agentic-cli
      - name: smoke-run binaries
        run: |
          ./target/release/agenticd --help >/dev/null
          ./target/release/agentic --help >/dev/null

  # The broken-prompt demo, end-to-end. The demo IS the MVP discipline
  # (CLAUDE.md); the ADR-0013 scanner regression (issue #124 gap 5) sat
  # unnoticed since 2026-05-21 because nothing ran it. run-demo.sh
  # bootstraps its own venv and picks docker when podman is absent.
  demo:
    name: demo smoke (run-demo.sh)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@1.95.0
      - uses: Swatinem/rust-cache@v2
      - uses: actions/setup-python@v5
        with:
          python-version: "3.12"
      - name: install postgres client
        run: sudo apt-get update && sudo apt-get install -y --no-install-recommends postgresql-client
      - name: run the broken-prompt demo end-to-end
        run: ./examples/langgraph-rollback/scripts/run-demo.sh
```

- [ ] **Step 2: Validate the workflow YAML**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('YAML OK')"` (or `docker run --rm -v "$PWD":/w -w /w rhysd/actionlint:latest` if actionlint is preferred and available).
Expected: `YAML OK`.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "Add macOS release-build and demo-smoke CI jobs so demo breakage cannot land invisibly"
```

---

### Task 10: Clean-state end-to-end verification + bookkeeping

**Files:**
- Modify: `.wolf/buglog.json` (main checkout — OpenWolf metadata lives there)
- Modify: `.wolf/memory.md` (main checkout)

**Interfaces:** none. This task proves the acceptance criterion: `run-demo.sh` green from a clean state.

- [ ] **Step 1: Tear down the diagnosis leftovers from the 2026-07-10 session**

```bash
kill 644 2>/dev/null || true          # diagnosis agenticd (may already be gone)
rm -f /tmp/agentic124.sock /tmp/agentic124-daemon.log /tmp/agent-run2.out
rm -rf .venv-124                       # diagnosis venv at the worktree root
docker compose -f examples/langgraph-rollback/docker-compose.yml down -v
rm -rf examples/langgraph-rollback/.agentic examples/langgraph-rollback/prompts/__langgraph__ examples/langgraph-rollback/.venv
git -C . checkout -- examples/langgraph-rollback/prompts/system.txt 2>/dev/null || true
```

- [ ] **Step 2: Run the demo from the worktree root (nested path — exercises the SUN_LEN fix)**

Run: `./examples/langgraph-rollback/scripts/run-demo.sh`
Expected: all 12 steps print, ending with `✓ broken-prompt demo complete`. The baseline ask (step 4) answers empathetically, the bad ask (step 7) hallucinates a refund, the post-rollback ask (step 11) is empathetic again with clean memory.

- [ ] **Step 3: Run it again from a deeper working directory (path-depth variance)**

```bash
(cd examples/langgraph-rollback && ./scripts/run-demo.sh)
```
Expected: same green run. (Both runs happen inside the worktree — the main checkout stays untouched per worktree discipline; the post-merge `demo` CI job covers the clean-clone case.)

- [ ] **Step 4: Update OpenWolf bookkeeping (main checkout)**

In `/Users/tonibergholm/Developer/github/git.agentic/.wolf/buglog.json`, set on **bug-132**: `"fix": "strip = \"debuginfo\" in [profile.release] (Cargo.toml); macOS release-build CI job guards the regression. Landed on issue-124-demo-green."` and on **bug-133**: `"fix": "ADR-0017 path-scoped entropy exemption: commit staging skips the entropy heuristic for blobs under __langgraph__/ (configurable --scanner-exempt-entropy-prefix); pattern rules still run; rollback threads the same prefixes. SDK gained connect/request socket timeouts. Landed on issue-124-demo-green."`, bumping each entry's `last_seen` to the current date. Append a one-line entry to `.wolf/memory.md` recording the green run.

- [ ] **Step 5: Final gate and wrap-up**

```bash
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
sdk/python/.venv/bin/python -m pytest sdk/python/tests -q
git log --oneline main..HEAD
```
Expected: all green; the branch shows one commit per task. Then hand back to Toni for PR creation (`/commit-push-pr`), including in the PR body: the gap-5 root cause (scanner vs base64 checkpoint envelope, broken since 2026-05-21), why the blob-hash allowlist could not fix it, and a pointer to ADR-0017 and the spec. Closing issue #124 happens via the PR (`Fixes #124`).

---

## Self-Review (completed at plan-writing time)

- **Spec coverage:** ADR-0017 exemption → Tasks 1–5; SDK timeouts → Task 6; gaps 2/3/4 → Task 7; gap 1 → Task 8; both CI jobs → Task 9; clean-state E2E from two path depths → Task 10; rollback re-staging hazard (found during planning — rollback's `read_text_blobs` → `stage_and_commit` re-scans checkpoint blobs) → covered in Tasks 4 and 5.
- **Placeholder scan:** no TBDs; every code step carries the code; the one conditional (Task 8 fallback to dropping `strip`) has an explicit trigger and action.
- **Type consistency:** `ScanPolicy { skip_entropy: bool }` (Tasks 1–4), `put_with_policy(&self, &Object, ScanPolicy) -> Result<Hash>` (Tasks 2–3), `exempt_entropy_prefixes: Vec<String>` on `CommitInputs`/`DaemonState` (Tasks 3–4), `AgenticClient(connect_timeout: float, request_timeout: float)` (Task 6) — names match across tasks.
