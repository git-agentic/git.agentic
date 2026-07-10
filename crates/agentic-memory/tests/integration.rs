//! End-to-end integration tests for the Postgres backend.
//!
//! These tests require a real Postgres + pgvector instance and are gated
//! by `#[ignore]` so they don't block the default `cargo test` run on
//! laptops without a container runtime. To run with podman:
//!
//! ```bash
//! podman compose -f tests/fixtures/pg.yml up -d
//! DATABASE_URL=postgres://agentic:agentic@localhost:54321/agentic \
//!   cargo test -p agentic-memory --test integration -- --ignored --test-threads=1
//! ```
//!
//! Or with the demo's Postgres on port 54322:
//!
//! ```bash
//! docker compose -f examples/langgraph-rollback/docker-compose.yml up -d
//! docker exec agentic-demo-pg psql -U agentic -d agentic -c "CREATE EXTENSION IF NOT EXISTS vector"
//! DATABASE_URL=postgres://agentic:agentic@localhost:54322/agentic \
//!   cargo test -p agentic-memory --test integration -- --ignored --test-threads=1
//! ```
//!
//! Every test allocates its own schema (`agentic_test_<nanos>`) so user
//! data is isolated. **Run with `--test-threads=1`**: `public.agentic_change_log`
//! is shared across schemas by design (one daemon = one database = one
//! log), so concurrent tests interfere with each other's trigger events
//! and with restore's TRUNCATE-of-change_log behavior (audit §A1 / issue
//! #35). Each test drops its schema on teardown.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agentic_core::FsObjectStore;
use agentic_memory::postgres::{PgConfig, PostgresAdapter, TrackedTable};
use agentic_memory::MemoryAdapter;
use sqlx::{Executor, PgPool};

fn database_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

async fn make_schema(pool: &PgPool, schema: &str) -> sqlx::Result<()> {
    pool.execute(format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\"").as_str())
        .await?;
    pool.execute(
        format!(
            r#"
            CREATE TABLE "{schema}".episodes (
                id    bigint PRIMARY KEY,
                text  text   NOT NULL
            );
            "#
        )
        .as_str(),
    )
    .await?;
    pool.execute(
        format!(
            "INSERT INTO \"{schema}\".episodes (id, text) \
             VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e')"
        )
        .as_str(),
    )
    .await?;
    Ok(())
}

async fn drop_schema(pool: &PgPool, schema: &str) {
    let _ = pool
        .execute(format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE").as_str())
        .await;
}

fn fresh_schema_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("agentic_test_{nanos}")
}

/// Build a URL with `search_path` pointing at our temp schema so adapter
/// SQL like `SELECT * FROM "episodes"` resolves correctly.
fn schema_scoped_url(base: &str, schema: &str) -> String {
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-csearch_path%3D{schema}%2Cpublic")
}

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

#[tokio::test]
#[ignore]
async fn bootstrap_produces_a_deterministic_manifest() {
    let Some(db) = TestDb::create("bootstrap").await else {
        eprintln!("DATABASE_URL not set — skipping integration test");
        return;
    };
    let url = db.url.clone();

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();

    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg.clone(), store.clone())
        .await
        .unwrap();
    adapter.init().await.unwrap();

    let m1 = adapter.snapshot().await.unwrap();
    let m2 = adapter.snapshot().await.unwrap();

    assert_eq!(
        m1.manifest.hash(),
        m2.manifest.hash(),
        "back-to-back snapshots of unchanged data must hash identically"
    );
    assert_eq!(
        m1.manifest.entries.len(),
        1,
        "single small table fits in one sealed segment"
    );
    assert_eq!(m1.manifest.entries[0].row_count, 5);

    drop_schema(&admin_pool, &schema).await;
    db.drop().await;
}

#[tokio::test]
#[ignore]
async fn install_helpers_is_idempotent() {
    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());
    let cfg = PgConfig::new(url, Vec::new());
    let mut a = PostgresAdapter::connect(cfg.clone(), store.clone())
        .await
        .unwrap();
    a.init().await.unwrap();
    // Re-init must not error or duplicate helpers.
    a.init().await.unwrap();
    let v = a.current_schema_version().await.unwrap();
    assert_eq!(v, "0.0.0", "no migrations recorded yet");
}

/// The advisory lock used by `snapshot()` must be observable from a
/// separate Postgres session while a snapshot is in progress. We verify
/// by acquiring it ourselves from a second pool and watching `snapshot`
/// block until we release it.
#[tokio::test]
#[ignore]
async fn snapshot_serialises_through_advisory_lock() {
    use std::time::{Duration, Instant};

    // The same constant used by postgres.rs. Duplicated here on purpose:
    // changing it server-side without updating tests should fail loudly.
    const SNAPSHOT_KEY: i64 = 0x6167_656e_7469_635f;

    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();

    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg.clone(), store.clone())
        .await
        .unwrap();
    adapter.init().await.unwrap();

    // Hold the snapshot lock on a sibling connection.
    let mut holder = admin_pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SNAPSHOT_KEY)
        .execute(&mut *holder)
        .await
        .unwrap();

    // Kick off a snapshot — it should block on us.
    let start = Instant::now();
    let snapshot_fut = tokio::spawn(async move { adapter.snapshot().await });

    // Give the snapshot a moment to enter the lock acquire.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !snapshot_fut.is_finished(),
        "snapshot should still be blocked on the advisory lock"
    );

    // Release. The snapshot should complete shortly after.
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SNAPSHOT_KEY)
        .execute(&mut *holder)
        .await
        .unwrap();
    drop(holder);

    let handle = tokio::time::timeout(Duration::from_secs(5), snapshot_fut)
        .await
        .expect("snapshot did not complete after lock release")
        .expect("snapshot task panicked")
        .expect("snapshot returned Err");
    assert!(start.elapsed() >= Duration::from_millis(200));
    assert_eq!(handle.schema_version, "0.0.0");

    drop_schema(&admin_pool, &schema).await;
}

