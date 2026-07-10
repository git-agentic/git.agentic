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
        approval_key: None,
        limits: agenticd::limits::LimitsConfig::default(),
        rate: agenticd::limits::RateLimiter::new(
            agenticd::limits::LimitsConfig::default().rate_per_uid,
        ),
        commit_slots: Arc::new(tokio::sync::Semaphore::new(
            agenticd::limits::LimitsConfig::default().commit_queue_depth,
        )),
        commit_queue_depth: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
            approval_token: None,
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

// ---------------------------------------------------------------------------
// ADR-0014 destructive-rollback approval gate.
// ---------------------------------------------------------------------------

use agentic_core::approval::{generate_token, ApprovalKey};
use agenticd::rollback::ApprovalError;

const TEST_KEY_BYTES: [u8; 32] = [42u8; 32];

fn test_key() -> ApprovalKey {
    ApprovalKey::from_bytes(&TEST_KEY_BYTES).unwrap()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn downcast_approval(err: &anyhow::Error) -> &ApprovalError {
    err.chain()
        .find_map(|e| e.downcast_ref::<ApprovalError>())
        .unwrap_or_else(|| panic!("expected ApprovalError, got: {err:#}"))
}

/// Build an in-memory-backed daemon state (optionally with the approval
/// key) and commit a baseline so there's a resolvable rollback target.
/// Returns `(state, adapter, baseline_hash_hex, tempdir)` — the caller must
/// keep the `TempDir` alive for the test's duration; it cleans up on drop.
async fn setup_gate_state(
    with_key: bool,
) -> (
    Arc<DaemonState>,
    Arc<InMemoryAdapter>,
    String,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let agentic_dir = repo_root.join(".agentic");
    std::fs::create_dir_all(agentic_dir.join("objects")).unwrap();

    let store: Arc<dyn ObjectStore + Send + Sync> =
        Arc::new(FsObjectStore::open(agentic_dir.join("objects")).unwrap());
    let refs = Refs::open(&agentic_dir).unwrap();
    let adapter = Arc::new(InMemoryAdapter::new(Arc::clone(&store)));
    adapter.apply_migration("001_init").await;

    let state = DaemonState {
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
        approval_key: None,
        limits: agenticd::limits::LimitsConfig::default(),
        rate: agenticd::limits::RateLimiter::new(
            agenticd::limits::LimitsConfig::default().rate_per_uid,
        ),
        commit_slots: Arc::new(tokio::sync::Semaphore::new(
            agenticd::limits::LimitsConfig::default().commit_queue_depth,
        )),
        commit_queue_depth: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let state = if with_key {
        state.with_approval_key(Some(test_key()))
    } else {
        state
    };
    let state = Arc::new(state);

    let baseline = commit::execute(
        Arc::clone(&state),
        commit_input_with_memory("baseline"),
        Some(1000),
    )
    .await
    .expect("baseline commit");
    (state, adapter, baseline.commit_hash, dir)
}

fn destructive_args(target: &str, token: Option<String>) -> RollbackArgs {
    RollbackArgs {
        target: target.to_string(),
        dry_run: false,
        accept_data_loss: true,
        approval_token: token,
        repo: std::path::PathBuf::from("/nonexistent-should-not-be-reached"),
    }
}

/// The gate rejecting must leave the branch tip untouched — proof of "zero
/// side effects before rejection".
async fn assert_no_side_effects(state: &DaemonState, before_tip: Option<agentic_core::Hash>) {
    assert_eq!(
        state.refs.read_branch("main").unwrap(),
        before_tip,
        "a rejected destructive rollback must not forward-record a commit"
    );
}

#[tokio::test]
async fn gate_rejects_when_no_key_configured() {
    let (state, _adapter, baseline, _dir) = setup_gate_state(false).await;
    let tip = state.refs.read_branch("main").unwrap();
    // A well-formed token is present but the daemon has no key → still fail
    // closed (Decision 4): the key, not the token, is the gate.
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    let err = rollback::execute(
        Arc::clone(&state),
        destructive_args(&baseline, Some(tok)),
        Some(1000),
    )
    .await
    .expect_err("no-key must reject");
    assert!(matches!(
        downcast_approval(&err),
        ApprovalError::KeyNotConfigured
    ));
    assert_no_side_effects(&state, tip).await;
}

#[tokio::test]
async fn gate_rejects_when_peer_uid_absent() {
    let (state, _adapter, baseline, _dir) = setup_gate_state(true).await;
    let tip = state.refs.read_branch("main").unwrap();
    // Insecure mode: peer_uid is None. Even a key + token can't bind.
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    let err = rollback::execute(
        Arc::clone(&state),
        destructive_args(&baseline, Some(tok)),
        None,
    )
    .await
    .expect_err("absent peer uid must reject");
    assert!(matches!(downcast_approval(&err), ApprovalError::NoPeerUid));
    assert_no_side_effects(&state, tip).await;
}

#[tokio::test]
async fn gate_rejects_when_token_missing() {
    let (state, _adapter, baseline, _dir) = setup_gate_state(true).await;
    let tip = state.refs.read_branch("main").unwrap();
    let err = rollback::execute(
        Arc::clone(&state),
        destructive_args(&baseline, None),
        Some(1000),
    )
    .await
    .expect_err("missing token must reject");
    assert!(matches!(
        downcast_approval(&err),
        ApprovalError::TokenRequired
    ));
    assert_no_side_effects(&state, tip).await;
}

#[tokio::test]
async fn gate_rejects_wrong_uid_token() {
    let (state, _adapter, baseline, _dir) = setup_gate_state(true).await;
    let tip = state.refs.read_branch("main").unwrap();
    // Token bound to uid 2000, but the connection's uid is 1000.
    let tok = generate_token(&test_key(), &baseline, 2000, now_secs());
    let err = rollback::execute(
        Arc::clone(&state),
        destructive_args(&baseline, Some(tok)),
        Some(1000),
    )
    .await
    .expect_err("wrong-uid token must reject");
    assert!(matches!(
        downcast_approval(&err),
        ApprovalError::Rejected(agentic_core::approval::ApprovalRejection::InvalidSignature)
    ));
    assert_no_side_effects(&state, tip).await;
}

#[tokio::test]
async fn gate_rejects_before_dry_run_branch() {
    // ADR-0014 Decision 1: the gate is evaluated before the dry-run
    // branch, so a *dry-run* destructive rollback with no token is still
    // rejected — you can't probe a destructive plan without approval.
    let (state, _adapter, baseline, _dir) = setup_gate_state(true).await;
    let tip = state.refs.read_branch("main").unwrap();
    let args = RollbackArgs {
        target: baseline.clone(),
        dry_run: true,
        accept_data_loss: true,
        approval_token: None,
        repo: state.repo_root.clone(),
    };
    let err = rollback::execute(Arc::clone(&state), args, Some(1000))
        .await
        .expect_err("dry-run must not bypass the gate");
    assert!(matches!(
        downcast_approval(&err),
        ApprovalError::TokenRequired
    ));
    assert_no_side_effects(&state, tip).await;
}

#[tokio::test]
async fn gate_rejects_expired_token() {
    let (state, _adapter, baseline, _dir) = setup_gate_state(true).await;
    let tip = state.refs.read_branch("main").unwrap();
    // Timestamp far in the past → outside the 300s window.
    let tok = generate_token(
        &test_key(),
        &baseline,
        1000,
        now_secs().saturating_sub(10_000),
    );
    let err = rollback::execute(
        Arc::clone(&state),
        destructive_args(&baseline, Some(tok)),
        Some(1000),
    )
    .await
    .expect_err("expired token must reject");
    assert!(matches!(
        downcast_approval(&err),
        ApprovalError::Rejected(agentic_core::approval::ApprovalRejection::Expired { .. })
    ));
    assert_no_side_effects(&state, tip).await;
}

#[tokio::test]
async fn gate_rejects_malformed_token() {
    let (state, _adapter, baseline, _dir) = setup_gate_state(true).await;
    let tip = state.refs.read_branch("main").unwrap();
    let err = rollback::execute(
        Arc::clone(&state),
        destructive_args(&baseline, Some("not-a-valid-token".into())),
        Some(1000),
    )
    .await
    .expect_err("malformed token must reject");
    assert!(matches!(
        downcast_approval(&err),
        ApprovalError::Rejected(agentic_core::approval::ApprovalRejection::Malformed)
    ));
    assert_no_side_effects(&state, tip).await;
}

#[tokio::test]
async fn gate_accepts_valid_token_and_rollback_proceeds() {
    // Full happy path: valid token → gate passes → destructive rollback
    // executes end-to-end (schema reversed, forward-record commit written).
    let (state, adapter, baseline, _dir) = setup_gate_state(true).await;

    // Contaminate: bump schema + dirty data, and provide the down.sql the
    // reverse-migration loader reads.
    adapter.apply_migration("002_bump").await;
    adapter
        .insert_rows(
            "messages",
            vec![serde_json::json!({"id": 99, "body": "bad"})],
        )
        .await;
    let schema_dir = state.refs.agentic_dir().join("schema");
    std::fs::create_dir_all(&schema_dir).unwrap();
    std::fs::write(schema_dir.join("002_bump.down.sql"), "-- no-op\n").unwrap();

    let tip_before = state.refs.read_branch("main").unwrap();
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    let args = RollbackArgs {
        target: baseline.clone(),
        dry_run: false,
        accept_data_loss: true,
        approval_token: Some(tok),
        repo: state.repo_root.clone(),
    };
    let out = rollback::execute(Arc::clone(&state), args, Some(1000))
        .await
        .expect("valid token should let the destructive rollback proceed");

    assert!(out.executed);
    assert!(
        out.new_head_hash.is_some(),
        "accepted rollback forward-records"
    );
    assert_ne!(
        state.refs.read_branch("main").unwrap(),
        tip_before,
        "the branch tip must advance past the rollback"
    );
    assert_eq!(
        adapter.current_schema_version().await.unwrap(),
        "001_init",
        "schema reversed to the target after an approved destructive rollback"
    );
}
