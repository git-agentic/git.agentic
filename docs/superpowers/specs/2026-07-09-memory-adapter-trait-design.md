# Complete the `MemoryAdapter` trait (issue #43 / audit §A9) — design

**Date:** 2026-07-09
**Status:** Approved by Toni (brainstorming session 2026-07-09)
**Closes:** [#43](https://github.com/git-agentic/git.agentic/issues/43) (audit §A9; addresses S1, S6, C9; S4 already fixed by #44)

## Context

Issue #43 was written against the 2026-05-21 audit and is already half-done
on `main`:

- Commit `ece20e8` (A9-partial) added `migrations_after`, `begin_restore`,
  and `restore_with_guard` to the trait and shipped the `InMemoryAdapter`
  fixture (the issue's "HashMapAdapter") with rollback round-trip tests.
- `SegmentManifest::from_canonical_bytes` (issue item 4) landed via #44.
- The issue's "Blocked by ADR-0005 (Proposed)" note is stale — ADR-0005 was
  Accepted 2026-05-22.

What remains, and what this design covers:

1. Reverse migrations are not on the trait — `rollback::execute` calls
   `PostgresAdapter`-only inherent methods (`begin_reverse_tx`,
   `apply_down_migration_tx`) via `migrate::run_reverse`, because sqlx 0.8's
   `Transaction` type cannot cross an `async_trait` boundary (HRTB
   incompatibility, documented at `adapter.rs:68-86`).
2. `DaemonState.memory` is `Option<Arc<Mutex<PostgresAdapter>>>`
   (`server.rs:55`) — concrete type, plus an outer mutex that
   over-serializes an internally concurrency-safe pool (audit C9).
3. No `MemoryBackendSpec` factory (audit S6) — the backend is constructed
   inline in `DaemonState::open`, unlike `ObjectStoreSpec`'s parse/open
   split.

## Decisions made during brainstorming

1. **Scope:** all three remaining pieces in one conceptual change; §A9 is
   marked done at the end.
2. **Trait shape:** one coarse method —
   `async fn apply_reverse_migrations(&self, steps: &[MigrationStep]) -> Result<()>`
   — the backend owns transactionality internally. Rejected alternative: an
   opaque `ReverseTx` handle with begin/apply/commit methods (RestoreGuard
   style) — more trait surface and handle-round-tripping for no current
   consumer of per-step orchestration. The coarse shape sidesteps the HRTB
   problem entirely because no transaction type crosses the boundary.
3. **Locking:** drop the adapter `Mutex` entirely. Write-path exclusivity is
   already owned by `DaemonState.commit_lock` (`server.rs:45`, ADR-0001's
   one-commit-at-a-time); `PostgresAdapter` is `&self`/pool-safe;
   `InMemoryAdapter` has interior locking. Resolves C9.
4. **Factory surface:** `MemoryBackendSpec { None, Postgres }` with
   `parse("none" | "postgres")`; Postgres connection details keep coming
   from the existing `PgConfig` env vars. No CLI/config behavior change in
   this pass. URL-style specs ("postgres://…") rejected for now — they
   duplicate the env path and add credential-in-flag concerns with no second
   backend to justify them.
5. **No ADR.** The change completes what §A9/#43 already specified; the
   trait's own doc comment promised this revision. No new crate, wire
   change, or daemon dependency — the ADR triggers in CLAUDE.md don't fire.

## Design

### 1. Trait change (`crates/agentic-memory`)

- `MigrationStep { name: String, sql: String }` moves from
  `agenticd/src/migrate.rs` into `agentic-memory` (exported next to the
  trait); `agenticd` imports it.
- New required trait method:

  ```rust
  /// Apply reverse (down) migrations, in the given order, atomically.
  ///
  /// All-or-nothing: if any step fails, the backend's state must be
  /// unchanged. Postgres implements this as a single transaction with
  /// one commit at the end. The method is deliberately coarse — a
  /// transaction handle cannot cross the async_trait boundary under
  /// sqlx 0.8 (HRTB incompatibility), so each backend owns its own
  /// atomicity mechanism.
  async fn apply_reverse_migrations(&self, steps: &[MigrationStep]) -> Result<()>;
  ```

- The `adapter.rs` doc-comment paragraph explaining why reverse migration is
  *not* a trait method is removed; its HRTB rationale moves into the method
  doc above.
- `PostgresAdapter`: the body of `migrate::run_reverse` moves into the trait
  impl (begin tx → apply each step → single commit). `begin_reverse_tx` and
  `apply_down_migration_tx` become private (`fn` not `pub fn`) — the trait
  is the only reverse-migration surface.
- `InMemoryAdapter`: gains a minimal migration model —
  `applied_migrations: Vec<String>` (ordered, newest last) inside its state;
  `migrations_after(target)` returns the names after `target` (replacing
  the current empty-or-error stub); `apply_reverse_migrations` validates
  every step against the applied list first, then pops them and sets
  `schema_version` accordingly — all-or-nothing preserved without
  transactions. Test helper to seed applied migrations. Still documented as
  a Rule-of-Three fixture, not a production backend.

### 2. Daemon retype (`crates/agenticd`)

- `DaemonState.memory: Option<Arc<dyn MemoryAdapter>>` (trait is already
  `Send + Sync`-bound). Field doc updated: exclusivity between write-path
  operations comes from `commit_lock`, not from the adapter.
- `commit.rs::snapshot_memory`: drop `lock_owned()`; call
  `adapter.snapshot()` directly (already runs under `commit_lock`).
- `rollback/mod.rs::execute`: drop `.lock()` calls; call trait methods on
  the `Arc<dyn MemoryAdapter>`; replace `migrate::run_reverse(&adapter,
  steps)` with `adapter.apply_reverse_migrations(&steps)`.
- `migrate.rs`: keeps `load_steps` (filesystem parsing of down-migration
  files) and its tests; `run_reverse` is deleted (moved into
  `PostgresAdapter`).
- `init(&mut self)` runs on the concrete adapter before `Arc<dyn>` coercion
  (existing pattern in `DaemonState::open`, now inside the factory).

### 3. `MemoryBackendSpec` factory (`crates/agenticd`)

New module (`crates/agenticd/src/membackend.rs`) mirroring `objstore.rs`:

```rust
pub enum MemoryBackendSpec { None, Postgres }

impl MemoryBackendSpec {
    pub fn parse(spec: &str) -> anyhow::Result<Self>;   // "none" | "postgres"
    pub async fn open(
        self,
        store: Arc<dyn ObjectStore + Send + Sync>,
    ) -> anyhow::Result<Option<Arc<dyn MemoryAdapter>>>;
}
```

`open(Postgres)` builds `PgConfig` from the existing env vars, connects,
runs `init()`, and coerces to `Arc<dyn MemoryAdapter>`. `DaemonState::open`
derives the spec from the same config-presence logic it uses today and
delegates construction to the factory — externally observable behavior is
unchanged. Future backends (Mem0/Zep/Letta, v1.1) land as new variants.

### 4. Error handling

- `apply_reverse_migrations` errors must leave state unchanged (contract
  above); `rollback::execute` propagates them through the existing
  `wire_error` mapping unchanged.
- `MemoryBackendSpec::parse` rejects unknown specs with a named-value error
  (same style as `ObjectStoreSpec::parse`).

### 5. Testing

- `agentic-memory` unit tests (`in_memory.rs`):
  `apply_reverse_migrations` ordering, all-or-nothing on an unknown step
  (state unchanged after failure), schema-version tracking through
  `migrations_after` → reverse-apply round trip; dyn-compat assertion stays.
- `agentic-memory`/`agenticd` Postgres paths keep real-Postgres integration
  tests (repo rule: never mock Postgres for snapshot/restore/migration
  tests). The existing `reverse_migration.rs` integration tests are updated
  to drive the trait method instead of `migrate::run_reverse` and must keep
  their outer-transaction assertions.
- New `agenticd` rollback integration test: `rollback::execute` runs with
  `DaemonState.memory` holding an `InMemoryAdapter` — the second backend
  passing the same rollback path (acceptance criterion 3). This does not
  replace any Postgres test; it adds trait-level coverage.
- Existing tests touching `Arc<Mutex<PostgresAdapter>>`
  (`commit_with_memory.rs`, rollback tests) updated for the retype.
- Gates: `cargo test`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo fmt --check`.

### 6. Docs & bookkeeping

- Audit doc `docs/ops/2026-05-21-agenticd-architectural-analysis.md`: §A9
  annotated **DONE 2026-07-09** (trait covers rollback's full surface;
  daemon retyped; factory landed); S1 and S6 noted resolved; R10 row
  updated. (S4 was already fixed by #44 but its text was left stale — fix
  that annotation in the same pass.)
- PR description references and closes #43.

## Acceptance criteria (from #43, restated against current reality)

- [ ] Trait covers everything `rollback::execute` calls (the one missing
      piece is `apply_reverse_migrations`).
- [ ] `DaemonState.memory` is `Option<Arc<dyn MemoryAdapter>>`, no adapter
      mutex.
- [ ] `InMemoryAdapter` implements the full trait (incl. reverse
      migrations) and passes the same rollback integration path.
- [ ] `MemoryBackendSpec` factory exists with parse/open split; daemon
      behavior unchanged.
- [ ] Audit doc §A9 marked done; clippy/fmt/test gates green.

## Out of scope

- Any real second backend (Mem0/Zep/Letta) — v1.1, behind this trait.
- CLI flag for backend selection — add when a second real backend exists.
- Changing `Quiesceable`/`RestoreGuard` semantics (A1, done) or the
  streamer/trigger machinery.
