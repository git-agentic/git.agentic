//! Rollback end-to-end against the InMemoryAdapter — the trait-level
//! proof that the daemon's schema + memory rollback path works for a
//! backend that isn't Postgres (issue #43 acceptance criterion 3).
//!
//! Unlike `commit_with_memory.rs`, this test requires no Docker/Postgres:
//! `InMemoryAdapter` is a pure in-process fixture, so the whole scenario
//! (commit a baseline, contaminate schema + data, roll back, assert all
//! three dimensions came back) runs under plain `cargo test`.

use std::sync::Arc;

use agentic_core::refs::Refs;
use agentic_core::{FsObjectStore, ObjectStore};
use agentic_memory::in_memory::InMemoryAdapter;
use agentic_memory::MemoryAdapter;
use agentic_proto::CommitInput;
use agenticd::commit;
use agenticd::rollback::{self, RollbackArgs};
use agenticd::server::DaemonState;
use tokio::sync::Mutex;

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
        // Opt into memory snapshotting so the rollback path has a
        // memory_snapshot + schema_version to reverse.
        no_memory: false,
    }
}

#[tokio::test]
async fn rollback_reverses_schema_and_restores_memory_in_memory_backend() {
    // -- Arrange: adapter at schema 001 with clean rows, committed. ------
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let agentic_dir = repo_root.join(".agentic");
    std::fs::create_dir_all(&agentic_dir).unwrap();
    std::fs::create_dir_all(agentic_dir.join("objects")).unwrap();

    let store: Arc<dyn ObjectStore + Send + Sync> =
        Arc::new(FsObjectStore::open(agentic_dir.join("objects")).unwrap());
    let refs = Refs::open(&agentic_dir).unwrap();

    let adapter = Arc::new(InMemoryAdapter::new(Arc::clone(&store)));
    adapter.apply_migration("001_init").await;
    adapter
        .insert_rows(
            "messages",
            vec![serde_json::json!({"id": 1, "body": "clean"})],
        )
        .await;
    // NOTE: InMemoryAdapter::init is a no-op, so no init call is needed
    // before the coercion.

    let state = Arc::new(DaemonState {
        repo_root: repo_root.clone(),
        store: Arc::clone(&store),
        refs,
        commit_lock: Arc::new(Mutex::new(())),
        shutdown: tokio_util::sync::CancellationToken::new(),
        memory: Some(adapter.clone() as Arc<dyn MemoryAdapter>),
        mcp_servers: Vec::new(),
        http: reqwest::Client::builder()
            .user_agent("agenticd-test")
            .build()
            .unwrap(),
        peer_auth: Arc::new(agenticd::peer_auth::PeerAuthPolicy::InsecureAllowAny),
    });

    // Commit the baseline (schema_version "001_init", memory snapshot).
    let baseline = commit::execute(
        Arc::clone(&state),
        commit_input_with_memory("baseline"),
        None,
    )
    .await
    .expect("baseline commit with memory should succeed");
    let baseline_ref = baseline.commit_hash.clone();

    // -- Act 1: contaminate — bump schema and dirty the data. ------------
    adapter.apply_migration("002_bump").await;
    adapter
        .insert_rows(
            "messages",
            vec![serde_json::json!({"id": 99, "body": "bad"})],
        )
        .await;

    // The reverse-migration loader reads <agentic_dir>/schema/002_bump.down.sql;
    // the fixture ignores SQL, but the file must exist and pass the
    // IRREVERSIBLE check.
    let schema_dir = agentic_dir.join("schema");
    std::fs::create_dir_all(&schema_dir).unwrap();
    std::fs::write(
        schema_dir.join("002_bump.down.sql"),
        "-- no-op for fixture\n",
    )
    .unwrap();

    // -- Act 2: roll back to the baseline commit. -------------------------
    let out = rollback::execute(
        Arc::clone(&state),
        RollbackArgs {
            target: baseline_ref,
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
    assert!(
        out.new_head_hash.is_some(),
        "rollback forward-records a commit"
    );
}
