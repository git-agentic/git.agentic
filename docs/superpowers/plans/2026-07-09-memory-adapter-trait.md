# Complete MemoryAdapter Trait (#43) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put reverse migrations on the `MemoryAdapter` trait, retype `DaemonState.memory` to `Option<Arc<dyn MemoryAdapter>>` (dropping the adapter mutex), and add a `MemoryBackendSpec` factory — closing issue #43 / audit §A9.

**Architecture:** One coarse trait method `apply_reverse_migrations(&self, steps: &[MigrationStep])` — each backend owns its own atomicity (Postgres: one transaction), so no `sqlx::Transaction` crosses the async_trait boundary (the HRTB blocker). Write-path exclusivity is already owned by `DaemonState.commit_lock` (ADR-0001, one commit at a time), so the adapter-level `Mutex` is dropped, not relocated.

**Tech Stack:** Rust 1.95 workspace; `async_trait`, `sqlx 0.8`, `tokio`; crates `agentic-memory` and `agenticd`.

**Spec:** `docs/superpowers/specs/2026-07-09-memory-adapter-trait-design.md`.

> **Spec correction (flagged for Toni):** the spec sketched
> `MemoryBackendSpec::parse("none" | "postgres")` with connection details
> "from the existing PgConfig env vars". The daemon actually configures
> Postgres via **CLI flags** (`postgres_url: Option<&str>` + `tables:
> Vec<TrackedTable>` into `DaemonState::open`; there are no env vars).
> The plan therefore gives the factory a `from_flags(postgres_url,
> tables)` constructor carrying the same values — honoring the spec's
> binding intent (parse/open split, behavior unchanged, variants for
> future backends) while matching reality. A string `parse` would be
> dead code today (YAGNI).

## Global Constraints

