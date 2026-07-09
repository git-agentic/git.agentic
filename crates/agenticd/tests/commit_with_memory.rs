//! Integration test covering `commit::execute` with a real Postgres
//! memory backend attached. Required because every other commit test in
//! `commit.rs` passes `no_memory: true`, so the `store_async::put_raw`
//! call site in `snapshot_memory` (ObjectKind::Tree, Arc<store>) is
//! never exercised by the workspace test suite. Audit §A5 follow-up
//! (PR #55 review Finding 2).
//!
//! Gated by `#[ignore]`; bring Postgres up first:
//!
//! ```bash
//! docker compose -f examples/langgraph-rollback/docker-compose.yml up -d
//! DATABASE_URL=postgres://agentic:agentic@localhost:54322/agentic \
//!   cargo test -p agenticd --test commit_with_memory -- --ignored
//! ```
//!
//! Allocates its own schema so parallel runs don't collide; the schema
//! is dropped on teardown.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agentic_core::commit::walk_log;
use agentic_core::refs::Refs;
use agentic_core::{FsObjectStore, ObjectStore};
use agentic_memory::postgres::{PgConfig, PostgresAdapter};
use agentic_memory::MemoryAdapter;
use agentic_proto::CommitInput;
use agenticd::commit;
use agenticd::server::DaemonState;
use sqlx::{Executor, PgPool};
use tokio::sync::Mutex;

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

fn schema_scoped_url(base: &str, schema: &str) -> String {
    let sep = if base.contains('?') { '&' } else { '?' };
    format!("{base}{sep}options=-csearch_path%3D{schema}%2Cpublic")
}

async fn drop_schema(pool: &PgPool, schema: &str) {
    let _ = pool
        .execute(sqlx::query(&format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE"
        )))
        .await;
}

fn commit_input_with_memory(message: &str) -> CommitInput {
    let mut prompts = std::collections::BTreeMap::new();
    prompts.insert("system.md".to_string(), b"you are helpful".to_vec());
    CommitInput {
        message: message.to_string(),
        author: Some("tester".to_string()),
        code_sha: Some("deadbeef".to_string()),
        branch: Some("main".to_string()),
        prompts,
        mcp_servers: Vec::new(),
        model: Some("anthropic:claude-opus:2026-05-01".to_string()),
        // Critical: opt into memory snapshotting so the `put_raw` call
        // site at commit.rs::snapshot_memory line ~144 is exercised.
        no_memory: false,
    }
}

/// AC: when `no_memory=false` and a real `PostgresAdapter` is attached,
/// `commit::execute` calls `store_async::put_raw` for the segment
/// manifest and threads the resulting hash into the Commit blob's
/// `memory_snapshot` field. Verifies the hash is present, addressable,
/// and the schema_version is populated.
#[tokio::test]
#[ignore]
async fn commit_with_memory_persists_manifest_via_put_raw() {
    let Some(base_url) = database_url() else {
        eprintln!("DATABASE_URL not set; skipping commit_with_memory integration test");
        return;
    };

    let schema = fresh_schema_name("commit_mem");
    let setup_pool = PgPool::connect(&base_url).await.unwrap();
    setup_pool
        .execute(sqlx::query(&format!("CREATE SCHEMA {schema}")))
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let agentic_dir = dir.path().join(".agentic");
    std::fs::create_dir_all(&agentic_dir).unwrap();
    std::fs::create_dir_all(agentic_dir.join("objects")).unwrap();

    let store: Arc<dyn ObjectStore + Send + Sync> =
        Arc::new(FsObjectStore::open(agentic_dir.join("objects")).unwrap());
    let refs = Refs::open(&agentic_dir).unwrap();

    let scoped_url = schema_scoped_url(&base_url, &schema);
    let cfg = PgConfig::new(scoped_url, Vec::new());
    let mut adapter = PostgresAdapter::connect(cfg, Arc::clone(&store))
        .await
        .unwrap();
    adapter.init().await.unwrap();

    let state = Arc::new(DaemonState {
        repo_root: dir.path().to_path_buf(),
        store: Arc::clone(&store),
        refs,
        commit_lock: Arc::new(Mutex::new(())),
        shutdown: tokio_util::sync::CancellationToken::new(),
        memory: Some(Arc::new(adapter)),
        mcp_servers: Vec::new(),
        http: reqwest::Client::builder()
            .user_agent("agenticd-test")
            .build()
            .unwrap(),
        peer_auth: Arc::new(agenticd::peer_auth::PeerAuthPolicy::InsecureAllowAny),
    });

    let out = commit::execute(
        Arc::clone(&state),
        commit_input_with_memory("with-memory"),
        None,
    )
    .await
    .expect("commit with memory should succeed");

    // Read the commit blob back and inspect its memory_snapshot field —
    // the dimension `snapshot_memory` is supposed to populate via
    // `store_async::put_raw`.
    let commit_hash: agentic_core::Hash = out.commit_hash.parse().unwrap();
    let log = walk_log(state.store.as_ref(), commit_hash, 1).unwrap();
    let (_h, c) = &log[0];

    let manifest_hash = c.memory_snapshot.expect(
        "memory_snapshot must be Some when a memory backend is attached and no_memory=false",
    );
    assert!(
        state.store.has(&manifest_hash),
        "the manifest hash returned by put_raw must be addressable in the same store \
         (proves Arc<store> was threaded correctly through store_async::put_raw)"
    );
    assert!(
        c.schema_version.is_some(),
        "schema_version must be populated alongside memory_snapshot"
    );

    drop_schema(&setup_pool, &schema).await;
}