/// AC1 for issue #35 / audit §A1: writes that occur while a restore is in
/// progress must not leak into the streamer's view (and therefore must
/// not appear in any later snapshot). The poller is paused for the
/// duration of `restore`, and `restore` truncates `agentic_change_log`
/// inside its own transaction so neither pre-existing nor restore-fired
/// trigger events reach the streamer.
#[tokio::test]
#[ignore]
async fn ac1_writes_during_restore_are_reverted() {
    use std::time::Duration;

    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();
    // make_schema inserts 5 rows (id=1..=5). Snapshot at that point is
    // our manifest A. The 100 "concurrent" writes will use id=100..=199
    // so they don't collide with the baseline rows.

    // Long poll interval so the poller can't drain the test's writes
    // between setup and restore — the only drainer permitted in this
    // window is `restore`'s own change_log TRUNCATE inside its tx.
    let mut cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    cfg.poll_interval = Duration::from_secs(60);

    let mut adapter = PostgresAdapter::connect(cfg, store.clone()).await.unwrap();
    adapter.init().await.unwrap();

    // Snapshot the pre-write state. Manifest A captures id=1..=5.
    let handle_a = adapter.snapshot().await.unwrap();
    let baseline_total: u64 = handle_a.manifest.entries.iter().map(|e| e.row_count).sum();
    assert_eq!(baseline_total, 5, "make_schema inserted 5 rows");

    // User writes 100 more rows. Triggers fire and populate
    // public.agentic_change_log; the poller is on a 60s interval so
    // those entries sit there waiting.
    for i in 100i64..200 {
        admin_pool
            .execute(
                format!(
                    "INSERT INTO \"{schema}\".episodes (id, text) \
                     VALUES ({i}, 'concurrent-write-{i}')"
                )
                .as_str(),
            )
            .await
            .unwrap();
    }

    // Confirm change_log accumulated entries (proves we're testing the
    // path the audit identified — pre-restore writes the poller could
    // forward to the streamer).
    let pre_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM public.agentic_change_log")
        .fetch_one(&admin_pool)
        .await
        .unwrap();
    assert!(
        pre_count.0 >= 100,
        "expected >= 100 change_log entries from the 100 concurrent writes, got {}",
        pre_count.0
    );

    // Restore from manifest A. This pauses the poller, opens a transaction,
    // TRUNCATEs the tracked table, INSERTs only the manifest's rows,
    // TRUNCATEs agentic_change_log inside the same transaction, and
    // commits. The poller resumes on guard drop and sees an empty log.
    adapter.restore(&handle_a).await.unwrap();

    // Table is back to the baseline.
    let row_count: (i64,) =
        sqlx::query_as(format!("SELECT COUNT(*) FROM \"{schema}\".episodes").as_str())
            .fetch_one(&admin_pool)
            .await
            .unwrap();
    assert_eq!(
        row_count.0, 5,
        "expected 5 baseline rows after restore; got {}",
        row_count.0
    );

    // The 100 concurrent writes are gone from change_log (truncated
    // inside the restore tx); the poller never forwarded them.
    let post_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM public.agentic_change_log")
        .fetch_one(&admin_pool)
        .await
        .unwrap();
    assert_eq!(
        post_count.0, 0,
        "restore should have TRUNCATEd agentic_change_log inside its tx; got {} rows",
        post_count.0
    );

    // The strongest assertion: take a fresh snapshot. Its manifest must
    // match A — no extra rows leaked into the streamer's view from the
    // 100 concurrent writes. (Without the fix, the poller would have
    // forwarded those events to the streamer between the writes and
    // restore, and the next snapshot would describe 105 rows.)
    let handle_b = adapter.snapshot().await.unwrap();
    let post_total: u64 = handle_b.manifest.entries.iter().map(|e| e.row_count).sum();
    assert_eq!(
        post_total, baseline_total,
        "post-restore snapshot must contain only the baseline rows; \
         got {post_total} (baseline {baseline_total})"
    );

    drop_schema(&admin_pool, &schema).await;
}

// ── §9 performance smoke ──────────────────────────────────────────────────────
//
// Sprint item A3 / docs/architecture/benchmarks.md §9 targets: this test
// captures snapshot / restore / diff timings at parameterised row count.
// `#[ignore]` so default `cargo test` skips it; run explicitly:
//
//   docker compose -f examples/langgraph-rollback/docker-compose.yml up -d
//   docker exec agentic-demo-pg psql -U agentic -d agentic \
//       -c "CREATE EXTENSION IF NOT EXISTS vector"
//   DATABASE_URL=postgres://agentic:agentic@localhost:54322/agentic \
//   BENCH_ROWS=1000000 \
//       cargo test -p agentic-memory --test integration -- \
//       --ignored --test-threads=1 --nocapture pg_snapshot_perf_smoke
//
// Defaults `BENCH_ROWS=10000` so first-time runs finish in seconds. Larger
// values trade time for fidelity to the §9-shaped 1M-row claim. Numbers
// print to stderr (`eprintln!`) in a paste-into-markdown shape — the
// operator copies them into `docs/architecture/benchmarks.md`.

