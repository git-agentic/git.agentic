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

#[tokio::test]
#[ignore]
async fn bootstrap_produces_a_deterministic_manifest() {
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

/// Bulk-INSERT `n` rows into the schema's `episodes` table in a single
/// statement. Postgres' single-statement INSERT with many VALUES is far
/// faster than N round-trip statements; for 1M rows on a laptop this is
/// the difference between "minutes" and "tens of seconds".
async fn bulk_insert_episodes(pool: &PgPool, schema: &str, n: u64) {
    // Cap the statement size to avoid hitting Postgres' max query length
    // (~1GB but practical limit is much lower). 50k rows per batch keeps
    // each statement well under any limit and gives the planner room.
    const BATCH: u64 = 50_000;
    let mut next_id: u64 = 100; // start past the 5 baseline rows make_schema seeds
    let end = next_id + n;
    while next_id < end {
        let batch_end = (next_id + BATCH).min(end);
        let mut sql = format!("INSERT INTO \"{schema}\".episodes (id, text) VALUES ");
        for i in next_id..batch_end {
            if i > next_id {
                sql.push(',');
            }
            // Each row carries a short identifier so total table size scales
            // with `n` rather than being dominated by a single huge text.
            sql.push_str(&format!("({i}, 'ep-{i}')"));
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
