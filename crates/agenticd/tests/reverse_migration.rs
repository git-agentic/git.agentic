//! Integration tests for the reverse-migration outer-transaction behavior
//! introduced by A8 (issue #37 / audit B8). These tests require a real
//! Postgres instance and are gated by `#[ignore]` so the default
//! `cargo test` run skips them on machines without one.
//!
//! Bring Postgres up (the broken-prompt demo's compose works fine):
//!
//! ```bash
//! docker compose -f examples/langgraph-rollback/docker-compose.yml up -d
//! DATABASE_URL=postgres://agentic:agentic@localhost:54322/agentic \
//!   cargo test -p agenticd --test reverse_migration -- --ignored
//! ```
//!
//! Or use `agentic-memory`'s test compose on port 54321:
//!
//! ```bash
//! podman compose -f crates/agentic-memory/tests/fixtures/pg.yml up -d
//! DATABASE_URL=postgres://agentic:agentic@localhost:54321/agentic \
//!   cargo test -p agenticd --test reverse_migration -- --ignored
//! ```
//!
//! Every test allocates its own schema (`agentic_test_<nanos>_<tag>`) so
//! parallel runs don't collide; the schema is dropped on teardown.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agentic_core::FsObjectStore;
use agentic_memory::postgres::{PgConfig, PostgresAdapter};
use agenticd::migrate;
use sqlx::{Executor, PgPool};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test fixture helpers
// ---------------------------------------------------------------------------

fn database_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

fn fresh_schema_name(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("agentic_test_{nanos}_{tag}")
}

/// Build a URL with `search_path` pointing at our temp schema so adapter
/// SQL like `SELECT FROM agentic_migrations` resolves to the test schema.
fn schema_scoped_url(base: &str, schema: &str) -> String {
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-csearch_path%3D{schema}%2Cpublic")
}