fn bench_rows() -> u64 {
    std::env::var("BENCH_ROWS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
}

/// Bulk-INSERT `n` rows into the schema's `episodes` table as a small
/// number of batched single-statement INSERTs. Postgres' single-statement
/// INSERT with many VALUES is far faster than N round-trip statements;
/// for 1M rows on a laptop this is the difference between "minutes" and
/// "tens of seconds". The batching cap keeps each statement well under
/// any practical query-length limit.
async fn bulk_insert_episodes(pool: &PgPool, schema: &str, n: u64) {
    use std::fmt::Write;

    const BATCH: u64 = 50_000;
    let next_start: u64 = 100; // start past the 5 baseline rows make_schema seeds
                               // Checked addition: BENCH_ROWS is operator-controlled; refuse to
                               // overflow into a wraparound (release) or panic (debug) on a
                               // misconfigured value rather than silently producing the wrong
                               // number of rows.
    let end = next_start
        .checked_add(n)
        .unwrap_or_else(|| panic!("BENCH_ROWS={n} overflows u64 when offset by the baseline 100"));

    let mut next_id = next_start;
    while next_id < end {
        let batch_end = (next_id + BATCH).min(end);
        // Pre-reserve capacity for the batch so per-row `write!` calls
        // don't reallocate. Each row encodes as roughly
        // "(<19-digit id>, 'ep-<19-digit id>'), " — bound generously
        // at 64 bytes/row plus the static prefix.
        let mut sql = String::with_capacity(64 + 64 * (batch_end - next_id) as usize);
        write!(sql, "INSERT INTO \"{schema}\".episodes (id, text) VALUES ").unwrap();
        for i in next_id..batch_end {
            if i > next_id {
                sql.push(',');
            }
            // `write!` into the existing buffer avoids the per-row
            // String allocation that `format!(...)` followed by
            // `push_str` would do.
            write!(sql, "({i}, 'ep-{i}')").unwrap();
        }
        pool.execute(sql.as_str())
            .await
            .expect("bulk INSERT failed");
        next_id = batch_end;
    }
}

/// Pretty-print a duration so the eprintln! output is easy to read in a
/// terminal AND mechanically extractable. Trailing whitespace matters:
/// numbers are right-aligned to a 12-char column.
fn fmt_dur(d: std::time::Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms >= 1000.0 {
        format!("{:>10.3} s", ms / 1000.0)
    } else if ms >= 1.0 {
        format!("{:>9.2} ms", ms)
    } else {
        format!("{:>9.1} µs", ms * 1000.0)
    }
}

#[tokio::test]
#[ignore]
async fn pg_snapshot_perf_smoke() {
    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping perf smoke");
            return;
        }
    };
    let n = bench_rows();

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();

    // Hand-seed N rows on top of the 5 baseline make_schema inserts so
    // the snapshot runs against a realistically-sized table.
    let seed_t0 = std::time::Instant::now();
    bulk_insert_episodes(&admin_pool, &schema, n).await;
    let seed_elapsed = seed_t0.elapsed();

    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg, store.clone()).await.unwrap();
    adapter.init().await.unwrap();

    // ── snapshot ──────────────────────────────────────────────────────
    // §9 target: < 2 s on 1M-row pgvector.
    let t0 = std::time::Instant::now();
    let handle_a = adapter.snapshot().await.unwrap();
    let snapshot_elapsed = t0.elapsed();
    let manifest_total: u64 = handle_a.manifest.entries.iter().map(|e| e.row_count).sum();

    // ── restore (no-op: snapshot back into the same state) ────────────
    // §9 target: < 5 s on 1M-row pgvector + 10 commits. This run is the
    // single-snapshot lower bound; multi-commit rollback isn't modelled
    // here because the perf cost of restore is dominated by the
    // TRUNCATE + INSERT replay, which is what we measure.
    let t0 = std::time::Instant::now();
    adapter.restore(&handle_a).await.unwrap();
    let restore_elapsed = t0.elapsed();

    // ── second snapshot for diff ──────────────────────────────────────
    let handle_b = adapter.snapshot().await.unwrap();

    // ── diff ──────────────────────────────────────────────────────────
    // §9 target: < 1 s on 1M-row pgvector. The diff is a content-addressed
    // comparison of two SegmentManifests, so it doesn't touch Postgres at
    // all — measured for completeness.
    let t0 = std::time::Instant::now();
    let manifest_a_hash = handle_a.manifest.hash();
    let manifest_b_hash = handle_b.manifest.hash();
    let identical = manifest_a_hash == manifest_b_hash;
    let diff_elapsed = t0.elapsed();
    assert!(
        identical,
        "snapshot→restore→snapshot must yield identical manifest hashes \
         (no schema or row mutations in between); got {manifest_a_hash} vs {manifest_b_hash}"
    );

    // ── report (paste-into-markdown format) ───────────────────────────
    eprintln!();
    eprintln!("# pg_snapshot_perf_smoke (BENCH_ROWS={n}, manifest_total={manifest_total})");
    eprintln!();
    eprintln!("| Operation | Measured     | §9 target           |");
    eprintln!("|---|---|---|");
    eprintln!(
        "| bulk seed (Postgres INSERTs, {n} rows)       | {} | n/a — setup cost            |",
        fmt_dur(seed_elapsed)
    );
    eprintln!(
        "| `snapshot()`                                   | {} | < 2 s @ 1M-row pgvector     |",
        fmt_dur(snapshot_elapsed)
    );
    eprintln!(
        "| `restore()` (no-op replay)                     | {} | < 5 s @ 1M-row + 10 commits |",
        fmt_dur(restore_elapsed)
    );
    eprintln!(
        "| `diff` (manifest hash compare)                 | {} | < 1 s @ 1M-row pgvector     |",
        fmt_dur(diff_elapsed)
    );
    eprintln!();
    eprintln!("Manifest A hash: {manifest_a_hash}");
    eprintln!("Manifest B hash: {manifest_b_hash}");
    eprintln!();

    drop_schema(&admin_pool, &schema).await;
}
// ── Data-integrity error paths ────────────────────────────────────────────────