- Work ONLY in the worktree `.worktrees/a9-memory-adapter-trait/` (branch `a9-memory-adapter-trait`); never the main checkout.
- Gates for every task: the named tests pass, plus `cargo fmt` (run it, don't just check). Final task additionally runs `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check`.
- No `unwrap()` in non-test code without a `// SAFETY:` or `// INVARIANT:` comment.
- Never mock Postgres for snapshot/restore/migration tests — Postgres-path integration tests run against real Postgres (fixture: `docker compose -f tests/fixtures/pg.yml up -d`, port per that file). If Docker is unavailable in your environment, compile the tests (`cargo test -p agenticd --no-run`) and say so explicitly in your report — do not delete or `#[ignore]` them.
- Externally observable daemon behavior must not change: same CLI flags, same error for `--postgres` without `--tables`, same log lines ("memory backend attached").
- Existing public error-message phrases asserted by tests (e.g. `"not the baseline"`, `"not the live schema"` in `InMemoryAdapter::migrations_after`) must be preserved.
- Commits: plain prose, imperative mood, no conventional-commits prefixes.

---

### Task 1: `agentic-memory` — `MigrationStep` + `apply_reverse_migrations` on the trait, both backends

**Files:**
- Modify: `crates/agentic-memory/src/adapter.rs` (trait doc lines 77–86, new type + method)
- Modify: `crates/agentic-memory/src/postgres.rs` (trait impl; inherent methods stay `pub` until Task 2)
- Modify: `crates/agentic-memory/src/in_memory.rs` (migration model + trait impl + tests)
- Modify: `crates/agentic-memory/src/lib.rs` (re-export)

**Interfaces:**
- Consumes: existing `MemoryAdapter` trait, `Error`/`Result`, `tracing` (already a dependency — `postgres.rs:560` uses it).
- Produces (Tasks 2–3 rely on these exactly):
  - `pub struct MigrationStep { pub name: String, pub sql: String }` in `agentic_memory::adapter`, re-exported as `agentic_memory::MigrationStep`.
  - Trait method `async fn apply_reverse_migrations(&self, steps: &[MigrationStep]) -> Result<()>` on `MemoryAdapter`, implemented by `PostgresAdapter` and `InMemoryAdapter`.
  - `InMemoryAdapter::apply_migration(&self, name: impl Into<String>)` test helper (records applied migration and sets live schema version to `name`).

- [ ] **Step 1: Write the failing tests** — append inside `mod tests` in `crates/agentic-memory/src/in_memory.rs`:

```rust
    #[tokio::test]
    async fn reverse_migrations_pop_applied_and_update_schema() {
        let adapter = fixture();
        adapter.apply_migration("001_init").await;
        adapter.apply_migration("002_add_embeddings").await;
        adapter.apply_migration("003_widen_body").await;
        assert_eq!(
            adapter.current_schema_version().await.unwrap(),
            "003_widen_body"
        );

        // migrations_after returns newest-first — the order they reverse.
        let names = adapter.migrations_after("001_init").await.unwrap();
        assert_eq!(names, vec!["003_widen_body", "002_add_embeddings"]);

        let steps: Vec<MigrationStep> = names
            .iter()
            .map(|n| MigrationStep {
                name: n.clone(),
                sql: String::new(), // the fixture ignores SQL
            })
            .collect();
        adapter.apply_reverse_migrations(&steps).await.unwrap();
        assert_eq!(adapter.current_schema_version().await.unwrap(), "001_init");
        assert!(adapter
            .migrations_after("001_init")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn reverse_migrations_all_or_nothing_on_bad_step() {
        let adapter = fixture();
        adapter.apply_migration("001_init").await;
        adapter.apply_migration("002_x").await;
        // First step valid, second bogus — NOTHING may change.
        let steps = vec![
            MigrationStep {
                name: "002_x".into(),
                sql: String::new(),
            },
            MigrationStep {
                name: "999_nope".into(),
                sql: String::new(),
            },
        ];
        let err = adapter.apply_reverse_migrations(&steps).await.unwrap_err();
        assert!(format!("{err:#}").contains("999_nope"));
        assert_eq!(adapter.current_schema_version().await.unwrap(), "002_x");
        assert_eq!(
            adapter.migrations_after("0.0.0").await.unwrap(),
            vec!["002_x", "001_init"]
        );
    }

    #[tokio::test]
    async fn reverse_migrations_to_baseline_and_empty_noop() {
        let adapter = fixture();
        // Empty steps: no-op even with nothing applied.
        adapter.apply_reverse_migrations(&[]).await.unwrap();
        adapter.apply_migration("001_init").await;
        let steps = vec![MigrationStep {
            name: "001_init".into(),
            sql: String::new(),
        }];
        adapter.apply_reverse_migrations(&steps).await.unwrap();
        assert_eq!(adapter.current_schema_version().await.unwrap(), "0.0.0");
    }
```

Also add `MigrationStep` to the test module's imports (it arrives via `use super::*;` once `in_memory.rs` imports it — see Step 3).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p agentic-memory --lib in_memory 2>&1 | tail -20`
Expected: compile FAILURE — `apply_migration`, `MigrationStep`, `apply_reverse_migrations` not found.

- [ ] **Step 3: Implement in `adapter.rs`**

(a) Add after the `SnapshotHandle` struct (below line 20):

```rust
/// One step in a reverse-migration plan.
///
/// Produced by the daemon's `.down.sql` loader
/// (`agenticd::migrate::load_steps`) in the order returned by
/// [`MemoryAdapter::migrations_after`] — most-recent first. `sql` is the
/// pre-read file content so backends never do filesystem I/O.
#[derive(Debug, Clone)]
pub struct MigrationStep {
    /// Migration stem name as recorded by the backend's bookkeeping
    /// (e.g. `"003_add_embeddings"` in Postgres's `agentic_migrations`).
    pub name: String,
    /// Reverse-migration content. SQL for SQL backends; other backends
    /// may ignore it and reverse by name alone.
    pub sql: String,
}
```

(b) Replace the trait doc paragraph at lines 77–86 (from `/// Reverse-migration application is intentionally not a trait method` through `/// right abstraction, this gets reopened.`) with:

```rust
/// Reverse-migration application is a single coarse method
/// ([`Self::apply_reverse_migrations`]) rather than a begin/apply/commit
/// triple: sqlx 0.8's `Executor<'c>` HRTBs don't unify across
/// async_trait's boxed-future elision, so a transaction handle cannot
/// cross the trait boundary. Each backend owns its own atomicity
/// mechanism instead (audit §A9, issue #43).
```

(c) Add the method at the end of the trait (after `restore_with_guard`):

```rust
    /// Apply reverse (down) migrations, in the given order, atomically.
    ///
    /// All-or-nothing: if any step fails, the backend's observable state
    /// (schema, data, and migration bookkeeping) must be unchanged.
    /// Postgres implements this as one transaction committed only after
    /// every step succeeds. An empty `steps` slice is a no-op.
    ///
    /// `steps` must be in the order returned by
    /// [`Self::migrations_after`] — most-recent first.
    async fn apply_reverse_migrations(&self, steps: &[MigrationStep]) -> Result<()>;
```

- [ ] **Step 4: Implement in `postgres.rs`**

(a) Add `MigrationStep` to the existing `use crate::adapter::…` import.

(b) Add inside the `#[async_trait::async_trait] impl MemoryAdapter for PostgresAdapter` block, after `restore_with_guard` (currently ends line 650):

```rust
    /// One outer transaction; every step's `sql` plus its
    /// `agentic_migrations` bookkeeping delete runs on the same
    /// connection, so any failure drops the transaction and rolls back
    /// the whole sequence (audit §A8 semantics, unchanged — the code
    /// moved here from `agenticd::migrate::run_reverse`).
    async fn apply_reverse_migrations(&self, steps: &[MigrationStep]) -> Result<()> {
        if steps.is_empty() {
            return Ok(());
        }
        let mut tx = self.begin_reverse_tx().await?;
        for step in steps {
            self.apply_down_migration_tx(&mut tx, &step.name, &step.sql)
                .await?;
            tracing::info!(migration = %step.name, "reverse migration applied (in outer tx)");
        }
        tx.commit().await?;
        Ok(())
    }
```

Leave `begin_reverse_tx` / `apply_down_migration_tx` **public** in this task — `agenticd::migrate::run_reverse` still calls them until Task 2 (the workspace must compile at every commit).

- [ ] **Step 5: Implement in `in_memory.rs`**

(a) Import: change line 33 to

```rust
use crate::adapter::{MemoryAdapter, MigrationStep, RestoreGuard, SnapshotHandle};
```

(b) `InMemoryState` gains a field:

```rust
#[derive(Default)]
struct InMemoryState {
    schema_version: String,
    tables: HashMap<String, InMemoryTable>,
    /// Applied forward migrations, oldest → newest. The live
    /// `schema_version` is the last entry (or `"0.0.0"` when empty),
    /// mirroring the Postgres convention where the schema version is
    /// the most recent `agentic_migrations` stem.
    applied_migrations: Vec<String>,
}
```

Initialize `applied_migrations: Vec::new()` in `InMemoryAdapter::new`.

(c) Add the test helper next to `set_schema_version`:

```rust
    /// Test helper: record a forward migration as applied and set the
    /// live schema version to its name (the Postgres convention).
    pub async fn apply_migration(&self, name: impl Into<String>) {
        let mut state = self.state.lock().await;
        let name = name.into();
        state.applied_migrations.push(name.clone());
        state.schema_version = name;
    }
```

(d) Replace the whole `migrations_after` method (lines 158–181) with:

```rust
    async fn migrations_after(&self, target_name: &str) -> Result<Vec<String>> {
        let state = self.state.lock().await;
        if target_name == "0.0.0" {
            return Ok(state.applied_migrations.iter().rev().cloned().collect());
        }
        if let Some(pos) = state
            .applied_migrations
            .iter()
            .position(|n| n == target_name)
        {
            return Ok(state.applied_migrations[pos + 1..]
                .iter()
                .rev()
                .cloned()
                .collect());
        }
        if target_name == state.schema_version {
            return Ok(Vec::new());
        }
        Err(Error::Other(anyhow::anyhow!(
            "InMemoryAdapter::migrations_after: target schema_version {target_name:?} \
             is not the baseline ('0.0.0'), not a recorded migration, and not the live \
             schema ({:?}); reversing an unknown target is unsafe",
            state.schema_version
        )))
    }
```

(The error message keeps the `"not the baseline"` and `"not the live schema"` phrases that `migrations_after_errors_on_unknown_target` asserts.)

(e) Add the trait method inside the `impl MemoryAdapter for InMemoryAdapter` block, after `restore_with_guard`:

```rust
    /// The fixture reverses by name alone — `step.sql` is ignored
    /// (there is no SQL engine here). All-or-nothing is achieved by
    /// validating every step against the applied list before mutating
    /// anything.
    async fn apply_reverse_migrations(&self, steps: &[MigrationStep]) -> Result<()> {
        if steps.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        // Validate first: steps must peel the applied list newest-first.
        {
            let mut expected = state.applied_migrations.iter().rev();
            for step in steps {
                match expected.next() {
                    Some(applied) if *applied == step.name => {}
                    Some(applied) => {
                        return Err(Error::Other(anyhow::anyhow!(
                            "InMemoryAdapter::apply_reverse_migrations: step {:?} does not \
                             match the most recent unreversed migration {:?}; state unchanged",
                            step.name,
                            applied
                        )));
                    }
                    None => {
                        return Err(Error::Other(anyhow::anyhow!(
                            "InMemoryAdapter::apply_reverse_migrations: step {:?} has no \
                             corresponding applied migration; state unchanged",
                            step.name
                        )));
                    }
                }
            }
        }
        // Commit: pop the reversed migrations, re-derive the live version.
        let keep = state.applied_migrations.len() - steps.len();
        state.applied_migrations.truncate(keep);
        state.schema_version = state
            .applied_migrations
            .last()
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());
        Ok(())
    }
```

(f) Update the module doc: delete the "Schema migrations." bullet from the "What this fixture **does not** model" list (lines 13–16) and add above that list:

```rust
//! The fixture models schema migrations just enough for the daemon's
//! reverse-migration path: `apply_migration` records a forward
//! migration (the live schema version is the newest applied name, or
//! `"0.0.0"`), `migrations_after` walks that bookkeeping, and
//! `apply_reverse_migrations` pops entries all-or-nothing, ignoring
//! each step's `sql`.
```

(g) `lib.rs`: find the existing `pub use adapter::…` re-export line and add `MigrationStep` to it (so `agentic_memory::MigrationStep` resolves).

- [ ] **Step 6: Run the tests**

Run: `cargo test -p agentic-memory 2>&1 | tail -15`
Expected: PASS, including the three new tests and all pre-existing `in_memory` tests (the four `migrations_after` legacy tests must still pass — the new logic preserves their behavior for an empty applied list).

- [ ] **Step 7: Format and commit**

```bash
cargo fmt
git add crates/agentic-memory
git commit -m "agent-memory: put reverse migrations on the MemoryAdapter trait

One coarse apply_reverse_migrations(&self, steps) per audit §A9 /
issue #43 — each backend owns its atomicity, so no sqlx Transaction
crosses the async_trait boundary. Postgres wraps the moved
run_reverse body; the InMemoryAdapter fixture gains a minimal
applied-migrations model so the daemon's schema path can be exercised
without Postgres."
```

---

### Task 2: `agenticd` — retype `DaemonState.memory`, `MemoryBackendSpec` factory, call-site migration

**Files:**
- Create: `crates/agenticd/src/membackend.rs`
- Modify: `crates/agenticd/src/lib.rs` (module declaration)
- Modify: `crates/agenticd/src/server.rs:17-18,52-55,80-97`
- Modify: `crates/agenticd/src/commit.rs:146-150`
- Modify: `crates/agenticd/src/rollback/mod.rs:1-33 (docs),102-199`
- Modify: `crates/agenticd/src/migrate.rs` (delete `MigrationStep` + `run_reverse`; re-point `load_steps`)
- Modify: `crates/agentic-memory/src/postgres.rs:653-703` (privatize the two inherent methods)
- Test: `crates/agenticd/tests/reverse_migration.rs`, `crates/agenticd/tests/commit_with_memory.rs`

**Interfaces:**
- Consumes (from Task 1): `agentic_memory::MigrationStep { name, sql }`; trait method `apply_reverse_migrations(&self, steps: &[MigrationStep]) -> agentic_memory::Result<()>`; trait is `Send + Sync` with all post-`init` methods `&self`.
- Produces (Task 3 relies on): `DaemonState.memory: Option<Arc<dyn MemoryAdapter>>`; `pub enum MemoryBackendSpec { None, Postgres { url: String, tables: Vec<TrackedTable> } }` with `from_flags(postgres_url: Option<&str>, tables: Vec<TrackedTable>) -> anyhow::Result<Self>` and `async fn open(self, store: Arc<dyn ObjectStore + Send + Sync>) -> anyhow::Result<Option<Arc<dyn MemoryAdapter>>>`; `migrate::load_steps(agentic_dir, names, accept_data_loss) -> anyhow::Result<Vec<agentic_memory::MigrationStep>>` (unchanged signature apart from the step type's crate).

- [ ] **Step 1: Create `membackend.rs`**

```rust
//! Memory-backend factory for agenticd.
//!
//! Mirrors [`crate::objstore::ObjectStoreSpec`]'s parse/open split
//! (audit §S6): `DaemonState::open` derives a spec from its CLI-provided
//! configuration and delegates construction here, so future backends
//! (Mem0 / Zep / Letta, v1.1) land as new variants instead of new
//! inline construction paths.

use std::sync::Arc;

use agentic_core::ObjectStore;
use agentic_memory::postgres::{PgConfig, PostgresAdapter, TrackedTable};
use agentic_memory::MemoryAdapter;
use anyhow::Context;

/// Which memory backend to attach.
#[derive(Debug)]
pub enum MemoryBackendSpec {
    /// No memory backend; commits skip the memory-snapshot dimension.
    None,
    /// Postgres + pgvector — the v1.0 backend.
    Postgres {
        url: String,
        tables: Vec<TrackedTable>,
    },
}

impl MemoryBackendSpec {
    /// Derive the spec from the daemon's CLI configuration. Preserves
    /// the pre-factory behavior exactly: `--postgres` present requires
    /// at least one `--tables` entry; absent means no backend.
    pub fn from_flags(
        postgres_url: Option<&str>,
        tables: Vec<TrackedTable>,
    ) -> anyhow::Result<Self> {
        match postgres_url {
            None => Ok(Self::None),
            Some(url) => {
                if tables.is_empty() {
                    return Err(anyhow::anyhow!(
                        "--postgres requires at least one --tables entry"
                    ));
                }
                Ok(Self::Postgres {
                    url: url.to_string(),
                    tables,
                })
            }
        }
    }

    /// Connect and initialise the backend, returning the daemon-facing
    /// `Arc<dyn MemoryAdapter>`. `init` runs on the concrete adapter
    /// before the unsize coercion, per the trait's contract.
    pub async fn open(
        self,
        store: Arc<dyn ObjectStore + Send + Sync>,
    ) -> anyhow::Result<Option<Arc<dyn MemoryAdapter>>> {
        match self {
            Self::None => Ok(None),
            Self::Postgres { url, tables } => {
                let cfg = PgConfig::new(&url, tables);
                let mut adapter = PostgresAdapter::connect(cfg, store)
                    .await
                    .context("connecting Postgres memory backend")?;
                adapter
                    .init()
                    .await
                    .context("initialising memory backend")?;
                tracing::info!(
                    logical_decoding = adapter.logical_decoding_available(),
                    "memory backend attached"
                );
                Ok(Some(Arc::new(adapter)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_flags_none_when_no_url() {
        assert!(matches!(
            MemoryBackendSpec::from_flags(None, Vec::new()).unwrap(),
            MemoryBackendSpec::None
        ));
    }

    #[test]
    fn from_flags_requires_tables_with_url() {
        let err = MemoryBackendSpec::from_flags(Some("postgres://x"), Vec::new()).unwrap_err();
        assert!(
            err.to_string().contains("--tables"),
            "must keep the pre-factory error message; got: {err}"
        );
    }
}
```

Note: if `PgConfig::new`'s first parameter is a `String`/`impl Into<String>` rather than `&str`, adjust the call (`PgConfig::new(&url, tables)` vs `PgConfig::new(url, tables)`) to match — `server.rs:88` shows the current usage to copy.

Declare the module: in `crates/agenticd/src/lib.rs`, add `pub mod membackend;` alongside the existing `pub mod objstore;` line.

- [ ] **Step 2: Retype `server.rs`**

(a) Line 17: keep only what's still used — `use agentic_memory::postgres::TrackedTable;` (drop `PgConfig`, `PostgresAdapter`). Line 18 (`use agentic_memory::MemoryAdapter;`) stays. Add `use crate::membackend::MemoryBackendSpec;`.

(b) Field (lines 52–55) becomes:

```rust
    /// Optional memory backend. When present, every commit takes a memory
    /// snapshot under the commit lock and threads its manifest hash into
    /// the Commit's `memory_snapshot` dimension. No adapter-level mutex:
    /// write-path exclusivity comes from `commit_lock` (one commit at a
    /// time, ADR-0001), and adapters are internally `&self`-safe
    /// (audit §C9 / §A9).
    pub memory: Option<Arc<dyn MemoryAdapter>>,
```

(c) The construction block (lines 80–97) becomes:

```rust
        let memory = MemoryBackendSpec::from_flags(postgres_url, tables)?
            .open(store.clone())
            .await?;
```

(`tables` is moved into the spec; nothing after this uses it.)

- [ ] **Step 3: Update `commit.rs::snapshot_memory`** (lines 146–150) to:

```rust
    let Some(adapter) = state.memory.as_ref().map(Arc::clone) else {
        return Ok((None, None));
    };
    let handle = adapter.snapshot().await.context("taking memory snapshot")?;
```

(The `memory.lock_owned().await` line is deleted; the rest of the function is unchanged.)

- [ ] **Step 4: Rewrite `rollback/mod.rs`'s schema/memory section** (lines 102–199) to:

```rust
    // -- Schema migrations + memory restore ----------------------------------
    // No adapter lock: `Arc<dyn MemoryAdapter>` methods take `&self`, and
    // exclusivity against concurrent commits comes from the daemon's
    // commit_lock (audit §C9 / §A9).
    if let Some(ref target_schema) = target.schema_version {
        let adapter: std::sync::Arc<dyn MemoryAdapter> =
            std::sync::Arc::clone(state.memory.as_ref().ok_or_else(|| {
                anyhow!("target commit has a schema_version but no memory backend is attached")
            })?);

        // Phase 1: query the backend for the live schema version and the
        // pending migration names.
        //
        // NOTE: the live-vs-target comparison here is a planning step
        // (decides whether migrations are needed and how many), not a
        // duplicate of the gate that `restore_with_guard` performs
        // against the post-migration live state (audit §S5). The
        // reverse-migration sequence is atomic inside the backend
        // (apply_reverse_migrations), so partial failures don't leave
        // intermediate live versions.
        let live_schema = adapter
            .current_schema_version()
            .await
            .context("reading live schema version")?;
        let migration_names = if live_schema != *target_schema {
            adapter
                .migrations_after(target_schema)
                .await
                .context("querying pending reverse migrations")?
        } else {
            Vec::new()
        };

        if live_schema != *target_schema {
            plan.push(format!(
                "reverse schema migrations: {live_schema} → {target_schema}"
            ));
            // Phase 2: synchronous filesystem I/O — no adapter call in
            // flight. `accept_data_loss` is forwarded so
            // `check_irreversible` can honor the operator's opt-in for
            // IRREVERSIBLE-marked migrations.
            let steps = migrate::load_steps(
                state.refs.agentic_dir(),
                &migration_names,
                args.accept_data_loss,
            )
            .context("loading reverse migration files")?;

            // Phase 3: execute — atomic inside the backend.
            if !args.dry_run {
                adapter
                    .apply_reverse_migrations(&steps)
                    .await
                    .context("running reverse migrations")?;
            }
        } else {
            plan.push(format!(
                "schema already at {target_schema} — no migrations needed"
            ));
        }

        // -- Memory ----------------------------------------------------------
        if let Some(manifest_hash) = target.memory_snapshot {
            plan.push(format!(
                "restore memory from manifest {}",
                manifest_hash.short()
            ));
            if !args.dry_run {
                let manifest = load_manifest(&state, &manifest_hash)?;
                let handle = SnapshotHandle {
                    manifest,
                    schema_version: target_schema.clone(),
                };
                // Pause the backend's data capture for the restore
                // window, then call the guard-taking restore method so
                // the quiesce discipline is visible at the call site.
                // The capture resumes when `guard` is dropped.
                // Audit anchor: §A1 / [R1] — without this the demo's
                // atomicity claim is silently false.
                let guard = adapter
                    .begin_restore()
                    .await
                    .context("pausing data capture for restore window")?;
                adapter
                    .restore_with_guard(&guard, &handle)
                    .await
                    .context("restoring memory snapshot")?;
                drop(guard);
            }
        } else {
            plan.push("no memory snapshot in target — skipping memory data restore".into());
        }
    } else {
        plan.push(
            "no schema_version in target — skipping schema migration and memory restore".into(),
        );
    }
```

Also update the module header: in the doc comment, step 2 becomes "run reverse SQL migrations via `MemoryAdapter::apply_reverse_migrations` (steps loaded by `crate::migrate::load_steps`)" and step 3's "hand it to `PostgresAdapter::restore_with_guard`" becomes "hand it to the adapter's `restore_with_guard`".

- [ ] **Step 5: Slim `migrate.rs`**

(a) Delete the `MigrationStep` struct (lines 33–44) and `run_reverse` (lines 104–139). Delete `use agentic_memory::postgres::PostgresAdapter;` (line 30). Add `use agentic_memory::MigrationStep;`.

(b) In `load_steps`, the push (lines 94–98) becomes:

```rust
        steps.push(MigrationStep {
            name: name.clone(),
            sql,
        });
```

(The `path` field is gone — it was `#[allow(dead_code)]` and never read; the error messages before construction already carry the path.)

(c) Module doc: replace the sentence fragment "pre-read so `run_reverse` doesn't need filesystem access" (line 42's doc lived on the struct — now deleted) and update the module header's first paragraph to say execution happens in `MemoryAdapter::apply_reverse_migrations`; this module only *plans* (loads and validates files). The `load_steps` doc note "callers can release the `MutexGuard<PostgresAdapter>`" becomes "callers run this between adapter calls — it does blocking filesystem I/O".

(d) `migrate.rs`'s own `#[cfg(test)]` tests compile unchanged except `load_steps_returns_steps_in_given_order`, which references only `name`/`sql` — no edit needed. If any test references `.path`, delete that assertion.

- [ ] **Step 6: Privatize the Postgres inherent methods**

In `crates/agentic-memory/src/postgres.rs` (lines 653–703): change `pub async fn begin_reverse_tx` and `pub async fn apply_down_migration_tx` to `async fn` (drop `pub`). Replace the "Not a trait method in v1.0: …" doc paragraph (lines 660–665) with:

```rust
    /// Internal helper for the trait's `apply_reverse_migrations` — the
    /// transaction stays inside this impl because sqlx 0.8's
    /// `Executor<'c>` HRTBs can't cross the async_trait boundary.
```

- [ ] **Step 7: Update the two integration test files**

(a) `crates/agenticd/tests/reverse_migration.rs`:
- Line 29: `use agentic_memory::postgres::{PgConfig, PostgresAdapter};` stays (the adapter is still constructed concretely); ADD `use agentic_memory::MemoryAdapter;` so the trait method resolves.
- Replace all three call sites (lines 174, 241, 343): `migrate::run_reverse(&adapter, steps)` → `adapter.apply_reverse_migrations(&steps)`. Where `steps` was consumed by value, it's now borrowed — no other change needed.
- Update line 155's comment and line 345's expect-message text mentioning `run_reverse` to say `apply_reverse_migrations`.
- The tests' assertions (outer-transaction rollback on failure, bookkeeping deletes, IRREVERSIBLE bypass) must remain untouched — they now pin the trait method's atomicity contract.

(b) `crates/agenticd/tests/commit_with_memory.rs`:
- Line 117: `memory: Some(Arc::new(Mutex::new(adapter)))` → `memory: Some(Arc::new(adapter))`. Keep the existing `adapter.init()` call that precedes it (init must run before the coercion). Remove the now-unused `Mutex` import if it has no other use in the file (check `commit_lock: Arc::new(Mutex::new(()))` in the same constructor — if present, the import stays).

- [ ] **Step 8: Build, test, format**

Run: `cargo build --workspace 2>&1 | tail -5`
Expected: success (no `run_reverse`/`Mutex<PostgresAdapter>` stragglers — if the compiler finds one this plan missed, fix it the same way as its siblings above).

Run: `cargo test -p agenticd --lib 2>&1 | tail -10` and `cargo test -p agentic-memory 2>&1 | tail -5`
Expected: PASS.

Postgres integration tests: `docker compose -f tests/fixtures/pg.yml up -d`, then
`cargo test -p agenticd --test reverse_migration --test commit_with_memory 2>&1 | tail -15`
Expected: PASS. (No Docker → `cargo test -p agenticd --no-run` and report the skip explicitly.)

`cargo fmt`

- [ ] **Step 9: Commit**

```bash
git add crates/agenticd crates/agentic-memory
git commit -m "agenticd: retype DaemonState.memory to Arc<dyn MemoryAdapter>

Drops the adapter-level mutex (commit_lock already serialises the
write path per ADR-0001; adapters are &self-safe — audit §C9), routes
rollback's reverse migrations through the trait, and adds the
MemoryBackendSpec factory mirroring ObjectStoreSpec (audit §S6). The
Postgres begin_reverse_tx/apply_down_migration_tx helpers go private —
the trait is the only reverse-migration surface. Closes the remaining
type-level blocker for non-Postgres backends (§A9, issue #43)."
```

---

### Task 3: Rollback integration test against `InMemoryAdapter`

**Files:**
- Create: `crates/agenticd/tests/rollback_in_memory.rs`
- Reference (read first, copy its `DaemonState` construction pattern): `crates/agenticd/tests/commit_with_memory.rs`

**Interfaces:**
- Consumes: `DaemonState { memory: Option<Arc<dyn MemoryAdapter>>, … }` (Task 2); `InMemoryAdapter::{new, apply_migration, insert_rows, rows_of}` and trait methods (Task 1); `rollback::{execute, RollbackArgs}`; `commit::execute` (or the same commit entry point `commit_with_memory.rs` uses).

This is acceptance criterion 3: a second backend passes the same rollback path — schema reversal via `migrations_after` → `load_steps` → `apply_reverse_migrations`, memory restore via `begin_restore` → `restore_with_guard`, and forward-record. No Postgres, no Docker.

- [ ] **Step 1: Read `commit_with_memory.rs`** and note (a) how it builds `DaemonState` (field by field — `repo_root`, `store`, `refs`, `commit_lock`, `shutdown`, `memory`, `mcp_servers`, `http`, `peer_auth`), (b) how it drives a commit and what the commit input struct is called, (c) any helper fns worth copying verbatim (temp repo dirs, refs bootstrap).

- [ ] **Step 2: Write the test.** Shape (adapt constructor details to what Step 1 found — same fields, same helpers; the *scenario* below is the requirement):

```rust
//! Rollback end-to-end against the InMemoryAdapter — the trait-level
//! proof that the daemon's schema + memory rollback path works for a
//! backend that isn't Postgres (issue #43 acceptance criterion 3).

use std::sync::Arc;

use agentic_memory::in_memory::InMemoryAdapter;
use agentic_memory::MemoryAdapter;
use agenticd::rollback::{self, RollbackArgs};
// … plus the DaemonState/commit imports commit_with_memory.rs uses.

#[tokio::test]
async fn rollback_reverses_schema_and_restores_memory_in_memory_backend() {
    // -- Arrange: adapter at schema 001 with clean rows, committed. ------
    // (store, repo dirs, refs, state construction copied from
    // commit_with_memory.rs, with `memory: Some(Arc::new(adapter_arc))`
    // where adapter_arc is an InMemoryAdapter kept cloneable for
    // assertions — construct as `Arc<InMemoryAdapter>` first, keep a
    // clone, and coerce the other into Arc<dyn MemoryAdapter>.)
    let adapter = Arc::new(InMemoryAdapter::new(store.clone()));
    adapter.apply_migration("001_init").await;
    adapter
        .insert_rows("messages", vec![serde_json::json!({"id": 1, "body": "clean"})])
        .await;
    // NOTE: InMemoryAdapter::init is a no-op, so no init call is needed
    // before the coercion.
    let state = /* DaemonState with memory: Some(adapter.clone() as Arc<dyn MemoryAdapter>) */;

    // Commit the baseline (schema_version "001_init", memory snapshot).
    let baseline = /* drive the same commit path commit_with_memory.rs uses */;

    // -- Act 1: contaminate — bump schema and dirty the data. ------------
    adapter.apply_migration("002_bump").await;
    adapter
        .insert_rows("messages", vec![serde_json::json!({"id": 99, "body": "bad"})])
        .await;

    // The reverse-migration loader reads <agentic_dir>/schema/002_bump.down.sql;
    // the fixture ignores SQL, but the file must exist and pass the
    // IRREVERSIBLE check.
    let schema_dir = agentic_dir.join("schema");
    std::fs::create_dir_all(&schema_dir).unwrap();
    std::fs::write(schema_dir.join("002_bump.down.sql"), "-- no-op for fixture\n").unwrap();

    // -- Act 2: roll back to the baseline commit. -------------------------
    let out = rollback::execute(
        Arc::clone(&state),
        RollbackArgs {
            target: baseline_ref, // however commit_with_memory.rs names/refs its commit
            dry_run: false,
            accept_data_loss: false,
            repo: repo_root.clone(),
        },
        None,
    )
    .await
    .expect("rollback against InMemoryAdapter should succeed");

    // -- Assert: all three dimensions came back. --------------------------
    assert!(out.executed);
    assert_eq!(
        adapter.current_schema_version().await.unwrap(),
        "001_init",
        "schema must be reversed to the target's version"
    );
    let rows = adapter.rows_of("messages").await;
    assert_eq!(rows.len(), 1, "contaminated row must be gone");
    assert_eq!(rows[0]["body"], "clean");
    assert!(out.new_head_hash.is_some(), "rollback forward-records a commit");
}
```

The commented placeholders above are **construction details to copy from `commit_with_memory.rs`**, not design freedom: same store/refs/dirs bootstrap, same commit entry point, `peer_uid: None`. Everything asserted must stay as written.

- [ ] **Step 3: Run it**

Run: `cargo test -p agenticd --test rollback_in_memory 2>&1 | tail -15`
Expected: PASS, with no Docker/Postgres running (that's the point). If it fails on rollback's prompts phase (no `prompts` in the commit → the phase is skipped by design), check the baseline commit shape against what `commit_with_memory.rs` produces.

- [ ] **Step 4: Format and commit**

```bash
cargo fmt
git add crates/agenticd/tests/rollback_in_memory.rs
git commit -m "agenticd: rollback integration test against the in-memory backend

Issue #43 acceptance criterion 3: a non-Postgres MemoryAdapter drives
the full rollback path — reverse migrations through the trait, memory
restore under the guard, forward-record — with no Docker required."
```

---

### Task 4: Docs, audit annotations, final gates

**Files:**
- Modify: `docs/ops/2026-05-21-agenticd-architectural-analysis.md` (§A9, §S1, §S4, §S6, R10 row)

**Interfaces:**
- Consumes: everything landed in Tasks 1–3.

- [ ] **Step 1: Annotate the audit doc.** Match the existing DONE-annotation style used by §A1/§A10 (read them first):

  - **§A9**: change its status marker from PARTIAL to **DONE 2026-07-09** and replace the "Still pending under the same issue…" sentence with: "Completed 2026-07-09 (issue #43): `apply_reverse_migrations` is on the trait (coarse, backend-owned atomicity — the sqlx HRTB blocker never crosses the boundary), `DaemonState.memory` is `Option<Arc<dyn MemoryAdapter>>` with no adapter mutex (write-path exclusivity via `commit_lock`, resolving §C9's over-serialization), and `MemoryBackendSpec` mirrors `ObjectStoreSpec` (§S6). The `InMemoryAdapter` fixture models migrations and passes the daemon's rollback path end-to-end (`crates/agenticd/tests/rollback_in_memory.rs`)."
  - **§S1**: append "— resolved 2026-07-09 by §A9's completion (issue #43)."
  - **§S4**: append "— resolved 2026-05-22 by §A10 (issue #44); this entry was left stale when A10 landed." (fixing the doc's internal inconsistency).
  - **§S6**: append "— resolved 2026-07-09 by `MemoryBackendSpec` (§A9 / issue #43)."
  - **R10 row**: append "(resolved 2026-07-09 — §A9 complete)" in its notes/impact cell, matching however other resolved rows are annotated (read a neighboring resolved row first; if none exists, add the note in the row's last cell).

- [ ] **Step 2: Full workspace gates**

Run, expecting all to pass:
```bash
cargo test --workspace 2>&1 | tail -10
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5
cargo fmt --check
```
(Postgres-backed tests need the pg fixture up, as in Task 2 Step 8. No Docker → run the non-PG subset plus `--no-run` compiles, and report exactly which suites were skipped.)

- [ ] **Step 3: Commit**

```bash
git add docs/ops/2026-05-21-agenticd-architectural-analysis.md
git commit -m "docs: mark audit §A9 done — MemoryAdapter trait complete (#43)

Also fixes the stale §S4 entry that §A10's landing (issue #44) left
behind, and notes §S1/§S6/R10 as resolved."
```

---

## Execution notes

- One PR for the whole branch: "Complete the MemoryAdapter trait (#43)". PR body explains the *why* (type-level unblock for v1.1 backends; C9 over-serialization; S6 factory) and states "Closes #43".
- Do not deploy, publish, or touch the website; this is daemon/library work only.
