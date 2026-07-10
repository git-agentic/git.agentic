# Issue #100: Per-Test Database Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `cargo test -p agentic-memory --test integration -- --ignored` pass at default test parallelism by giving each test its own Postgres database.

**Architecture:** A `TestDb` helper inside the test file provisions `agentic_test_<nanos>_<tag>` as a *database* per test (CREATE DATABASE serialized through a static mutex, `CREATE EXTENSION vector` installed, `DROP DATABASE … WITH (FORCE)` on teardown). Every test's connections point at its own database, which isolates the two database-scoped shared resources that cause the flakes: `public.agentic_change_log` and the snapshot advisory-lock key. Zero product-code changes; all assertions unchanged.

**Tech Stack:** Rust, tokio, sqlx 0.8, Postgres 16 + pgvector (fixture `tests/fixtures/pg.yml`, port 54321).

**Spec:** `docs/superpowers/specs/2026-07-10-issue-100-per-test-db-isolation-design.md`

## Global Constraints

- Work in the worktree `.worktrees/issue-100-per-test-db/` (branch `issue-100-per-test-db`). Never edit the main checkout.
- No product-code changes: the only Rust file touched is `crates/agentic-memory/tests/integration.rs`.
- No weakened test semantics: same assertions, no new `#[ignore]`, no serialization workaround (acceptance criterion 2 of issue #100).
- No `unwrap()` without a `// SAFETY:`/`// INVARIANT:` comment applies to non-test code only — test code may `unwrap()`/`expect()` freely (existing suite style).
- Gates before finishing: `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` must pass.
- The Postgres fixture must be running for every verification step:

```bash
podman compose -f tests/fixtures/pg.yml up -d   # or: docker compose -f tests/fixtures/pg.yml up -d
export DATABASE_URL=postgres://agentic:agentic@localhost:54321/agentic
```

---

### Task 1: `TestDb` helper + first converted test

**Files:**
- Modify: `crates/agentic-memory/tests/integration.rs` (helpers live near the existing `make_schema`/`drop_schema`/`fresh_schema_name` block, ~lines 37–86; first conversion is `bootstrap_produces_a_deterministic_manifest`, ~lines 88–134)

**Interfaces:**
- Consumes: existing helpers `database_url()`, `make_schema`, `drop_schema`, `fresh_schema_name`, `schema_scoped_url` — all unchanged.
- Produces (used verbatim by Task 2's conversions):
  - `struct TestDb { name: String, url: String, maint_pool: PgPool }`
  - `TestDb::create(tag: &str) -> Option<TestDb>` (async; `None` when `DATABASE_URL` unset)
  - `TestDb::drop(self)` (async; best-effort teardown)
  - `fn url_with_database(base: &str, db: &str) -> String`

- [ ] **Step 1: Reproduce the failure (red)**

```bash
cargo test -p agentic-memory --test integration -- --ignored
```

Expected: FAIL — roughly 5–7 tests fail with a varying mix of untracked-table strict-drain errors, wrong row counts, and blocked snapshots. Record the failing set in the task report. (If this run is green by luck, run it once more; the point is documenting the flake we're fixing.)

- [ ] **Step 2: Add the helper block**

Insert after the existing `schema_scoped_url` function (after ~line 86):

```rust
/// Serializes `CREATE DATABASE` statements across parallel tests.
/// Postgres can reject concurrent creates from the same template
/// ("source database is being accessed by other users"); the guard is
/// held only for the CREATE, so tests themselves still run in parallel.
static CREATE_DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Swap the database path segment of a Postgres URL. Expects the URL
/// to carry a database name (both documented fixture URLs do).
fn url_with_database(base: &str, db: &str) -> String {
    let (core, query) = match base.split_once('?') {
        Some((c, q)) => (c, Some(q)),
        None => (base, None),
    };
    let idx = core
        .rfind('/')
        .expect("DATABASE_URL must include a database path");
    let mut out = format!("{}/{db}", &core[..idx]);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out
}

/// A Postgres database owned by exactly one test.
///
/// The suite's shared-state flakiness (issue #100) came from resources
/// that are *database*-scoped, not schema-scoped: the trigger capture
/// log is pinned to `public.agentic_change_log` by product design, and
/// the snapshot advisory-lock key is a constant whose lock space is
/// per-database. One database per test isolates both without touching
/// product code.
struct TestDb {
    name: String,
    /// Connection URL for this test's database. Feed through
    /// `schema_scoped_url` exactly like the old shared URL.
    url: String,
    /// Pool on the *maintenance* database (the original DATABASE_URL),
    /// kept for teardown — a session can't drop the database it is
    /// connected to.
    maint_pool: PgPool,
}

impl TestDb {
    /// `None` when DATABASE_URL is unset — callers keep the suite's
    /// existing graceful-skip behavior.
    async fn create(tag: &str) -> Option<TestDb> {
        let base = database_url()?;
        let maint_pool = PgPool::connect(&base)
            .await
            .expect("connect maintenance pool (is the pg.yml fixture up?)");

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("agentic_test_{nanos}_{tag}");
        {
            let _guard = CREATE_DB_LOCK.lock().await;
            maint_pool
                .execute(format!("CREATE DATABASE \"{name}\"").as_str())
                .await
                .expect("create per-test database");
        }

        let url = url_with_database(&base, &name);
        // The adapter's init() validates pgvector but deliberately never
        // installs it (needs superuser); the fixture's `agentic` user is
        // the container superuser, so install it here — same division of
        // labor as CI's "create pgvector extension" step.
        let setup = PgPool::connect(&url)
            .await
            .expect("connect per-test database");
        setup
            .execute("CREATE EXTENSION IF NOT EXISTS vector")
            .await
            .expect("create vector extension in per-test database");
        setup.close().await;

        Some(TestDb {
            name,
            url,
            maint_pool,
        })
    }

    /// Best-effort teardown, mirroring `drop_schema`: a panicking test
    /// leaks its database exactly as it leaks its schema today. FORCE
    /// (pg13+) terminates the adapter's still-open pool/poller sessions
    /// so the drop can't hang on them.
    async fn drop(self) {
        let _ = self
            .maint_pool
            .execute(format!("DROP DATABASE \"{}\" WITH (FORCE)", self.name).as_str())
            .await;
        self.maint_pool.close().await;
    }
}
```

- [ ] **Step 3: Convert `bootstrap_produces_a_deterministic_manifest`**

The conversion pattern (identical for every test in Task 2). Replace:

```rust
    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };
```

with:

```rust
    let Some(db) = TestDb::create("bootstrap").await else {
        eprintln!("DATABASE_URL not set — skipping integration test");
        return;
    };
    let url = db.url.clone();
```

and append `db.drop().await;` as the final statement of the test (after the existing `drop_schema(&admin_pool, &schema).await;`). Nothing else in the body changes — `admin_pool` now connects to the test database because `url` now points there.

- [ ] **Step 4: Run the converted test (green)**

```bash
cargo test -p agentic-memory --test integration -- --ignored bootstrap_produces_a_deterministic_manifest --nocapture
```

Expected: PASS (`test result: ok. 1 passed`). Then confirm no database leaked:

```bash
psql "$DATABASE_URL" -tc "SELECT datname FROM pg_database WHERE datname LIKE 'agentic_test_%'"
```

Expected: empty output.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p agentic-memory --all-targets -- -D warnings
git add crates/agentic-memory/tests/integration.rs
git commit -m "Add per-test database helper to the memory integration suite (issue #100)"
```

---

### Task 2: Convert the remaining 16 tests + module doc

**Files:**
- Modify: `crates/agentic-memory/tests/integration.rs` (module doc lines 1–27; every remaining `#[tokio::test]`)

**Interfaces:**
- Consumes: `TestDb::create(tag)`, `TestDb::drop()`, exactly as defined in Task 1.
- Produces: the fully parallel-safe suite Task 3's CI change relies on.

- [ ] **Step 1: Apply the Task 1 conversion pattern to every remaining test**

For each test below: swap the `let url = match database_url() {…}` block for the `let Some(db) = TestDb::create("<tag>").await else {…}` + `let url = db.url.clone();` pattern (code shown verbatim in Task 1 Step 3), and add `db.drop().await;` as the last statement. Tags:

| Test | Tag |
|---|---|
| `install_helpers_is_idempotent` | `idem` |
| `snapshot_serialises_through_advisory_lock` | `lock` |
| `ac1_writes_during_restore_are_reverted` | `ac1` |
| `pg_snapshot_perf_smoke` | `perf` |
| `init_rejects_null_primary_key` | `nullpk` |
| `init_rejects_non_finite_float` | `nan` |
| `init_rejects_absent_primary_key_column` | `absentpk` |
| `init_accepts_finite_floats_and_nullable_non_pk_columns` | `floats` |
| `init_rejects_positive_and_negative_infinity` | `inf` |
| `restore_rejects_delete_envelope_with_null_pk` | `delnull` |
| `restore_rejects_delete_envelope_with_absent_pk` | `delabsent` |
| `snapshot_strict_drain_preserves_bad_row_on_block` | `strictdrain` |
| `schema_qualified_tracked_table_routes_events` | `qualified` |
| `restore_handles_duplicate_pk_within_a_batch` | `duppk` |
| `restore_rejects_empty_delete_envelope` | `emptydel` |
| `restore_preserves_order_across_mode_transitions` | `modes` |

Special cases (everything else is the uniform pattern):

- `install_helpers_is_idempotent` has no schema and no `admin_pool`; it currently passes `url` straight into `PgConfig::new(url, Vec::new())`. Convert to `PgConfig::new(db.url.clone(), Vec::new())` and end with `db.drop().await;`. No other change.
- `snapshot_serialises_through_advisory_lock`: no extra edits beyond the pattern, but this is the test the URL swap matters most for — the lock-holder `admin_pool` must live in the same database as the adapter for `pg_advisory_lock` to conflict. The pattern achieves that because `admin_pool` connects via `url`.
- `init_rejects_positive_and_negative_infinity` loops over two fixtures, creating a pool + schema per iteration. Create ONE `TestDb` before the loop (`TestDb::create("inf")`), use `db.url.clone()` inside the loop where `url` was used, and call `db.drop().await;` once after the loop. The two iterations run sequentially in one database — same as today's behavior in the shared database.
- `snapshot_strict_drain_preserves_bad_row_on_block` ends with a trailing `DELETE FROM public.agentic_change_log …` cleanup after `drop_schema`. Keep it (harmless, minimal diff); `db.drop().await;` goes after it.
- `pg_snapshot_perf_smoke`: uniform pattern with tag `perf`; `BENCH_ROWS` handling unchanged.
- Both explicit `TRUNCATE public.agentic_change_log` fixture statements (strict-drain and `schema_qualified_tracked_table_routes_events`) stay exactly as they are — now scoped to the test's own database.

- [ ] **Step 2: Replace the module doc header (lines 1–27)**

```rust
//! End-to-end integration tests for the Postgres backend.
//!
//! These tests require a real Postgres + pgvector instance and are gated
//! by `#[ignore]` so they don't block the default `cargo test` run on
//! laptops without a container runtime. To run with podman:
//!
//! ```bash
//! podman compose -f tests/fixtures/pg.yml up -d
//! DATABASE_URL=postgres://agentic:agentic@localhost:54321/agentic \
//!   cargo test -p agentic-memory --test integration -- --ignored
//! ```
//!
//! Or with the demo's Postgres on port 54322:
//!
//! ```bash
//! docker compose -f examples/langgraph-rollback/docker-compose.yml up -d
//! DATABASE_URL=postgres://agentic:agentic@localhost:54322/agentic \
//!   cargo test -p agentic-memory --test integration -- --ignored
//! ```
//!
//! Every test provisions its own *database* (`agentic_test_<nanos>_<tag>`,
//! see `TestDb`) and drops it on teardown, so the suite is safe at default
//! test parallelism (issue #100). Databases — not schemas — are the
//! isolation unit because two shared resources are database-scoped by
//! product design: `public.agentic_change_log` (one daemon = one database
//! = one log) and the constant snapshot advisory-lock key. The connecting
//! user must be allowed to CREATE DATABASE / CREATE EXTENSION (the fixture
//! user is the container superuser). Within its database each test still
//! creates a scratch schema for its tracked tables.
```

(Note: the old header's `docker exec … CREATE EXTENSION` line for the demo database is no longer needed — `TestDb::create` installs the extension in every per-test database.)

- [ ] **Step 3: Run the full suite at default parallelism**

```bash
cargo test -p agentic-memory --test integration -- --ignored
```

Expected: PASS — `test result: ok. 17 passed; 0 failed`. Then confirm no leaked databases:

```bash
psql "$DATABASE_URL" -tc "SELECT datname FROM pg_database WHERE datname LIKE 'agentic_test_%'"
```

Expected: empty output.

- [ ] **Step 4: Format, lint, commit**

```bash
cargo fmt
cargo clippy -p agentic-memory --all-targets -- -D warnings
git add crates/agentic-memory/tests/integration.rs
git commit -m "Isolate every memory integration test in its own database (issue #100)"
```

---

### Task 3: Drop the CI `--test-threads=1` workaround

**Files:**
- Modify: `.github/workflows/ci.yml` (postgres job, "memory adapter suite" step, ~lines 153–159)

**Interfaces:**
- Consumes: the parallel-safe suite from Task 2.
- Produces: CI as the standing regression guard for acceptance criterion 1.

- [ ] **Step 1: Replace the step**

Old:

```yaml
      - name: memory adapter suite (snapshot/restore)
        # --test-threads=1 is required by the suite's own contract (see
        # its module doc): public.agentic_change_log is shared across
        # test schemas by design, so parallel tests interfere with each
        # other's trigger events. Verified: parallel runs fail two
        # tests; single-threaded is green.
        run: cargo test -p agentic-memory --test integration -- --ignored --nocapture --test-threads=1
```

New:

```yaml
      - name: memory adapter suite (snapshot/restore)
        # Runs at default parallelism: each test provisions its own
        # database (issue #100), so the suite no longer needs
        # --test-threads=1. Keeping it parallel here is deliberate —
        # CI is the regression guard for the isolation.
        run: cargo test -p agentic-memory --test integration -- --ignored --nocapture
```

Leave the job's `create pgvector extension` step alone — the fixture database keeps its extension; per-test databases install their own.

- [ ] **Step 2: Sanity-check the YAML and commit**

```bash
python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('yaml ok')"
git add .github/workflows/ci.yml
git commit -m "Run the memory integration suite at default parallelism in CI (issue #100)"
```

Expected: `yaml ok`.

---

### Task 4: Acceptance verification + OpenWolf bookkeeping

**Files:**
- Modify: `.wolf/buglog.json` (bug-125, bug-128 — in the MAIN checkout, `/Users/tonibergholm/Developer/github/git.agentic/.wolf/`, since `.wolf/` is session state, not branch content)
- Modify: `.wolf/cerebrum.md`, `.wolf/memory.md`, `.wolf/anatomy.md` (main checkout)

**Interfaces:**
- Consumes: the finished suite + CI change.
- Produces: the evidence for the issue's acceptance criteria, recorded for the PR description.

- [ ] **Step 1: Acceptance criterion 1 — three consecutive parallel runs**

```bash
for i in 1 2 3; do
  echo "=== run $i ==="
  cargo test -p agentic-memory --test integration -- --ignored || exit 1
done
```

Expected: three × `test result: ok. 17 passed; 0 failed`.

- [ ] **Step 2: Acceptance criterion 3 — single-threaded still green**

```bash
cargo test -p agentic-memory --test integration -- --ignored --test-threads=1
```

Expected: `test result: ok. 17 passed; 0 failed`.

- [ ] **Step 3: Workspace gates**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

Expected: both clean.

- [ ] **Step 4: Update OpenWolf records (main checkout)**

In `.wolf/buglog.json`: update bug-125 and bug-128 — append to each `fix` field: `"FIXED by issue #100 branch issue-100-per-test-db: per-test CREATE DATABASE isolation (change_log + advisory locks are database-scoped); CI --test-threads=1 removed."`, bump `last_seen` to today.

In `.wolf/cerebrum.md` under `## Key Learnings`, add:

```markdown
- agentic-memory integration tests: per-test SCHEMA isolation is insufficient — `public.agentic_change_log` (pinned to public by design) and advisory-lock keys are DATABASE-scoped. Per-test CREATE DATABASE is the pattern (integration.rs `TestDb`, issue #100).
```

Append the session line to `.wolf/memory.md` and add the plan file entry to `.wolf/anatomy.md`.

- [ ] **Step 5: Commit the plan file (worktree) — bookkeeping files live outside the branch**

```bash
git add docs/superpowers/plans/2026-07-10-issue-100-per-test-db-isolation.md
git commit -m "Add implementation plan for issue #100 per-test database isolation"
```

---

## Done means

- Three consecutive default-parallelism runs green (criterion 1) — evidence in Task 4 Step 1.
- Assertions untouched; no new `#[ignore]`; no serialization workaround (criterion 2) — the diff shows only the skip-guard/URL/teardown pattern, helper block, and module doc.
- `--test-threads=1` green (criterion 3) — Task 4 Step 2.
- CI runs the suite in parallel.
- Branch ready for PR referencing issue #100 (fixes bug-125 / bug-128).