/// A NULL-valued primary key must cause `snapshot` to fail loudly rather
/// than silently anchor a segment with `pk_lo`/`pk_hi == null`. The
/// manifest's range metadata is load-bearing for restore; a null-anchored
/// segment would corrupt downstream rollback selection.
///
/// Schema: `id bigint NULL` (not the usual NOT NULL) so we can insert a
/// row with `id = NULL` and exercise the path.
#[tokio::test]
#[ignore]
async fn init_rejects_null_primary_key() {
    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    admin_pool
        .execute(format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\"").as_str())
        .await
        .unwrap();
    admin_pool
        .execute(
            format!(
                r#"CREATE TABLE "{schema}".episodes (
                    id    bigint,
                    text  text NOT NULL
                )"#
            )
            .as_str(),
        )
        .await
        .unwrap();
    admin_pool
        .execute(
            format!("INSERT INTO \"{schema}\".episodes (id, text) VALUES (NULL, 'orphan')")
                .as_str(),
        )
        .await
        .unwrap();

    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg, store).await.unwrap();
    // `init` bootstraps every tracked table via the same `row_to_json`
    // path that `snapshot` uses, so the error surfaces here on the
    // very first read — earlier than `snapshot` is even reached. The
    // operator-level shape: "starting agenticd against a table with a
    // NULL PK fails loudly with the offending column named."
    let err = adapter
        .init()
        .await
        .expect_err("init must reject a table with a NULL PK row");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("primary-key column \"id\" is NULL"),
        "error message must name the NULL PK explicitly; got: {msg}"
    );

    drop_schema(&admin_pool, &schema).await;
}

/// A column whose decode would fail (NaN floats can't round-trip through
/// JSON Number) must propagate as an Err instead of silently emitting
/// `Json::Null`. Before this fix, every decode-failure arm in
/// `row_to_json` did `.unwrap_or(Json::Null)` and the snapshot was
/// silently corrupted.
#[tokio::test]
#[ignore]
async fn init_rejects_non_finite_float() {
    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    admin_pool
        .execute(format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\"").as_str())
        .await
        .unwrap();
    admin_pool
        .execute(
            format!(
                r#"CREATE TABLE "{schema}".episodes (
                    id    bigint PRIMARY KEY,
                    score double precision NOT NULL,
                    text  text NOT NULL
                )"#
            )
            .as_str(),
        )
        .await
        .unwrap();
    admin_pool
        .execute(
            format!(
                "INSERT INTO \"{schema}\".episodes (id, score, text) \
                 VALUES (1, 'NaN'::float8, 'nan-row')"
            )
            .as_str(),
        )
        .await
        .unwrap();

    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg, store).await.unwrap();
    // Same shape as the NULL-PK test: `init` bootstraps the table via
    // the `row_to_json` path, so the non-finite-float decode error
    // surfaces here.
    let err = adapter
        .init()
        .await
        .expect_err("init must reject a non-finite float row");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("non-finite") && msg.contains("score"),
        "error message must name the offending column and the reason; got: {msg}"
    );
    // Table name must appear too — without this assertion, removing
    // `table: &str` from row_to_json/decode_err breaks no test.
    assert!(
        msg.contains("episodes"),
        "error message must name the table for multi-table diagnostics; got: {msg}"
    );

    drop_schema(&admin_pool, &schema).await;
}

/// A `TrackedTable.pk` that names a column that doesn't exist in the
/// live table should produce the "absent column" error, not the
/// "NULL column" one — distinct operator-facing messages for distinct
/// causes.
#[tokio::test]
#[ignore]
async fn init_rejects_absent_primary_key_column() {
    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();

    // Configure a PK column name that's NOT in the table. The make_schema
    // helper creates `episodes (id, text)`; ask for `not_a_column`.
    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "not_a_column".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg, store).await.unwrap();

    let err = adapter
        .init()
        .await
        .expect_err("init must reject an absent PK column");
    let msg = format!("{err:#}");
    // Bootstrap's `SELECT ... ORDER BY <pk>` query fails at the SQL
    // level before any row reaches `row_to_json`, so Postgres' own
    // "column does not exist" message is what surfaces. The
    // `row_to_json`-level "primary-key column absent" branch only
    // fires on the streamer path where events arrive without
    // going through an ORDER BY (a TrackedTable.pk that doesn't
    // exist would still trip there). For bootstrap the SQL error
    // is the right operator signal.
    assert!(
        msg.contains("not_a_column")
            && (msg.contains("does not exist") || msg.contains("is absent")),
        "error must name the missing PK column; got: {msg}"
    );

    drop_schema(&admin_pool, &schema).await;
}

