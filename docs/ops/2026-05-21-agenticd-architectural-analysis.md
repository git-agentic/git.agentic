# Architectural Analysis — `crates/agenticd/` (the daemon)

**Date:** 2026-05-21
**Focus area:** `crates/agenticd/src/` (six files, 1503 lines) + call-graph extensions into `agentic-core`, `agentic-memory`, `agentic-proto`.
**Produced by:** the `han:architectural-analysis` skill — five agents in pipeline: `structural-analyst`, `behavioral-analyst`, `concurrency-analyst`, `risk-analyst`, `software-architect`.
**Tracked by:** the `agenticd architectural-analysis follow-ups` GH meta-issue — see "Follow-up tasks" at the bottom of this doc for the per-recommendation issue links.

This document is the durable evidence behind the v1.0 hardening + v1.1 architectural work items on the `agenticd` daemon. Each follow-up issue links here for the full upstream finding chain.

---

## Executive Summary

**Five findings that matter most:**

1. **The rollback's atomicity claim is currently false** ([C8](#c8) → [R1](#r1)). The trigger poller is **never quiesced during memory restore** — application writes during the restore window are immediately replayed into the just-restored tables. The demo's entire pitch — "atomic rollback across all six dimensions" — is undermined silently with no error surfacing. Must fix before v1.0.
2. **No SIGTERM handler + phantom-branch HEAD on first-commit failure** ([C2](#c2), [B7](#b7) → [R2](#r2)). Process-kill mid-2PC leaves Postgres memory and branch refs pointing at different realities, with no detection or repair. Contradicts ADR-0002 Decision 3.
3. **GCS blocking I/O on the single-threaded LocalSet** ([C1](#c1), [B2](#b2), [B3](#b3) → [R3](#r3)). `reqwest::blocking` calls freeze every connection on the daemon (including read paths that don't hold `commit_lock`) for up to the 30s timeout. Demo is unaffected (uses `FsObjectStore`); Executor sidecar in production is severely affected.
4. **Memory restore is silently skipped when `target.schema_version is None`** ([B9](#b9), [B10](#b10) → [R4](#r4)). The control-flow guard at `rollback.rs:86` gates both schema migrations *and* the memory restore on the presence of `schema_version`. A target commit with memory but no schema version restores prompts and leaves memory at the post-rollback state. Compounded by the dead `accept_data_loss` flag.
5. **Reverse migrations have no outer transaction** ([B8](#b8) → [R5](#r5)). Partial-downgrade failure orphans the schema in a state no committed snapshot ever matches; subsequent rollbacks are non-deterministic.

**Highest-impact architectural moves** (full sketches in [§Software-Architecture Recommendations](#software-architecture-recommendations)):
- **[A1](#a1)** — `Quiesceable` trait + `RestoreGuard` lifetime on `PostgresAdapter::begin_restore()`. Makes the atomicity invariant a type.
- **[A2](#a2)** — New `crates/agenticd/src/lifecycle.rs` owning SIGTERM, commit_lock drain, and startup ref reconciliation.
- **[A3](#a3) + [A4](#a4)** — Extract `handle_commit` into a `crates/agenticd/src/commit.rs` orchestrator with named 2PC phases (mirror what `rollback::execute` already does); then split `rollback.rs` along its three responsibilities.
- **[A8](#a8)** — Wrap `run_reverse` in a single outer transaction; predicate the memory-restore guard on `target.memory_snapshot.is_some()` rather than `schema_version.is_some()`; wire the `accept_data_loss` flag.

**Dimensions that found NO inaction risk:** lock ordering ([C13](#c13) — globally consistent, no deadlock cycle), advisory-lock cancellation safety ([C12](#c12)), `spawn_local` panic isolation ([C11](#c11)), `FsObjectStore` TOCTOU ([C7](#c7) — benign by content-addressing). The daemon's basic concurrency machinery is sound; the failures are in *what it does inside the locks*, not in the lock structure.

**ADR overlap:** ADR-0005 (SessionStore amendment, Proposed) is the natural place to add the missing trait methods on `MemoryAdapter` ([A9](#a9) — deferred). ADR-0010 (planned: wire-protocol error model) unblocks [A6](#a6). ADR-0011 (planned: ObjectStore async-trait shape) is where the full async-trait redesign that solves [R3](#r3) lives — [A5](#a5) has a tactical `spawn_blocking` patch that lands now without waiting on the ADR. ADR-0007 (ephemeral branches, Proposed) governs the diff-atomicity question ([C6](#c6), [A11](#a11) — deferred).

---

## Structural Analysis

<a name="s1"></a>**S1 — `DaemonState.memory` typed to concrete `PostgresAdapter`, not the `MemoryAdapter` trait** (`server.rs:44`, `rollback.rs:87–148`).
The trait exists with `snapshot`/`restore`/`current_schema_version`, but rollback calls `migrations_after` and `apply_down_migration` (`rollback.rs:102, 122`) which only exist on the concrete type. `DaemonState` therefore stores `Option<Arc<Mutex<PostgresAdapter>>>`. Asymmetric to `ObjectStore` (which IS abstracted, via `Arc<dyn ObjectStore + Send + Sync>` at `server.rs:35`). Blocks the v1.1 Mem0/Zep/Letta path at the type level.

<a name="s2"></a>**S2 — `handle_commit` inlined in `server.rs:286–364`; `rollback::execute` extracted as its own module.** Same structural shape (acquire lock, run phased orchestration, return typed output); only rollback was extracted. Adding a third write-path operation has two models to follow.

<a name="s3"></a>**S3 — `rollback.rs` has three unrelated responsibilities.** Phase orchestration (`execute`, lines 43–231), typed object loaders (`load_commit`/`load_tree`/`load_blob`, lines 233–256, pure `ObjectStore` consumers with no rollback logic), and filesystem write-back (`restore_prompts`/`sweep_orphans`, lines 267–304, synchronous `std::fs` I/O).

<a name="s4"></a>**S4 — `load_manifest` (`rollback.rs:258–263`) calls `serde_json::from_slice` directly; no `SegmentManifest::from_canonical_bytes` to mirror the existing `to_canonical_bytes` writer.** Wire-format knowledge leaks out of `agentic-memory` into the daemon.

<a name="s5"></a>**S5 — Schema-version gate duplicated.** `PostgresAdapter::restore` at `postgres.rs:413–417` checks live vs target, AND `rollback.rs:94–108` does it first. Under success the inner guard is a no-op; under partial-migration failure both fire with different error types (`anyhow::Error` vs `Error::SchemaMismatch`).

<a name="s6"></a>**S6 — No `MemoryBackendSpec` factory equivalent to `ObjectStoreSpec`.** `ObjectStoreSpec::parse` + `open` (`objstore.rs:51, 88`) is cleanly testable. Memory backend is constructed inline in `DaemonState::open` (`server.rs:64–80`) with validation, config, connect, and init all in one block.

<a name="s7"></a>**S7 — `read_prompts_for_commit` and `read_tools_for_commit` are identical except for the field name** (`rollback.rs:306–336`). Two-line structural duplication; will recur per new tree-typed dimension.

**Well-structured:** `objstore.rs` (clean parse/open factory), `migrate.rs` (single responsibility, good defensive validation, tempdir-tested), `mcp.rs` (scoped narrowly).

---

## Behavioral Analysis

<a name="b1"></a>**B1 — MCP fingerprinting sequential under `commit_lock`** (`server.rs:146, 327`; `mcp.rs:70–74`). N servers × 10s timeout each = up to 10×N seconds with lock held; rollback shares the lock.

<a name="b2"></a>**B2 — GCS `reqwest::blocking` runs on the LocalSet** (`gcs_store.rs:83–86`; `main.rs:129–141`). A 30s timeout freezes every `spawn_local` task on the single thread, not just the connection that triggered the call.

<a name="b3"></a>**B3 — Memory `Mutex<PostgresAdapter>` held across `store.put_raw`** (`server.rs:304–312`). Under GCS, the lock window extends through synchronous HTTP upload.

<a name="b4"></a>**B4 — `chrono::Utc::now()` inside `stage_and_commit`** (`commit.rs:98–112`) breaks the file header's idempotent-retry claim. Identical inputs produce different commit hashes due to wall-clock timestamp in the Commit struct.

<a name="b5"></a>**B5 — `ObjectKind` parameter of `put_raw` silently discarded** (`store.rs:79`, `gcs_store.rs:270` — both use `_kind`). Dead parameter; caller's `ObjectKind::Tree` has no runtime effect.

<a name="b6"></a>**B6 — Single `Response::Error { message: String }` variant** conflates semantic absence (ref not found, `server.rs:139–142`) with operational failure. No error class, no retryability signal — clients must substring-match.

<a name="b7"></a>**B7 — HEAD advanced before commit body executes** (`server.rs:287–297`). On first-ever commit, `write_head_symbolic` runs unconditionally; if subsequent steps fail, HEAD points at a phantom branch.

<a name="b8"></a>**B8 — Reverse migration sequence has no outer transaction** (`migrate.rs:91–112`). Each step is transactional in isolation; mid-sequence failure orphans the schema at an intermediate version no snapshot was taken against.

<a name="b9"></a>**B9 — Memory restore silently skipped when `target.schema_version=None` but `target.memory_snapshot=Some`** (`rollback.rs:86–157`). The outer `if let Some(ref target_schema)` gates both schema migrations and the memory restore branch. Prompt files are written; memory database is left untouched.

<a name="b10"></a>**B10 — `accept_data_loss` flag is dead code** (`rollback.rs:216`: `let _ = args.accept_data_loss;`). `migrate.rs:137–142`'s error message instructs operators to use it; the flag is not wired.

<a name="b11"></a>**B11 — `restore.rs` module doc promises row-count validation step that the body omits** (`restore.rs:7–13` vs `restore.rs:58–82`). `SegmentRef::row_count` is never queried during restore.

<a name="b12"></a>**B12 — GCS `put`/`put_raw` unconditionally upload** (`gcs_store.rs:241–266`, `270–290`); no equivalent to `FsObjectStore`'s `if !path.exists()` guard. Rollback re-uploads every blob already present in GCS.

<a name="b13"></a>**B13 — Prompts cross wire as `BTreeMap<String, String>`** (`proto/lib.rs:94`). No binary payloads; non-UTF-8 hits JSON-parse error at framing, not structured error.

<a name="b14"></a>**B14 — Frame-level errors close the connection without a response envelope** (`server.rs:111–125`). Client cannot distinguish crashed daemon from frame-size violation.

<a name="b15"></a>**B15 — Snapshot opens a fresh `PgConnection` per call** (`postgres.rs:370–374`). Cancellation-safety rationale is sound, but every commit pays TCP + TLS + auth on the critical path.

**Well-handled:** `commit_lock` design is clear and consistent; object-store integrity checks recompute the hash on every `get`; SQL identifier validation blocks injection; migration name path-traversal validation is defense-in-depth.

---

## Concurrency Analysis

<a name="c1"></a>**C1 — GCS `reqwest::blocking` on the LocalSet** freezes read paths too (`ReadObject`, `Log`, `Diff`, `ResolveRef` don't take `commit_lock`). This is the most impactful real-time finding.

<a name="c2"></a>**C2 — No SIGTERM/shutdown handler in `main.rs:131–148`.** Process kill mid-2PC leaves Postgres memory state and branch ref pointing at different versions of reality. No detection or repair path on next startup. Directly contradicts the ADR-0002 D3 atomic claim.

<a name="c3"></a>**C3 — MCP fingerprinting sequential under commit_lock** (same code as B1, viewed as a concurrency story).

<a name="c4"></a>**C4 — Global `commit_lock` is branch-blind** (`server.rs:39–40`). Two SDK workers on `executor/session-A` vs `executor/session-B` serialize through one mutex despite disjoint state. Correctness is fine; throughput across sessions is single-threaded.

<a name="c5"></a>**C5 — Three separate `memory` mutex acquisitions in rollback** (`rollback.rs:94–109, 121, 144`). Currently safe because `commit_lock` provides outer serialization, but the invariant "`memory.lock()` only under `commit_lock`" is not enforced by type or comment.

<a name="c6"></a>**C6 — `Diff` reads two refs in sequence with no lock** (`server.rs:231–239`). Concurrent commit between the two `resolve` calls produces a logically-inconsistent point-in-time view. Not corrupting, but misleading.

<a name="c7"></a>**C7 — `FsObjectStore` TOCTOU on put/put_raw** (`store.rs:69–85`). Benign — content-addressed; both writes produce identical bytes.

<a name="c8"></a>**C8 — Trigger poller is NOT quiesced during restore** (`postgres.rs:325–333`, spawned on multi-thread tokio runtime, not LocalSet). Application writes during the restore window land in the just-restored tables. **The rollback-correctness killer.**

<a name="c9"></a>**C9 — `Arc<Mutex<PostgresAdapter>>` over-serializes pool operations.** Read-only calls (`current_schema_version`) gated by the outer mutex despite `PgPool` being designed for concurrent access.

<a name="c10"></a>**C10 — Per-connection request loop is serial** (`server.rs:111–125`). Pipelined `Ping` behind a slow `Commit` blocks. Informational; SDK doesn't pipeline today.

<a name="c11"></a>**C11 — `spawn_local` task panic isolation correct.** Lock releases cleanly because tokio `Mutex` has no poison semantics.

<a name="c12"></a>**C12 — Postgres advisory lock cancellation-safety correct** via dedicated `PgConnection` (`postgres.rs:354–374`).

<a name="c13"></a>**C13 — Lock acquisition order globally consistent** across all code paths: `commit_lock` → `memory` → Postgres advisory. No deadlock cycle within a single daemon.

---

## Risk Assessment

| Risk | Findings | Likelihood | Severity | Blast radius | Reversibility | Demo-day? |
|---|---|---|---|---|---|---|
| <a name="r1"></a>**R1** Trigger poller silently un-does rollback | [C8](#c8) | Likely | **Critical** | System-wide | Difficult — silent | **Yes** |
| <a name="r2"></a>**R2** SIGTERM + phantom-branch HEAD | [C2](#c2), [B7](#b7) | Likely | **Critical** | System-wide / single-module | Difficult / moderate | **Yes** |
| <a name="r3"></a>**R3** GCS blocking client on LocalSet | [C1](#c1), [B2](#b2), [B3](#b3) | Near certain | High | System-wide | Moderate | No (FS default) |
| <a name="r4"></a>**R4** Memory restore skipped on `schema_version=None` | [B9](#b9), [B10](#b10) | Possible | High | Single module | Moderate | Depends on demo corpus |
| <a name="r5"></a>**R5** Reverse migrations no outer transaction | [B8](#b8) | Possible | High | Single module | Difficult | Low |
| <a name="r6"></a>**R6** Duplicate schema-version gate | [S5](#s5) | Likely | Medium | Localized | Easy | No |
| <a name="r7"></a>**R7** Wire-protocol errors untyped | [B6](#b6), [B13](#b13), [B14](#b14) | Near certain | Medium | Multi-module | Moderate (ADR) | Low |
| <a name="r8"></a>**R8** Non-deterministic commit hashes | [B4](#b4) | Possible | Medium | Localized | Moderate | Low |
| <a name="r9"></a>**R9** MCP fingerprinting serial | [B1](#b1), [C3](#c3) | Possible | High when triggered | System-wide for lock window | Moderate | No (no MCP in demo) |
| <a name="r10"></a>**R10** MemoryAdapter trait incomplete | [S1](#s1), [S4](#s4), [S6](#s6) | Certain at v1.1 | Medium | Multi-module | Difficult | No |
| <a name="r11"></a>**R11** Code organization | [S2](#s2), [S3](#s3), [S7](#s7), [B5](#b5), [C7](#c7), [C11](#c11), [C12](#c12), [C13](#c13) | n/a | Low | n/a | n/a | No |

**Priorities:**

- **Must fix before v1.0:** R1 (C8), R2 (C2, B7), R4 (B9, B10), R5 (B8).
- **Should fix in hardening sprint:** R3 (if Executor ships GCS in v1.0), R7, R9, R6.
- **v1.1:** R8, R10. R11 is no-action.

---

## Software-Architecture Recommendations

<a name="a1"></a>**A1 — `Quiesceable` trait + `RestoreGuard` on `PostgresAdapter::begin_restore()`** — **DONE 2026-05-21** (issue [#35](https://github.com/git-agentic/git.agentic/issues/35)).
*Addresses:* [C8](#c8), [R1](#r1). *Principle:* SRP + invariant-as-type. *Effort:* M. *Blocker for v1.0.*

```rust
// crates/agentic-memory/src/triggers.rs
pub trait Quiesceable { async fn pause(&self) -> QuiesceToken; }
// crates/agentic-memory/src/restore.rs
pub struct RestoreGuard<'a> { _token: QuiesceToken, conn: &'a mut PgConnection }
impl PostgresAdapter {
    pub async fn begin_restore(&self) -> Result<RestoreGuard<'_>>;
}
// crates/agenticd/src/rollback.rs
let guard = memory.begin_restore().await?;
memory.restore(&guard, snapshot).await?;
drop(guard); // poller resumes
```

**Shipped shape:**

- `Quiesceable` + `QuiesceToken` live in `crates/agentic-memory/src/triggers.rs`. `spawn_poller` now returns a `PollerHandle` that implements `Quiesceable`; the poller's per-tick `pause_lock.lock().await` blocks while a token is held.
- `RestoreGuard` lives in `crates/agentic-memory/src/restore.rs`. It owns the `QuiesceToken` and is non-`'a`-parameterised — the `&mut PgConnection` field in the audit pseudocode was elided as not load-bearing for v1.0; the connection still comes from the pool inside `restore_manifest`.
- `PostgresAdapter::begin_restore() -> RestoreGuard` and `restore_with_guard(&guard, target)` are public on the adapter. The trait `MemoryAdapter::restore(target)` becomes a convenience wrapper that calls both — rollback paths use the explicit form so the quiesce window is visible at the call site (`crates/agenticd/src/rollback.rs`).
- **Load-bearing extra fix:** `restore_manifest` adds `TRUNCATE public.agentic_change_log` inside the restore transaction. The audit pseudocode pauses the poller but doesn't address change-log entries that were already there before the restore started — those would be drained by the poller as soon as the guard releases and would describe rows the TRUNCATE wiped. Truncating the log inside the restore tx covers both pre-existing entries and entries written by the restore's own INSERTs (which fire user triggers).
- **PgConfig.poll_interval** added (defaults to `triggers::DEFAULT_POLL_INTERVAL` 100 ms) so tests can extend the poller's idle window deterministically.

**Tests landed:** `crates/agentic-memory/tests/integration.rs::ac1_writes_during_restore_are_reverted` — 100 INSERTs accumulate in `agentic_change_log` (with the poller on a 60s interval so it can't drain); `adapter.restore(&handle_A)` pauses the poller, restores the baseline, truncates the log inside its tx, and releases the poller; a post-restore snapshot manifest matches the baseline (no leaked rows in the streamer view) and `agentic_change_log` is empty. Run serial (`--test-threads=1`) because `public.agentic_change_log` is shared across schemas by design.

<a name="a2"></a>**A2 — New `crates/agenticd/src/lifecycle.rs`: SIGTERM, commit_lock drain, startup ref reconciliation** — **DONE 2026-05-21** (issue [#36](https://github.com/git-agentic/git.agentic/issues/36)).
*Addresses:* [C2](#c2), [B7](#b7), [R2](#r2). *Principle:* SRP + DIP. *Effort:* M.

```rust
pub struct Lifecycle { shutdown: CancellationToken, commit_lock: Arc<Mutex<()>> }
impl Lifecycle {
    pub fn install_signal_handlers(&self);
    pub async fn drain(self) -> Result<()>;
}
pub async fn reconcile_refs_on_startup(repo: &Repo, store: &dyn ObjectStore) -> Result<()>;
```

**Shipped shape:**

- `crates/agenticd/src/lifecycle.rs` houses `Lifecycle { shutdown: CancellationToken, commit_lock: Arc<Mutex<()>> }` plus `install_signal_handlers()`, `shutdown_token()`, and `drain(&self)`. The drain method takes `&self` rather than `self` so the binary can call it after the accept loop exits without consuming the lifecycle prematurely.
- `tokio-util` (workspace dep) provides `CancellationToken`. SIGTERM and SIGINT both raise the same shutdown token on Unix; Ctrl+C raises it on non-Unix.
- The accept loop in `main.rs` is now `tokio::select! { _ = shutdown.cancelled() => break, accept = listener.accept() => { ... } }`. After the loop exits, `lifecycle.drain().await` blocks on the same `commit_lock` that `handle_commit` and `handle_rollback` hold while running their 2PC sequences — so the daemon never exits while a partial commit is in flight (ADR-0002 D3 atomicity guarantee preserved under operator-driven shutdowns).
- **B7 fix lives at the call site, not in the reconciler**: `server::handle_commit` no longer writes `HEAD -> refs/heads/<branch>` upfront on a first-ever commit. The `needs_head_write` flag is computed up front; the `write_head_symbolic` call moves to after `stage_and_commit` returns Ok. This closes the "phantom HEAD" window structurally — HEAD is published only once a commit blob exists and its branch ref has been pointed at it.
- **Startup reconciler is defence-in-depth, not magic recovery.** `reconcile_refs_on_startup(refs, store)` runs before the socket is bound. It scans every branch ref under `<agentic_dir>/refs/heads/`, verifies each tip hash exists in the object store, and returns `Err` listing every broken branch when any fails. The reconciler deliberately does NOT silently "rewind one parent back" (the audit pseudocode wording): without the commit blob in the store, the parent hash can't be read, and a non-malicious crash usually leaves the branch ref pointing at the previous (valid) tip — the orphan is just the in-progress commit blob. Loud detection + operator intervention is safer for v1.0 than auto-mutation.
- **New helper in agentic-core:** `Refs::list_branches() -> Result<Vec<String>>` reads `<agentic_dir>/refs/heads/` and returns the branch names (skipping `.tmp` files left by interrupted atomic writes).

**Tests landed:** 7 unit tests in `crates/agenticd/src/lifecycle.rs#cfg(test)`:
- `reconcile_passes_on_fresh_repo` — no branches, no error.
- `reconcile_passes_when_branch_tip_is_in_store` — happy path.
- `reconcile_rejects_branch_ref_with_missing_tip` — AC for issue #36: a branch ref pointing at a hash not in the store is detected and surfaced.
- `reconcile_lists_every_broken_branch` — multi-branch reporting; healthy branches are not flagged.
- `drain_returns_immediately_when_no_commit_in_flight` — drain is a no-op on idle daemon.
- `drain_waits_for_in_flight_commit_to_finish` — drain blocks until the commit_lock releases (the ADR-0002 D3 promise under shutdown).
- `shutdown_token_initially_not_cancelled` — sanity for the token's initial state.

Plus `list_branches_returns_existing_names` in `agentic-core::refs#tests`.

<a name="a3"></a>**A3 — Extract `handle_commit` into `crates/agenticd/src/commit.rs` with named 2PC phases** — **DONE 2026-05-21** (issue [#38](https://github.com/git-agentic/git.agentic/issues/38)).
*Addresses:* [S2](#s2), [B4](#b4) (cheap fix via injected `now_fn`), [B5](#b5), [B7](#b7) partially. *Principle:* SRP + OCP. *Effort:* M.

**Originally proposed** (kept for historical record):

```rust
pub struct CommitCtx<'a> {
    store: &'a dyn ObjectStore, memory: &'a dyn MemoryAdapter,
    repo: &'a Repo, now: fn() -> DateTime<Utc>,
}
pub async fn execute(ctx: CommitCtx<'_>, req: CommitRequest) -> Result<CommitId> {
    let blobs   = stage_blobs(&ctx, &req).await?;
    let segment = build_segment(&ctx, &blobs).await?;
    let commit  = build_commit_blob(&ctx, &segment, &req).await?;
    push_commit(&ctx, &commit).await?;        // single commit point per ADR-0002 D3
    update_ref(&ctx, &commit).await?;
    Ok(commit.id)
}
```

**Shipped shape** (differs from the proposal in three ways noted below):

```rust
// crates/agenticd/src/commit.rs
pub async fn execute(state: Arc<DaemonState>, input: CommitInput) -> Result<CommitOutput>;
pub async fn execute_with_now(state: Arc<DaemonState>, input: CommitInput, now: DateTime<Utc>) -> Result<CommitOutput>;
// private phases:
async fn snapshot_memory(state, no_memory) -> Result<(Option<Hash>, Option<String>)>;
async fn fingerprint_tools(state) -> Result<BTreeMap<String, Vec<u8>>>;
fn assemble_inputs(input, parent, memory_snapshot, schema_version, tools) -> CommitInputs;
fn publish_head(state, branch, commit_hash, needs_head_write);
// crates/agentic-core/src/commit.rs (B4 fix):
pub fn stage_and_commit_with_now<S>(store, refs, branch, inputs, now: DateTime<Utc>) -> Result<CommitOutputs>;
// `stage_and_commit` is now a thin wrapper that calls `_with_now(Utc::now())`.
```

**Departures from the audit pseudocode:**

- The phases live at the daemon (agenticd) level rather than the core (agentic-core) level. The audit's pseudocode proposed extracting the staging steps (`stage_blobs`, `build_segment`, `build_commit_blob`, `push_commit`, `update_ref`) at the orchestrator. Those steps already exist inside `agentic_core::commit::stage_and_commit` and only have one caller — extracting them again at the daemon level would duplicate the work. Instead, the agenticd-side phases (`snapshot_memory`, `fingerprint_tools`, `assemble_inputs`, `publish_head`) are the work that's actually agenticd-specific; phase 4 delegates to `stage_and_commit_with_now` for the ADR-0002 D3 single commit point.
- The B4 fix is a new `stage_and_commit_with_now(..., now: DateTime<Utc>)` function in `agentic-core`, with `stage_and_commit` keeping its current signature as a wrapper that reads `Utc::now()` once. Existing callers (the rollback path's forward-record step) are unchanged.
- `Arc<DaemonState>` is the parameter shape because that's what the dispatch arm in `server.rs` already passes (mirrors `rollback::execute`). A new `CommitCtx<'a>` struct was considered but rejected as scope creep — it would have churned every test and the rollback path with no functional gain.
- **B5 (`ObjectKind` parameter discarded by `put_raw`) is NOT addressed here.** Dropping the parameter requires changing the `ObjectStore` trait — wider blast radius than A3's scope. Filed for a follow-up.

**Tests landed:**

- `agentic_core::commit::tests::stage_and_commit_with_now_is_deterministic` — AC for §B4: identical `(CommitInputs, now)` across two independent tempdirs produces the same `commit_hash`.
- `agentic_core::commit::tests::stage_and_commit_with_now_differs_when_timestamp_differs` — companion guard: timestamp is part of the commit blob's content, so different `now` correctly yields different hash.
- `agenticd::commit::tests::execute_with_now_is_deterministic` — end-to-end determinism through the daemon-level orchestrator.
- `agenticd::commit::tests::execute_publishes_head_on_first_commit` — confirms the §B7 fix from A2 is preserved across the extraction (HEAD is written symbolically AFTER `stage_and_commit_with_now` succeeds).
- `agenticd::commit::tests::execute_chains_commits_on_same_branch` — sanity for parent linkage + the HEAD-already-set path.

Total: 5 new tests + the old `handle_commit` body deleted from `server.rs:319-432`. 27 lib tests in agentic-core (was 25), 46 in agenticd (was 43). 89 lib tests workspace-wide.

<a name="a4"></a>**A4 — Split `rollback.rs` into `mod.rs` (orchestration), `loaders.rs` (typed object readers), `writeback.rs` (FS prompts/tools)** — **DONE 2026-05-21** (issue [#39](https://github.com/git-agentic/git.agentic/issues/39)).
*Addresses:* [S3](#s3), [S7](#s7), [R11](#r11), [R6](#r6) (drop the duplicate guard). *Principle:* SRP + High Cohesion. *Effort:* S.

**Originally proposed** (kept for historical record; the shipped shape below differs in three ways noted in "Shipped shape"):

```rust
// crates/agenticd/src/rollback/loaders.rs
pub async fn load_commit(s: &dyn ObjectStore, id: &Hash) -> Result<Commit>;
pub async fn load_tree  (s: &dyn ObjectStore, id: &Hash) -> Result<Tree>;
pub async fn load_blob  (s: &dyn ObjectStore, id: &Hash) -> Result<Bytes>;
// crates/agenticd/src/rollback/writeback.rs
pub async fn read_text_blobs(commit: &Commit, field: TreeField) -> Result<Vec<(PathBuf,String)>>;
```

**Shipped signatures** (differ from the proposal: loaders are sync `pub(super)` taking `&DaemonState` instead of async public on `&dyn ObjectStore`; `read_text_blobs` takes an `Option<Hash>` instead of a `TreeField` enum — see "Shipped shape" below for why):

```rust
// crates/agenticd/src/rollback/loaders.rs
pub(super) fn load_commit  (state: &DaemonState, hash: &Hash) -> Result<Commit>;
pub(super) fn load_tree    (state: &DaemonState, hash: &Hash) -> Result<Tree>;
pub(super) fn load_blob    (state: &DaemonState, hash: &Hash) -> Result<Blob>;
pub(super) fn load_manifest(state: &DaemonState, hash: &Hash) -> Result<SegmentManifest>;
// crates/agenticd/src/rollback/writeback.rs
pub(super) fn restore_prompts  (state: &DaemonState, repo: &Path, prompts_hash: &Hash) -> Result<()>;
pub(super) fn read_text_blobs  (state: &DaemonState, hash: Option<Hash>) -> Result<BTreeMap<String, Vec<u8>>>;
pub(super) fn read_model_text  (state: &DaemonState, target: &Commit) -> Result<Option<String>>;
```

**Shipped shape:**

- `crates/agenticd/src/rollback.rs` (462 lines) replaced by `crates/agenticd/src/rollback/{mod.rs, loaders.rs, writeback.rs}`.
- `loaders.rs` owns `load_commit`, `load_tree`, `load_blob`, `load_manifest` — pure `ObjectStore` consumers, no rollback logic. Functions are `pub(super)` so the orchestrator imports them, and they take `&DaemonState` (not `&dyn ObjectStore`) to match the existing call signature; switching to `&dyn ObjectStore` was rejected as scope creep for this refactor.
- `writeback.rs` owns FS write-back (`restore_prompts`, `sweep_orphans`) and Commit-tree readers (`read_text_blobs`, `read_model_text`, helper `tree_to_map`).
- **`read_text_blobs(state, Option<Hash>)` collapses the previous `read_prompts_for_commit` / `read_tools_for_commit` pair** (audit [§S7](#s7)). Call sites at `mod.rs` now read `read_text_blobs(&state, target.prompts)` / `read_text_blobs(&state, target.tools)` — the `Option<Hash>` parameter makes the field selector explicit and removes the duplicated body.
- `mod.rs` owns phase orchestration (`execute`, `validate_target_shape`, `RollbackArgs`) plus the existing `#[cfg(test)] mod tests` for the validate-target-shape AC tests carried over from A8.
- **R6 (schema-version gate duplication) re-examined:** post-A8, the rollback-side check at `mod.rs` ~line 117 is a *planning* step (decides whether reverse migrations are needed and how many), not a *gate*. The actual gate lives in `PostgresAdapter::restore_with_guard` (`postgres.rs:415`) and only runs on the success path — A8's outer-transaction rollback ensures partial-migration failure can't leave the daemon between schema versions, so the two checks no longer raise inconsistent error types. The duplication noted in S5 is now informational; left in place with a comment in `mod.rs` documenting why.

**Tests:** all four `validate_target_shape` unit tests from A8 still pass (moved to `mod.rs#tests`). All 3 A8 integration tests (`reverse_migration.rs`) still pass against real Postgres. No new tests required — A4 is a behavior-preserving refactor.

<a name="a5"></a>**A5 — Move GCS blocking I/O off LocalSet via `tokio::task::spawn_blocking`** (tactical now; full async-trait via ADR-0011 follows)
*Addresses:* [C1](#c1), [B2](#b2), [B3](#b3), [R3](#r3). *Principle:* Loose coupling between execution model and I/O. *Effort:* S tactical / M with ADR.

```rust
impl ObjectStore for GcsObjectStore {
    async fn put_raw(&self, h:&Hash, k:ObjectKind, b:Bytes) -> Result<()> {
        let client = self.client.clone();
        tokio::task::spawn_blocking(move || client.put_blocking(h,b)).await?
    }
}
```

<a name="a6"></a>**A6 — `Response::Error` becomes a structured variant; framing errors get an envelope** *(daemon-side patch lands when ADR-0010 lands)*
*Addresses:* [B6](#b6), [B13](#b13), [B14](#b14), [R7](#r7). *Principle:* ISP + LSP. *Effort:* S after wire-shape decision.
**Wire-protocol shape decision deferred to ADR-0010** — needs ADR for backward compat across SDK/CLI/daemon versions.

<a name="a7"></a>**A7 — Parallelise MCP fingerprinting with `FuturesUnordered` + semaphore**
*Addresses:* [B1](#b1), [C3](#c3), [R9](#r9). *Effort:* S.

```rust
let mut fs = FuturesUnordered::new();
for server in servers { fs.push(fingerprint_one(server, sem.clone())); }
let prints: Vec<_> = fs.try_collect().await?;
```

<a name="a8"></a>**A8 — Reverse-migration outer transaction + memory-restore guard fix + wire `accept_data_loss`** — **DONE 2026-05-21** (issue [#37](https://github.com/git-agentic/git.agentic/issues/37), branch `fix/a8-reverse-migration-tx-and-restore-guard`).
*Addresses:* [B8](#b8), [B9](#b9), [B10](#b10), [R4](#r4), [R5](#r5). *Principle:* SRP. *Effort:* S. *Must ship pre-v1.0.*

```rust
pub async fn run_reverse(conn:&mut PgConnection, migs:&[Migration]) -> Result<()> {
    let mut tx = conn.begin().await?;
    for m in migs { apply_down(&mut tx, m).await?; }
    tx.commit().await
}
fn needs_memory_restore(t:&Target) -> bool { t.memory_snapshot.is_some() }
```

**Shipped shape** (see [docs/plans/a8-reverse-migration/](../plans/a8-reverse-migration/) for the full plan and rejected alternatives):

- **B8** atomicity threads a single `sqlx::Transaction` through `PostgresAdapter::begin_reverse_tx()` + `apply_down_migration_tx(tx, name, sql)`. The audit pseudocode above as written would NOT have delivered atomicity — `apply_down_migration` opened its own `self.pool.begin()` (a separate Postgres session). Caught by junior-developer in planning Round 1 ([J1](../plans/a8-reverse-migration/artifacts/implementation-iteration-history.md#r1)).
- **B9** is fixed by **rejecting** Commits with `memory_snapshot=Some, schema_version=None` at validation time, not by attempting to restore. Rationale: server-side commit production always yields both `Some` together or both `None` together, so the mixed shape is unreachable in v1.0; loud rejection resolves "silent skip" without changing the `MemoryAdapter` trait contract. User-decided ([D-1](../plans/a8-reverse-migration/artifacts/implementation-decision-log.md#d-1-q2-resolution--memory_snapshotsome-schema_versionnone-commits-are-rejected-as-malformed)) after specialist escalation.
- **B10** wires `accept_data_loss` through `load_steps` → `check_irreversible`. When `true`, IRREVERSIBLE-marked down.sql is loaded and executed; the v1.1 bounded-rollback path (ADR-0002 D5) stays unimplemented and `--accept-data-loss` does NOT trigger it.

**Tests landed:** AC3a/AC3b unit tests in `crates/agenticd/src/migrate.rs`; AC1 + AC3c integration tests + happy-path regression in `crates/agenticd/tests/reverse_migration.rs` (real Postgres, gated by `#[ignore]`); AC2 unit tests in `crates/agenticd/src/rollback.rs`. Full workspace `cargo test` + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --check` green.

<a name="a9"></a>**A9 — Defer: `MemoryAdapter` trait completeness** ([S1](#s1), [S4](#s4), [S6](#s6), [R10](#r10)) — blocked by ADR-0005; single backend today (Rule-of-Three fails).

<a name="a10"></a>**A10 — Defer: `SegmentManifest::from_canonical_bytes`** ([S4](#s4)) — aesthetic; address on next manifest schema change.

<a name="a11"></a>**A11 — Defer: diff atomicity** ([C6](#c6)) — blocked by ADR-0007 (ephemeral branches semantics decided first).

<a name="a12"></a>**A12 — Intentionally not addressed:** [C7](#c7), [C11](#c11), [C12](#c12), [C13](#c13) (correct/benign per analyst); [B12](#b12) (idempotent GCS uploads — performance only, YAGNI); [B15](#b15) (per-snapshot PgConnection — tactical patch inside `PostgresAdapter`, no architectural change); [C4](#c4), [C5](#c5) (correct under current single-writer assumption; revisit when goals change).

**Ship order (highest impact first):**
1. **A1** — demo correctness
2. **A8** — data integrity
3. **A2** — operational safety
4. **A3 + A4** — give the algorithms a home (unlocks the others)
5. **A5, A7** — hardening sprint
6. **A6** after ADR-0010 lands

---

## System-level concerns deferred

These cross a service or wire-protocol boundary; they get their own ADRs:

1. **ADR-0010 — Wire-protocol error model + binary payloads** (addresses [A6](#a6) / [R7](#r7) / [B6](#b6), [B13](#b13), [B14](#b14)). The daemon-side patch is small once the protocol shape is decided, but the shape itself needs an ADR covering backward compat across the (SDK, CLI, daemon) deployment fleet.
2. **ADR-0011 — `ObjectStore` async-trait shape** (addresses [A5](#a5) / [R3](#r3)). The tactical `spawn_blocking` patch (A5) lands immediately, but the trait-level cleanup that fully removes the LocalSet freeze risk is cross-crate and is exactly what ADR-0006 Decision 2 already proposes. ADR-0011 pins down the actual async-trait shape.
3. **Cross-daemon coordination** ([C13](#c13)'s "out of scope for v1.0 single-daemon" footnote). If/when the deployment model grows beyond a single daemon sharing one `.agentic/` directory + one Postgres instance, branch-ref atomicity needs a coordinator. Not a v1.0 concern; no ADR yet.

---

## Follow-up tasks

Each architectural recommendation has its own GH issue. Tracking meta-issue lists them all with checkboxes; child issues carry the priority label, milestone, and link back here for evidence.

(Issue numbers populated once issues are created.)

| Rec | Title | Issue | Label | Milestone |
|---|---|---|---|---|
| A1 | Quiesce trigger poller during memory restore | [#35](https://github.com/git-agentic/git.agentic/issues/35) **DONE** | `must-fix-v1.0` | `v1.0` |
| A2 | Lifecycle module: SIGTERM drain + startup ref reconciliation | [#36](https://github.com/git-agentic/git.agentic/issues/36) **DONE** | `must-fix-v1.0` | `v1.0` |
| A8 | Reverse-migration outer transaction + restore-guard fix + wire `accept_data_loss` | [#37](https://github.com/git-agentic/git.agentic/issues/37) **DONE** | `must-fix-v1.0` | `v1.0` |
| A3 | Extract `handle_commit` into `commit.rs` orchestrator | [#38](https://github.com/git-agentic/git.agentic/issues/38) **DONE** | `hardening-sprint` | — |
| A4 | Split `rollback.rs` into `mod` / `loaders` / `writeback` | [#39](https://github.com/git-agentic/git.agentic/issues/39) **DONE** | `hardening-sprint` | — |
| A5 | Move GCS blocking I/O off LocalSet via `spawn_blocking` | (TBD) | `hardening-sprint` | — |
| A6 | Structured `Response::Error` + framing-error envelope (blocked by ADR-0010) | (TBD) | `hardening-sprint` | — |
| A7 | Parallelise MCP fingerprinting with `FuturesUnordered` | (TBD) | `hardening-sprint` | — |
| A9 | Complete `MemoryAdapter` trait (blocked by ADR-0005) | (TBD) | `v1.1` | — |
| A10 | Add `SegmentManifest::from_canonical_bytes` | (TBD) | `v1.1` | — |
| A11 | `Diff` atomicity (blocked by ADR-0007) | (TBD) | `v1.1` | — |

This doc will be updated with the issue numbers once they are created.