/// Install enough of `PostgresAdapter::install_helpers` for the migration
/// runner to operate against a fresh schema: just `agentic_migrations`
/// (the `agentic_schema_version()` function is not needed by `run_reverse`).
async fn setup_schema_with_three_migrations(pool: &PgPool, schema: &str) -> sqlx::Result<()> {
    pool.execute(format!("CREATE SCHEMA \"{schema}\"").as_str())
        .await?;
    pool.execute(
        format!(
            "CREATE TABLE \"{schema}\".agentic_migrations ( \
                id          serial      PRIMARY KEY, \
                name        text        NOT NULL UNIQUE, \
                applied_at  timestamptz NOT NULL DEFAULT now() \
            )"
        )
        .as_str(),
    )
    .await?;
    // Three test tables representing three "applied" forward migrations.
    pool.execute(format!("CREATE TABLE \"{schema}\".step1_table (id int)").as_str())
        .await?;
    pool.execute(format!("CREATE TABLE \"{schema}\".step2_table (id int)").as_str())
        .await?;
    pool.execute(format!("CREATE TABLE \"{schema}\".step3_table (id int)").as_str())
        .await?;
    pool.execute(
        format!(
            "INSERT INTO \"{schema}\".agentic_migrations (name) VALUES \
              ('001_create_step1'), \
              ('002_create_step2'), \
              ('003_create_step3')"
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

fn write_down_sql(tmp: &TempDir, name: &str, sql: &str) {
    let schema_dir = tmp.path().join("schema");
    std::fs::create_dir_all(&schema_dir).unwrap();
    std::fs::write(schema_dir.join(format!("{name}.down.sql")), sql).unwrap();
}

async fn migration_row_count(pool: &PgPool, schema: &str) -> sqlx::Result<i64> {
    let row: (i64,) =
        sqlx::query_as(format!("SELECT COUNT(*) FROM \"{schema}\".agentic_migrations").as_str())
            .fetch_one(pool)
            .await?;
    Ok(row.0)
}

async fn table_exists(pool: &PgPool, schema: &str, table: &str) -> sqlx::Result<bool> {
    // to_regclass returns NULL when the relation doesn't exist; we coerce to bool.
    let row: (Option<String>,) = sqlx::query_as("SELECT to_regclass($1)::text")
        .bind(format!("\"{schema}\".{table}"))
        .fetch_one(pool)
        .await?;
    Ok(row.0.is_some())
}

// ---------------------------------------------------------------------------
// AC1 — mid-sequence reverse-migration failure rolls back the entire sequence
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn ac1_mid_sequence_failure_rolls_back_entire_sequence() {
    let Some(url) = database_url() else {
        eprintln!("DATABASE_URL not set — skipping integration test");
        return;
    };

    let admin = PgPool::connect(&url).await.expect("admin pool");
    let schema = fresh_schema_name("ac1");
    setup_schema_with_three_migrations(&admin, &schema)
        .await
        .expect("setup test schema");

    // Steps to reverse: 003 (succeeds), 002 (fails on the second statement).
    // step 1 (`001_create_step1`) stays applied because we're targeting
    // schema_version "001_create_step1".
    let tmp = TempDir::new().unwrap();
    write_down_sql(&tmp, "003_create_step3", "DROP TABLE step3_table;");
    // step 2's down.sql drops the table, then deliberately fails. The outer
    // tx must roll back BOTH the step-2 partial work AND the step-3 commit.
    write_down_sql(
        &tmp,
        "002_create_step2",
        "DROP TABLE step2_table;\nSELECT 1/0;",
    );

    // Build an adapter pointed at the test schema. The object store isn't
    // exercised by `run_reverse`, but constructing the adapter requires one.
    let store_dir = TempDir::new().unwrap();
    let store = Arc::new(FsObjectStore::open(store_dir.path().join("objects")).unwrap());
    let cfg = PgConfig::new(schema_scoped_url(&url, &schema), Vec::new());
    let adapter = PostgresAdapter::connect(cfg, store)
        .await
        .expect("connect adapter");

    let steps = migrate::load_steps(
        tmp.path(),
        &[
            "003_create_step3".to_string(),
            "002_create_step2".to_string(),
        ],
        false, // accept_data_loss
    )
    .expect("load steps");
    assert_eq!(steps.len(), 2);

    let result = migrate::run_reverse(&adapter, steps).await;
    assert!(
        result.is_err(),
        "expected step-2 failure to propagate; got {result:?}"
    );

    // The discriminating assertions: every reversible side-effect from step 3
    // (DROP TABLE step3_table + DELETE row from agentic_migrations) must be
    // undone, because the outer transaction never committed.
    let count = migration_row_count(&admin, &schema).await.unwrap();
    assert_eq!(
        count, 3,
        "expected all 3 agentic_migrations rows to remain after outer-tx rollback"
    );
    assert!(
        table_exists(&admin, &schema, "step3_table").await.unwrap(),
        "step3_table should still exist — step-3 down.sql ran but the outer tx rolled back"
    );
    assert!(
        table_exists(&admin, &schema, "step2_table").await.unwrap(),
        "step2_table should still exist — step-2 partial work also rolled back"
    );
    assert!(
        table_exists(&admin, &schema, "step1_table").await.unwrap(),
        "step1_table was never targeted; should be untouched"
    );

    drop_schema(&admin, &schema).await;
}

// ---------------------------------------------------------------------------
// Sanity check: a successful reverse sequence DOES commit (no regression
// from the new outer-transaction wrapping).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn run_reverse_commits_successful_sequence() {
    let Some(url) = database_url() else {
        eprintln!("DATABASE_URL not set — skipping integration test");
        return;
    };

    let admin = PgPool::connect(&url).await.expect("admin pool");
    let schema = fresh_schema_name("happy");
    setup_schema_with_three_migrations(&admin, &schema)
        .await
        .expect("setup test schema");

    let tmp = TempDir::new().unwrap();
    write_down_sql(&tmp, "003_create_step3", "DROP TABLE step3_table;");
    write_down_sql(&tmp, "002_create_step2", "DROP TABLE step2_table;");

    let store_dir = TempDir::new().unwrap();
    let store = Arc::new(FsObjectStore::open(store_dir.path().join("objects")).unwrap());
    let cfg = PgConfig::new(schema_scoped_url(&url, &schema), Vec::new());
    let adapter = PostgresAdapter::connect(cfg, store).await.unwrap();

    let steps = migrate::load_steps(
        tmp.path(),
        &[
            "003_create_step3".to_string(),
            "002_create_step2".to_string(),
        ],
        false,
    )
    .unwrap();
    migrate::run_reverse(&adapter, steps)
        .await
        .expect("happy-path reverse");

    let count = migration_row_count(&admin, &schema).await.unwrap();
    assert_eq!(count, 1, "expected only 001_create_step1 to remain");
    assert!(
        !table_exists(&admin, &schema, "step3_table").await.unwrap(),
        "step3_table should be dropped"
    );
    assert!(
        !table_exists(&admin, &schema, "step2_table").await.unwrap(),
        "step2_table should be dropped"
    );
    assert!(
        table_exists(&admin, &schema, "step1_table").await.unwrap(),
        "step1_table should remain"
    );

    drop_schema(&admin, &schema).await;
}