/// Happy-path regression guard: a finite float column must round-trip
/// through `row_to_json` as a JSON Number, not error. Before this PR
/// the silent `.unwrap_or(Json::Null)` made decode regressions
/// invisible; with errors propagating, a regression in the float arm
/// (wrong sqlx type, missing arm) would surface as init failure.
#[tokio::test]
#[ignore]
async fn init_accepts_finite_floats_and_nullable_non_pk_columns() {
    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    admin_pool
        .execute(format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\"").as_str())
        .await
        .unwrap();
    // Float column AND a nullable text column — covers (a) the
    // float-happy-path regression guard and (b) the "NULL in a
    // non-PK column should still be Ok(Json::Null)" regression
    // guard in one schema.
    admin_pool
        .execute(
            format!(
                r#"CREATE TABLE "{schema}".episodes (
                    id    bigint PRIMARY KEY,
                    score double precision NOT NULL,
                    note  text -- nullable
                )"#
            )
            .as_str(),
        )
        .await
        .unwrap();
    admin_pool
        .execute(
            format!(
                "INSERT INTO \"{schema}\".episodes (id, score, note) VALUES \
                 (1, 0.5, 'normal'), \
                 (2, -3.14, NULL), \
                 (3, 0.0, '')"
            )
            .as_str(),
        )
        .await
        .unwrap();

    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg, store).await.unwrap();
    adapter
        .init()
        .await
        .expect("init must succeed on finite floats + nullable non-PK columns");

    // Take a snapshot and verify the rows round-tripped (manifest carries
    // the expected row count).
    let handle = adapter.snapshot().await.expect("snapshot should succeed");
    let total: u64 = handle.manifest.entries.iter().map(|e| e.row_count).sum();
    assert_eq!(total, 3, "all three rows should land in the snapshot");

    drop_schema(&admin_pool, &schema).await;
}

/// ±Infinity are also non-finite — same JSON-can't-represent issue as
/// NaN, also should error. Covers both signs in one fixture.
#[tokio::test]
#[ignore]
async fn init_rejects_positive_and_negative_infinity() {
    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    for (literal, label) in [("Infinity", "+Inf"), ("-Infinity", "-Inf")] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

        let admin_pool = PgPool::connect(&url).await.unwrap();
        let schema = fresh_schema_name();
        admin_pool
            .execute(format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\"").as_str())
            .await
            .unwrap();
        admin_pool
            .execute(
                format!(
                    r#"CREATE TABLE "{schema}".episodes (
                        id    bigint PRIMARY KEY,
                        score double precision NOT NULL
                    )"#
                )
                .as_str(),
            )
            .await
            .unwrap();
        admin_pool
            .execute(
                format!(
                    "INSERT INTO \"{schema}\".episodes (id, score) \
                     VALUES (1, '{literal}'::float8)"
                )
                .as_str(),
            )
            .await
            .unwrap();

        let cfg = PgConfig::new(
            schema_scoped_url(&url, &schema),
            vec![TrackedTable {
                name: "episodes".into(),
                pk: "id".into(),
            }],
        );
        let mut adapter = PostgresAdapter::connect(cfg, store).await.unwrap();

        let err = adapter
            .init()
            .await
            .expect_err(&format!("init must reject {label}"));
        let msg = format!("{err:#}");
        assert!(
            msg.contains("non-finite"),
            "error must say 'non-finite' for {label}; got: {msg}"
        );

        drop_schema(&admin_pool, &schema).await;
    }
}

/// A delete envelope with a NULL or absent PK must fail restore loudly
/// rather than silently no-op via `DELETE … WHERE pk = NULL`. Construct
/// the segment by hand, write it to the object store, then call
/// `restore` with a manifest pointing at it.
#[tokio::test]
#[ignore]
async fn restore_rejects_delete_envelope_with_null_pk() {
    use agentic_core::{ObjectKind, ObjectStore as _};
    use agentic_memory::adapter::{MemoryAdapter as _, SnapshotHandle};
    use agentic_memory::segment::{Segment, SegmentManifest, SegmentRef};

    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();

    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg, store.clone()).await.unwrap();
    adapter.init().await.unwrap();

    // Build a segment with one delete envelope whose row has a NULL
    // PK. This mimics what a buggy streamer could otherwise persist.
    let mut seg = Segment {
        table: "episodes".into(),
        schema_version: "0.0.0".into(),
        pk_lo: serde_json::Value::Null,
        pk_hi: serde_json::Value::Null,
        row_count: 1,
        rows: Vec::new(),
        embeddings: Vec::new(),
        metadata: Default::default(),
    };
    seg.rows.push(serde_json::json!({
        "op": "delete",
        "row": {"id": serde_json::Value::Null},
    }));
    let bytes = seg.to_canonical_bytes();
    let seg_hash = store.put_raw(ObjectKind::Segment, &bytes).unwrap();

    let mut manifest = SegmentManifest::new("0.0.0".to_string());
    manifest.push(SegmentRef {
        table: "episodes".into(),
        pk_lo: serde_json::Value::Null,
        pk_hi: serde_json::Value::Null,
        segment: seg_hash,
        row_count: 1,
    });
    let handle = SnapshotHandle {
        manifest,
        schema_version: "0.0.0".to_string(),
    };

    let err = adapter
        .restore(&handle)
        .await
        .expect_err("restore must reject a delete envelope with NULL PK");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("PK column") && msg.contains("NULL"),
        "error must name the NULL PK and refuse the no-op DELETE; got: {msg}"
    );

    // Restore opens a transaction that TRUNCATEs then replays rows;
    // when delete_row errors mid-replay the whole tx must roll back,
    // leaving the user's original 5 baseline rows intact. Locks in
    // the invariant that a refactor moving TRUNCATE outside the tx
    // would break.
    let count: (i64,) =
        sqlx::query_as(format!("SELECT COUNT(*) FROM \"{schema}\".episodes").as_str())
            .fetch_one(&admin_pool)
            .await
            .unwrap();
    assert_eq!(
        count.0, 5,
        "failed restore must roll back TRUNCATE; original rows must survive"
    );

    drop_schema(&admin_pool, &schema).await;
}

/// Same shape but with the PK column absent from the delete row
/// payload entirely.
#[tokio::test]
#[ignore]
async fn restore_rejects_delete_envelope_with_absent_pk() {
    use agentic_core::{ObjectKind, ObjectStore as _};
    use agentic_memory::adapter::{MemoryAdapter as _, SnapshotHandle};
    use agentic_memory::segment::{Segment, SegmentManifest, SegmentRef};

    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();

    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg, store.clone()).await.unwrap();
    adapter.init().await.unwrap();

    let mut seg = Segment {
        table: "episodes".into(),
        schema_version: "0.0.0".into(),
        pk_lo: serde_json::Value::Null,
        pk_hi: serde_json::Value::Null,
        row_count: 1,
        rows: Vec::new(),
        embeddings: Vec::new(),
        metadata: Default::default(),
    };
    seg.rows.push(serde_json::json!({
        "op": "delete",
        "row": {"text": "no id field"},
    }));
    let bytes = seg.to_canonical_bytes();
    let seg_hash = store.put_raw(ObjectKind::Segment, &bytes).unwrap();

    let mut manifest = SegmentManifest::new("0.0.0".to_string());
    manifest.push(SegmentRef {
        table: "episodes".into(),
        pk_lo: serde_json::Value::Null,
        pk_hi: serde_json::Value::Null,
        segment: seg_hash,
        row_count: 1,
    });
    let handle = SnapshotHandle {
        manifest,
        schema_version: "0.0.0".to_string(),
    };

    let err = adapter
        .restore(&handle)
        .await
        .expect_err("restore must reject a delete envelope with absent PK");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("PK column") && msg.contains("absent"),
        "error must name the absent PK column; got: {msg}"
    );

    // Failed restore must roll back TRUNCATE — same invariant as the
    // NULL-PK sibling test.
    let count: (i64,) =
        sqlx::query_as(format!("SELECT COUNT(*) FROM \"{schema}\".episodes").as_str())
            .fetch_one(&admin_pool)
            .await
            .unwrap();
    assert_eq!(
        count.0, 5,
        "failed restore must roll back TRUNCATE; original rows must survive"
    );

    drop_schema(&admin_pool, &schema).await;
}

/// The snapshot fence's strict drain mode must (a) refuse to snapshot
/// when a change_log row can't be forwarded to the streamer, AND
/// (b) NOT delete the offending row — so a retry sees it, blocks
/// again, and the operator can fix the underlying invariant. The
/// previous round-4 fix returned `Err` but had already deleted the
/// row, making the retry succeed with a silently-incomplete snapshot.
#[tokio::test]
#[ignore]
async fn snapshot_strict_drain_preserves_bad_row_on_block() {
    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();

    // Use a long poll interval so the background poller doesn't drain
    // the bad row out from under us before we call snapshot().
    let mut cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    cfg.poll_interval = std::time::Duration::from_secs(600);
    let mut adapter = PostgresAdapter::connect(cfg, store).await.unwrap();
    adapter.init().await.unwrap();

    // The public.agentic_change_log table is shared across all
    // schemas in this database. By the time we get here, prior
    // tests have written + drained their own trigger events; the
    // log may be empty or may have residue. To guarantee the only
    // row drain_to_completion sees is the one we plant, clear it
    // explicitly. This is the same TRUNCATE pattern restore uses
    // for the same reason.
    admin_pool
        .execute("TRUNCATE public.agentic_change_log")
        .await
        .unwrap();

    // Inject a change_log row whose `table_name` references a table
    // we DON'T have in TrackedTable config. This trips the strict
    // mode's untracked-table check.
    admin_pool
        .execute(
            "INSERT INTO public.agentic_change_log (table_name, op, row) \
             VALUES ('public.not_tracked', 'insert', '{\"id\": 99}'::jsonb)",
        )
        .await
        .unwrap();
    let pre_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM public.agentic_change_log WHERE table_name = 'public.not_tracked'",
    )
    .fetch_one(&admin_pool)
    .await
    .unwrap();
    assert_eq!(pre_count.0, 1, "test fixture: planted row must be present");

    // Snapshot must fail — strict drain refuses to proceed.
    let err = adapter
        .snapshot()
        .await
        .expect_err("snapshot must block on an untracked-table change_log row");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("not_tracked") && msg.contains("untracked"),
        "error must name the offending table and the cause; got: {msg}"
    );

    // Critically: the bad row must STILL be in change_log. The
    // previous (broken) shape deleted it on the first call, making a
    // retry see an empty log and succeed with a silently-incomplete
    // snapshot.
    let post_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM public.agentic_change_log WHERE table_name = 'public.not_tracked'",
    )
    .fetch_one(&admin_pool)
    .await
    .unwrap();
    assert_eq!(
        post_count.0, 1,
        "strict drain must NOT delete the row that blocked the snapshot; \
         operator needs it preserved for the retry-after-fix flow"
    );

    drop_schema(&admin_pool, &schema).await;
    // Clean up the planted row so other tests in the suite aren't affected.
    let _ = admin_pool
        .execute("DELETE FROM public.agentic_change_log WHERE table_name = 'public.not_tracked'")
        .await;
}

/// A schema-qualified `TrackedTable.name` (e.g. `"public.episodes"`)
/// must route trigger-captured events to the streamer head correctly.
///
/// The previous resolver shape stored bare names as VALUES in the
/// exact-match map, so a configured `"public.episodes"` paired with
/// a trigger emission of the same string returned the bare
/// `"episodes"` — which didn't match the streamer's head keyed by
/// `"public.episodes"`. Events got silently dropped by the streamer
/// while strict pre-validation passed (the lookup found something,
/// just the wrong value).
///
/// This test creates a public-schema table, configures the adapter
/// with the schema-qualified name, writes a row, takes a snapshot,
/// and asserts the row landed in the manifest. If the resolver bug
/// returns it would silently produce an empty manifest.
#[tokio::test]
#[ignore]
async fn schema_qualified_tracked_table_routes_events() {
    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();
    // Clear any prior-test residue so the snapshot's strict drain
    // sees only the rows this test produced.
    admin_pool
        .execute("TRUNCATE public.agentic_change_log")
        .await
        .unwrap();

    // Configure with the SCHEMA-QUALIFIED form — the path that was
    // broken before the resolver fix.
    let qualified = format!("{schema}.episodes");
    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: qualified.clone(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg, store).await.unwrap();
    adapter.init().await.unwrap();

    let handle = adapter
        .snapshot()
        .await
        .expect("snapshot must succeed with a schema-qualified TrackedTable");
    let total: u64 = handle.manifest.entries.iter().map(|e| e.row_count).sum();
    assert_eq!(
        total, 5,
        "schema-qualified config must route the 5 baseline rows; pre-fix this returned 0"
    );
    // The manifest's table-name string must be the configured form,
    // not the bare form — downstream rollback selects by exact
    // string compare.
    assert!(
        handle.manifest.entries.iter().all(|e| e.table == qualified),
        "manifest entries must carry the configured table key; got: {:?}",
        handle
            .manifest
            .entries
            .iter()
            .map(|e| &e.table)
            .collect::<Vec<_>>()
    );

    drop_schema(&admin_pool, &schema).await;
}

// ── Restore batching edge cases (PR #92 review follow-ups) ───────────────────

/// Helper: construct a SegmentManifest pointing at one segment whose
/// raw bytes contain the given envelopes. Used by the batching tests
/// below to drive `restore` against hand-crafted shapes.
fn write_single_segment(
    store: &Arc<FsObjectStore>,
    table: &str,
    schema_version: &str,
    envelopes: Vec<serde_json::Value>,
) -> agentic_memory::segment::SegmentManifest {
    use agentic_core::{ObjectKind, ObjectStore as _};
    use agentic_memory::segment::{Segment, SegmentManifest, SegmentRef};

    let row_count = envelopes.len() as u64;
    let seg = Segment {
        table: table.into(),
        schema_version: schema_version.into(),
        pk_lo: serde_json::Value::Null,
        pk_hi: serde_json::Value::Null,
        row_count,
        rows: envelopes,
        embeddings: Vec::new(),
        metadata: Default::default(),
    };
    let bytes = seg.to_canonical_bytes();
    let seg_hash = store.put_raw(ObjectKind::Segment, &bytes).unwrap();
    let mut manifest = SegmentManifest::new(schema_version.to_string());
    manifest.push(SegmentRef {
        table: table.into(),
        pk_lo: serde_json::Value::Null,
        pk_hi: serde_json::Value::Null,
        segment: seg_hash,
        row_count,
    });
    manifest
}

/// Two upserts with the same PK in one same-shape run used to pack
/// into a single `INSERT … VALUES (...,A), (...,B) ON CONFLICT` —
/// which Postgres rejects with SQLSTATE 21000 (`ON CONFLICT DO UPDATE
/// command cannot affect row a second time`). The batching planner
/// now flushes on the second occurrence so each lands in its own
/// statement; ON CONFLICT DO UPDATE on the second statement
/// overwrites the first row's data, preserving "last writer wins".
#[tokio::test]
#[ignore]
async fn restore_handles_duplicate_pk_within_a_batch() {
    use agentic_memory::adapter::{MemoryAdapter as _, SnapshotHandle};

    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();

    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg, store.clone()).await.unwrap();
    adapter.init().await.unwrap();

    // Segment with two updates to id=42: text="first" then text="last".
    // Both are upserts of the same column-set, so the pre-fix planner
    // would have packed them into one INSERT … VALUES (42,'first'), (42,'last')
    // ON CONFLICT DO UPDATE — which Postgres rejects.
    let manifest = write_single_segment(
        &store,
        "episodes",
        "0.0.0",
        vec![
            serde_json::json!({"op": "insert", "row": {"id": 42, "text": "first"}}),
            serde_json::json!({"op": "update", "row": {"id": 42, "text": "last"}}),
        ],
    );
    let handle = SnapshotHandle {
        manifest,
        schema_version: "0.0.0".to_string(),
    };

    adapter
        .restore(&handle)
        .await
        .expect("restore must handle duplicate PK in same shape run by flushing between");

    // Verify last-writer-wins AND that only one row exists. Fetching
    // all rows and asserting full set — `fetch_one` would silently
    // accept extra rows beyond the first, which would mask a bug
    // where TRUNCATE didn't run or extra inserts leaked.
    let rows: Vec<(i64, String)> =
        sqlx::query_as(format!("SELECT id, text FROM \"{schema}\".episodes").as_str())
            .fetch_all(&admin_pool)
            .await
            .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "exactly one row expected after duplicate-PK restore; got {} rows: {:?}",
        rows.len(),
        rows
    );
    assert_eq!(rows[0].0, 42);
    assert_eq!(
        rows[0].1, "last",
        "duplicate-PK restore must preserve last-writer-wins via separate statements; \
         got {:?}",
        rows[0].1
    );

    drop_schema(&admin_pool, &schema).await;
}

/// An empty delete envelope (`{"op": "delete", "row": {}}`) has no PK
/// to issue the DELETE with. The old single-row `delete_row` errored
/// loudly; the batched path must preserve that loud failure rather
/// than silently no-op.
#[tokio::test]
#[ignore]
async fn restore_rejects_empty_delete_envelope() {
    use agentic_memory::adapter::{MemoryAdapter as _, SnapshotHandle};

    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();

    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg, store.clone()).await.unwrap();
    adapter.init().await.unwrap();

    let manifest = write_single_segment(
        &store,
        "episodes",
        "0.0.0",
        vec![serde_json::json!({"op": "delete", "row": {}})],
    );
    let handle = SnapshotHandle {
        manifest,
        schema_version: "0.0.0".to_string(),
    };

    let err = adapter
        .restore(&handle)
        .await
        .expect_err("restore must reject an empty delete envelope");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("delete envelope") && msg.contains("empty"),
        "error must name the empty-delete failure mode; got: {msg}"
    );

    // Failed restore must roll back the TRUNCATE — original 5 rows survive.
    let count: (i64,) =
        sqlx::query_as(format!("SELECT COUNT(*) FROM \"{schema}\".episodes").as_str())
            .fetch_one(&admin_pool)
            .await
            .unwrap();
    assert_eq!(
        count.0, 5,
        "failed restore must roll back TRUNCATE; original rows must survive"
    );

    drop_schema(&admin_pool, &schema).await;
}

/// Mode transitions mid-segment must flush. A sequence of
/// `Insert(id=1) Delete(id=1) Insert(id=1)` must apply as three
/// separate statements (the batching planner can't combine across
/// mode boundaries), and the final state must be the last Insert's
/// values.
#[tokio::test]
#[ignore]
async fn restore_preserves_order_across_mode_transitions() {
    use agentic_memory::adapter::{MemoryAdapter as _, SnapshotHandle};

    let url = match database_url() {
        Some(u) => u,
        None => {
            eprintln!("DATABASE_URL not set — skipping integration test");
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());

    let admin_pool = PgPool::connect(&url).await.unwrap();
    let schema = fresh_schema_name();
    make_schema(&admin_pool, &schema).await.unwrap();

    let cfg = PgConfig::new(
        schema_scoped_url(&url, &schema),
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }],
    );
    let mut adapter = PostgresAdapter::connect(cfg, store.clone()).await.unwrap();
    adapter.init().await.unwrap();

    // Insert id=7 v="first" → Delete id=7 → Insert id=7 v="final".
    // If mode transitions weren't flushed correctly, the final state
    // would be wrong (e.g. row missing because Delete ran after the
    // second Insert).
    let manifest = write_single_segment(
        &store,
        "episodes",
        "0.0.0",
        vec![
            serde_json::json!({"op": "insert", "row": {"id": 7, "text": "first"}}),
            serde_json::json!({"op": "delete", "row": {"id": 7}}),
            serde_json::json!({"op": "insert", "row": {"id": 7, "text": "final"}}),
        ],
    );
    let handle = SnapshotHandle {
        manifest,
        schema_version: "0.0.0".to_string(),
    };

    adapter
        .restore(&handle)
        .await
        .expect("restore must succeed across Insert→Delete→Insert sequence");

    let row: (i64, String) =
        sqlx::query_as(format!("SELECT id, text FROM \"{schema}\".episodes").as_str())
            .fetch_one(&admin_pool)
            .await
            .unwrap();
    assert_eq!(row.0, 7);
    assert_eq!(
        row.1, "final",
        "Insert→Delete→Insert must apply in order; final state is the last Insert. Got: {:?}",
        row.1
    );

    drop_schema(&admin_pool, &schema).await;
}
