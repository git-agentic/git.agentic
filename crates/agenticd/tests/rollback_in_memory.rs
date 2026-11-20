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
use agentic_core::{FsObjectStore, Hash, Object, ObjectStore, Tree, TypedRef};
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
        // ADR-0019: a rollback that runs reverse migrations needs an
        // operator approval token.
        approval_key: Some(test_key()),
        used_approval_tokens: Default::default(),
        limits: agenticd::limits::LimitsConfig::default(),
        rate: agenticd::limits::RateLimiter::new(
            agenticd::limits::LimitsConfig::default().rate_per_uid,
        ),
        commit_slots: Arc::new(tokio::sync::Semaphore::new(
            agenticd::limits::LimitsConfig::default().commit_queue_depth,
        )),
        commit_queue_depth: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        exempt_entropy_prefixes: Vec::new(),
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
    let token = generate_token(&test_key(), &baseline_ref, 1000, now_secs());

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
            approval_token: Some(token),
            repo: repo_root.clone(),
        },
        Some(1000),
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
        used_approval_tokens: Default::default(),
        limits: agenticd::limits::LimitsConfig::default(),
        rate: agenticd::limits::RateLimiter::new(
            agenticd::limits::LimitsConfig::default().rate_per_uid,
        ),
        commit_slots: Arc::new(tokio::sync::Semaphore::new(
            agenticd::limits::LimitsConfig::default().commit_queue_depth,
        )),
        commit_queue_depth: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        exempt_entropy_prefixes: Vec::new(),
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

fn is_replayed(err: &anyhow::Error) -> bool {
    matches!(
        downcast_approval(err),
        ApprovalError::Rejected(agentic_core::approval::ApprovalRejection::Replayed)
    )
}

/// Non-dry-run destructive args whose `repo` is the real daemon repo, for
/// tests where the rollback must actually execute.
fn real_args(state: &DaemonState, target: &str, token: Option<String>) -> RollbackArgs {
    RollbackArgs {
        repo: state.repo_root.clone(),
        ..destructive_args(target, token)
    }
}

fn dry_run_args(state: &DaemonState, target: &str, token: String) -> RollbackArgs {
    RollbackArgs {
        target: target.to_string(),
        dry_run: true,
        accept_data_loss: true,
        approval_token: Some(token),
        repo: state.repo_root.clone(),
    }
}

#[tokio::test]
async fn gate_token_is_single_use_for_destructive_rollback() {
    let (state, _adapter, baseline, _dir) = setup_gate_state(true).await;
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    let first = rollback::execute(
        Arc::clone(&state),
        real_args(&state, &baseline, Some(tok.clone())),
        Some(1000),
    )
    .await
    .expect("first use of a valid token succeeds");
    assert!(first.executed);

    let tip = state.refs.read_branch("main").unwrap();
    let err = rollback::execute(
        Arc::clone(&state),
        real_args(&state, &baseline, Some(tok)),
        Some(1000),
    )
    .await
    .expect_err("second use of the same token must be rejected");
    assert!(is_replayed(&err), "got: {err:#}");
    assert_no_side_effects(&state, tip).await;
}

#[tokio::test]
async fn gate_dry_run_verifies_but_does_not_consume_token() {
    let (state, _adapter, baseline, _dir) = setup_gate_state(true).await;
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    // Several dry runs first: none consume.
    for _ in 0..2 {
        let out = rollback::execute(
            Arc::clone(&state),
            dry_run_args(&state, &baseline, tok.clone()),
            Some(1000),
        )
        .await
        .expect("dry run with a valid token succeeds");
        assert!(!out.executed);
    }
    let out = rollback::execute(
        Arc::clone(&state),
        real_args(&state, &baseline, Some(tok.clone())),
        Some(1000),
    )
    .await
    .expect("real run after dry runs still succeeds once");
    assert!(out.executed);
    let err = rollback::execute(
        Arc::clone(&state),
        real_args(&state, &baseline, Some(tok)),
        Some(1000),
    )
    .await
    .expect_err("second real run is a replay");
    assert!(is_replayed(&err), "got: {err:#}");
}

#[tokio::test]
async fn gate_replay_detected_across_token_encodings() {
    let (state, _adapter, baseline, _dir) = setup_gate_state(true).await;
    let ts = now_secs();
    let tok = generate_token(&test_key(), &baseline, 1000, ts);
    let hex_tag = tok.split_once(':').unwrap().1.to_string();
    rollback::execute(
        Arc::clone(&state),
        real_args(&state, &baseline, Some(tok)),
        Some(1000),
    )
    .await
    .expect("canonical token succeeds once");

    // Same tag, different wire spelling: still the same token.
    for variant in [
        format!("{ts}:{}", hex_tag.to_ascii_uppercase()),
        format!("+{ts}:{hex_tag}"),
        format!("000{ts}:{hex_tag}"),
    ] {
        let err = rollback::execute(
            Arc::clone(&state),
            real_args(&state, &baseline, Some(variant.clone())),
            Some(1000),
        )
        .await
        .expect_err("re-encoded token must still be a replay");
        assert!(is_replayed(&err), "variant {variant:?}: {err:#}");
    }
}

#[tokio::test]
async fn gate_prunes_expired_ledger_entries() {
    let (state, _adapter, baseline, _dir) = setup_gate_state(true).await;
    state
        .used_approval_tokens
        .lock()
        .unwrap()
        .insert([9u8; 32], now_secs() - 1);
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    rollback::execute(
        Arc::clone(&state),
        real_args(&state, &baseline, Some(tok)),
        Some(1000),
    )
    .await
    .expect("valid token succeeds");
    let used = state.used_approval_tokens.lock().unwrap();
    assert_eq!(used.len(), 1, "stale entry pruned, new entry recorded");
    assert!(!used.contains_key(&[9u8; 32]));
}

// ADR-0017: rollback forward-records by re-staging the target commit's
// prompt blobs. A baseline containing a high-entropy checkpoint blob
// under an exempt prefix must therefore roll back cleanly — without the
// exemption threading, the scanner rejects the rollback commit itself.
#[tokio::test]
async fn rollback_restages_exempt_checkpoint_blob() {
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path().to_path_buf();
    let agentic_dir = repo_root.join(".agentic");
    std::fs::create_dir_all(agentic_dir.join("objects")).unwrap();

    let store: Arc<dyn ObjectStore + Send + Sync> =
        Arc::new(FsObjectStore::open(agentic_dir.join("objects")).unwrap());
    let refs = Refs::open(&agentic_dir).unwrap();

    let state = Arc::new(DaemonState {
        repo_root: repo_root.clone(),
        store: Arc::clone(&store),
        refs,
        commit_lock: Arc::new(Mutex::new(())),
        shutdown: tokio_util::sync::CancellationToken::new(),
        memory: None,
        mcp_servers: Vec::new(),
        http: reqwest::Client::builder()
            .user_agent("agenticd-test")
            .build()
            .unwrap(),
        peer_auth: Arc::new(agenticd::peer_auth::PeerAuthPolicy::InsecureAllowAny),
        approval_key: None,
        used_approval_tokens: Default::default(),
        limits: agenticd::limits::LimitsConfig::default(),
        rate: agenticd::limits::RateLimiter::new(
            agenticd::limits::LimitsConfig::default().rate_per_uid,
        ),
        commit_slots: Arc::new(tokio::sync::Semaphore::new(
            agenticd::limits::LimitsConfig::default().commit_queue_depth,
        )),
        commit_queue_depth: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        exempt_entropy_prefixes: vec!["__langgraph__/".to_string()],
    });

    // Baseline commit carrying a high-entropy checkpoint blob.
    let mut baseline_input = commit_input_no_memory("baseline with checkpoint");
    baseline_input.prompts.insert(
        "__langgraph__/abc123/checkpoint.json".to_string(),
        b"data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi".to_vec(),
    );
    let baseline = commit::execute(Arc::clone(&state), baseline_input, None)
        .await
        .expect("baseline with exempt checkpoint blob must commit");

    // Second commit so rollback has somewhere to come back from.
    commit::execute(Arc::clone(&state), commit_input_no_memory("second"), None)
        .await
        .expect("second commit");

    // Roll back to the baseline: re-stages the checkpoint blob.
    let out = rollback::execute(
        Arc::clone(&state),
        RollbackArgs {
            target: baseline.commit_hash.clone(),
            dry_run: false,
            accept_data_loss: false,
            approval_token: None,
            repo: repo_root,
        },
        None,
    )
    .await
    .expect("rollback across an exempt checkpoint blob must succeed");
    assert!(out.executed);
}

fn commit_input_no_memory(message: &str) -> CommitInput {
    let mut input = commit_input_with_memory(message);
    input.no_memory = true;
    input
}

// ---------------------------------------------------------------------------
// Prompt write-back preflight: an unwritable prompts tree must be rejected
// before any reverse migration or memory restore (and on dry runs).
// ---------------------------------------------------------------------------

/// Commit a target whose prompts tree contains `prompt_name`, then dirty the
/// schema and memory, so a rollback to the target would have real work to
/// undo. Returns the target ref.
async fn commit_target_then_contaminate(
    state: &Arc<DaemonState>,
    adapter: &InMemoryAdapter,
    agentic_dir: &std::path::Path,
    prompt_name: &str,
) -> String {
    let mut input = commit_input_with_memory("target");
    input
        .prompts
        .insert(prompt_name.to_string(), b"payload".to_vec());
    let target = commit::execute(Arc::clone(state), input, Some(1000))
        .await
        .expect("target commit")
        .commit_hash;

    adapter.apply_migration("002_bump").await;
    adapter
        .insert_rows(
            "messages",
            vec![serde_json::json!({"id": 99, "body": "bad"})],
        )
        .await;
    let schema_dir = agentic_dir.join("schema");
    std::fs::create_dir_all(&schema_dir).unwrap();
    std::fs::write(schema_dir.join("002_bump.down.sql"), "-- no-op\n").unwrap();
    target
}

async fn assert_rollback_refused_before_side_effects(
    state: &Arc<DaemonState>,
    adapter: &InMemoryAdapter,
    repo: &std::path::Path,
    target: &str,
    expect: &str,
) {
    let tip = state.refs.read_branch("main").unwrap();
    let rows_before = adapter.rows_of("messages").await;
    for dry_run in [true, false] {
        let err = rollback::execute(
            Arc::clone(state),
            RollbackArgs {
                target: target.to_string(),
                dry_run,
                accept_data_loss: false,
                approval_token: None,
                repo: repo.to_path_buf(),
            },
            None,
        )
        .await
        .expect_err("unwritable prompts tree must be refused");
        assert!(
            format!("{err:#}").contains(expect),
            "dry_run={dry_run}: {err:#}"
        );
        assert_eq!(
            adapter.current_schema_version().await.unwrap(),
            "002_bump",
            "schema must not be reversed (dry_run={dry_run})"
        );
        assert_eq!(
            adapter.rows_of("messages").await,
            rows_before,
            "memory must not be restored (dry_run={dry_run})"
        );
        assert_eq!(state.refs.read_branch("main").unwrap(), tip);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn rollback_refuses_symlinked_prompts_parent_before_side_effects() {
    let (state, adapter, _baseline, dir) = setup_gate_state(false).await;
    let agentic_dir = dir.path().join(".agentic");
    let target = commit_target_then_contaminate(&state, &adapter, &agentic_dir, "sub/x").await;

    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("keep.txt"), b"outside").unwrap();
    std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("prompts/sub")).unwrap();

    assert_rollback_refused_before_side_effects(&state, &adapter, dir.path(), &target, "symlink")
        .await;
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn rollback_refuses_invalid_prompt_name_before_side_effects() {
    let (state, adapter, _baseline, dir) = setup_gate_state(false).await;
    let agentic_dir = dir.path().join(".agentic");
    let target = commit_target_then_contaminate(&state, &adapter, &agentic_dir, "../escape").await;

    assert_rollback_refused_before_side_effects(
        &state,
        &adapter,
        dir.path(),
        &target,
        "unsafe path component",
    )
    .await;
}

// ---------------------------------------------------------------------------
// Pre-upgrade stores: a prompt blob whose bytes are canonical Tree JSON.
// ---------------------------------------------------------------------------

/// Store `bytes` the way a pre-upgrade store did: bare bytes at the typed
/// path (`<objects>/<ab>/<rest>.zst`), which the current code reads as a
/// blob. `put_raw` already produces that exact compressed file, under
/// `raw/`, so copy it into the legacy place.
fn plant_legacy_blob(objects: &std::path::Path, store: &dyn ObjectStore, bytes: &[u8]) -> Hash {
    let hash = store
        .put_raw(agentic_core::ObjectKind::Blob, bytes)
        .unwrap();
    let (a, b) = hash.shard();
    let from = objects.join("raw").join(&a).join(format!("{b}.raw"));
    let to = objects.join(&a).join(format!("{b}.zst"));
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::copy(from, to).unwrap();
    hash
}

#[tokio::test]
async fn rollback_refuses_legacy_ambiguous_blob_before_any_side_effect() {
    let (state, adapter, baseline, _dir) = setup_gate_state(false).await;
    let agentic_dir = state.repo_root.join(".agentic");

    // A target commit whose prompts tree holds a legacy blob that is
    // exactly the canonical JSON of a Tree.
    let tree_json = serde_json::to_vec(&Tree::new()).unwrap();
    let blob_hash = plant_legacy_blob(
        &agentic_dir.join("objects"),
        state.store.as_ref(),
        &tree_json,
    );
    let mut prompts = Tree::new();
    prompts.insert(
        "system.md",
        TypedRef {
            kind: agentic_core::ObjectKind::Blob,
            hash: blob_hash,
        },
    );
    let prompts_hash = state.store.put(&Object::Tree(prompts)).unwrap();
    let baseline_hash = state.refs.resolve(&baseline).unwrap().unwrap();
    let Object::Commit(mut target) = state.store.get(&baseline_hash).unwrap() else {
        panic!("baseline is a commit");
    };
    target.prompts = Some(prompts_hash);
    target.message = "legacy ambiguous prompt".into();
    let target_hash = state.store.put(&Object::Commit(target)).unwrap();

    // Contaminate live state so a rollback that proceeded would be visible.
    adapter.apply_migration("002_bump").await;
    adapter
        .insert_rows(
            "messages",
            vec![serde_json::json!({"id": 99, "body": "bad"})],
        )
        .await;
    let schema_dir = agentic_dir.join("schema");
    std::fs::create_dir_all(&schema_dir).unwrap();
    std::fs::write(
        schema_dir.join("002_bump.down.sql"),
        "-- no-op for fixture\n",
    )
    .unwrap();
    let tip = state.refs.read_branch("main").unwrap();

    for dry_run in [true, false] {
        let err = rollback::execute(
            Arc::clone(&state),
            RollbackArgs {
                target: target_hash.to_hex(),
                dry_run,
                accept_data_loss: false,
                approval_token: None,
                repo: state.repo_root.clone(),
            },
            None,
        )
        .await
        .expect_err("a legacy ambiguous blob must be refused");
        assert!(
            err.chain().any(|e| matches!(
                e.downcast_ref::<agentic_core::Error>(),
                Some(agentic_core::Error::AmbiguousBlob { .. })
            )),
            "expected AmbiguousBlob (dry_run={dry_run}), got: {err:#}"
        );

        assert_eq!(
            adapter.current_schema_version().await.unwrap(),
            "002_bump",
            "schema must not be reversed (dry_run={dry_run})"
        );
        let rows = adapter.rows_of("messages").await;
        assert_eq!(rows.len(), 1, "memory must not be restored");
        assert_eq!(rows[0]["body"], "bad");
        assert_eq!(state.refs.read_branch("main").unwrap(), tip);
        assert!(!state.repo_root.join("prompts").exists());
    }
}

// ---------------------------------------------------------------------------
// ADR-0019: a rollback that runs reverse migrations passes the approval gate
// whether or not accept_data_loss is set.
// ---------------------------------------------------------------------------

/// Bump the schema, dirty the data and provide the down.sql, so a rollback to
/// the baseline has a reverse migration to run.
async fn contaminate(state: &DaemonState, adapter: &InMemoryAdapter) {
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
}

fn plain_args(
    state: &DaemonState,
    target: &str,
    token: Option<String>,
    dry_run: bool,
) -> RollbackArgs {
    RollbackArgs {
        target: target.to_string(),
        dry_run,
        accept_data_loss: false,
        approval_token: token,
        repo: state.repo_root.clone(),
    }
}

/// Nothing a rollback touches has changed: schema, memory, branch tip.
async fn assert_untouched(
    state: &DaemonState,
    adapter: &InMemoryAdapter,
    tip: Option<agentic_core::Hash>,
) {
    assert_eq!(adapter.current_schema_version().await.unwrap(), "002_bump");
    let rows = adapter.rows_of("messages").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["body"], "bad");
    assert_eq!(state.refs.read_branch("main").unwrap(), tip);
}

#[tokio::test]
async fn reverse_migration_rollback_without_token_is_refused_before_side_effects() {
    let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
    contaminate(&state, &adapter).await;
    let tip = state.refs.read_branch("main").unwrap();
    let err = rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, None, false),
        Some(1000),
    )
    .await
    .expect_err("a plain reverse-migration rollback needs a token");
    assert!(matches!(
        downcast_approval(&err),
        ApprovalError::TokenRequiredForReverseMigration
    ));
    assert_untouched(&state, &adapter, tip).await;
    assert!(
        !state.repo_root.join("prompts").exists(),
        "prompts must not be written back"
    );
}

#[tokio::test]
async fn reverse_migration_rollback_with_wrong_uid_or_no_peer_is_refused() {
    let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
    contaminate(&state, &adapter).await;
    let tip = state.refs.read_branch("main").unwrap();
    let tok = generate_token(&test_key(), &baseline, 2000, now_secs());
    let err = rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, Some(tok.clone()), false),
        Some(1000),
    )
    .await
    .expect_err("token for another uid must be refused");
    assert!(matches!(
        downcast_approval(&err),
        ApprovalError::Rejected(agentic_core::approval::ApprovalRejection::InvalidSignature)
    ));
    let err = rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, Some(tok), false),
        None,
    )
    .await
    .expect_err("no attested peer uid must be refused");
    assert!(matches!(downcast_approval(&err), ApprovalError::NoPeerUid));
    assert_untouched(&state, &adapter, tip).await;
}

#[tokio::test]
async fn reverse_migration_rollback_with_valid_token_succeeds_once() {
    let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
    contaminate(&state, &adapter).await;
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    let out = rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, Some(tok.clone()), false),
        Some(1000),
    )
    .await
    .expect("valid token lets the rollback run");
    assert!(out.executed);
    assert_eq!(adapter.current_schema_version().await.unwrap(), "001_init");

    // Single use: with the schema bumped again, the same token is a replay.
    contaminate(&state, &adapter).await;
    let tip = state.refs.read_branch("main").unwrap();
    let err = rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, Some(tok), false),
        Some(1000),
    )
    .await
    .expect_err("second use must be refused");
    assert!(is_replayed(&err), "got: {err:#}");
    assert_untouched(&state, &adapter, tip).await;
}

#[tokio::test]
async fn reverse_migration_rollback_without_a_key_names_the_flag() {
    let (state, adapter, baseline, _dir) = setup_gate_state(false).await;
    contaminate(&state, &adapter).await;
    let tip = state.refs.read_branch("main").unwrap();
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    let err = rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, Some(tok), false),
        Some(1000),
    )
    .await
    .expect_err("no key configured must refuse");
    assert!(matches!(
        downcast_approval(&err),
        ApprovalError::KeyNotConfigured
    ));
    assert!(
        format!("{err:#}").contains("--approval-key-file"),
        "message must name the flag: {err:#}"
    );
    assert_untouched(&state, &adapter, tip).await;
}

#[tokio::test]
async fn rollback_without_schema_change_needs_no_token() {
    // Key configured and no key configured: both run with no token.
    for with_key in [true, false] {
        let (state, _adapter, baseline, _dir) = setup_gate_state(with_key).await;
        let out = rollback::execute(
            Arc::clone(&state),
            plain_args(&state, &baseline, None, false),
            Some(1000),
        )
        .await
        .expect("no reverse migration, no approval needed");
        assert!(out.executed);
    }
}

#[tokio::test]
async fn reverse_migration_dry_run_reports_approval_and_verifies_a_supplied_token() {
    let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
    contaminate(&state, &adapter).await;
    let tip = state.refs.read_branch("main").unwrap();

    // No token: the plan says approval is required; nothing is refused.
    let out = rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, None, true),
        Some(1000),
    )
    .await
    .expect("a dry run without a token reports the requirement");
    assert!(!out.executed);
    assert!(
        out.planned_steps
            .iter()
            .any(|s| s.contains("approval token required")),
        "plan: {:?}",
        out.planned_steps
    );

    // A bad token is still verified on a dry run.
    let bad = generate_token(&test_key(), &baseline, 2000, now_secs());
    rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, Some(bad), true),
        Some(1000),
    )
    .await
    .expect_err("a supplied token is verified on a dry run");

    // A good token is verified but not consumed.
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    for _ in 0..2 {
        rollback::execute(
            Arc::clone(&state),
            plain_args(&state, &baseline, Some(tok.clone()), true),
            Some(1000),
        )
        .await
        .expect("dry run with a valid token");
    }
    assert_untouched(&state, &adapter, tip).await;
    let out = rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, Some(tok), false),
        Some(1000),
    )
    .await
    .expect("the token survived the dry runs");
    assert!(out.executed);
}

#[tokio::test]
async fn accept_data_loss_with_reverse_migration_consumes_one_token_once() {
    // Both gates apply; the single token must not be checked (and consumed)
    // twice within one request.
    let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
    contaminate(&state, &adapter).await;
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    let out = rollback::execute(
        Arc::clone(&state),
        real_args(&state, &baseline, Some(tok)),
        Some(1000),
    )
    .await
    .expect("one token covers accept_data_loss plus reverse migrations");
    assert!(out.executed);
    assert_eq!(state.used_approval_tokens.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn refused_down_migration_load_does_not_consume_the_token() {
    // The down.sql is checked before the gate, so a missing or IRREVERSIBLE
    // file is refused with the token unspent.
    let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
    contaminate(&state, &adapter).await;
    let down = state.refs.agentic_dir().join("schema/002_bump.down.sql");
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    let tip = state.refs.read_branch("main").unwrap();

    for (what, contents, expect) in [
        (
            "irreversible",
            Some("-- IRREVERSIBLE\nDROP TABLE t;\n"),
            "IRREVERSIBLE",
        ),
        ("missing", None, "002_bump"),
    ] {
        match contents {
            Some(c) => std::fs::write(&down, c).unwrap(),
            None => std::fs::remove_file(&down).unwrap(),
        }
        let err = rollback::execute(
            Arc::clone(&state),
            plain_args(&state, &baseline, Some(tok.clone()), false),
            Some(1000),
        )
        .await
        .expect_err("an unloadable down migration must refuse the rollback");
        assert!(format!("{err:#}").contains(expect), "{what}: {err:#}");
        assert!(
            state.used_approval_tokens.lock().unwrap().is_empty(),
            "{what}: the token must not be consumed"
        );
        assert_untouched(&state, &adapter, tip).await;
    }

    // With the file fixed, the same token still works.
    std::fs::write(&down, "-- no-op\n").unwrap();
    rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, Some(tok), false),
        Some(1000),
    )
    .await
    .expect("the unspent token authorizes the corrected rollback");
}

#[tokio::test]
async fn plans_report_whether_approval_is_required() {
    // Reverse migration in the plan: true, on a dry run with no token.
    let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
    contaminate(&state, &adapter).await;
    let out = rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, None, true),
        Some(1000),
    )
    .await
    .unwrap();
    assert!(out.approval_required);

    // No schema change and no accept_data_loss: false.
    let (state, _adapter, baseline, _dir) = setup_gate_state(true).await;
    let out = rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, None, true),
        Some(1000),
    )
    .await
    .unwrap();
    assert!(!out.approval_required);

    // accept_data_loss: true, even with no schema change.
    let tok = generate_token(&test_key(), &baseline, 1000, now_secs());
    let out = rollback::execute(
        Arc::clone(&state),
        dry_run_args(&state, &baseline, tok),
        Some(1000),
    )
    .await
    .unwrap();
    assert!(out.approval_required);
}

// ---------------------------------------------------------------------------
// ADR-0019 scope binding.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reverse_migration_token_cannot_authorize_accept_data_loss() {
    use agentic_core::approval::{generate_token_scoped, ApprovalScope};
    // The attack: a token issued for an ordinary reverse-migration rollback
    // replayed with accept_data_loss=true, which would switch off the
    // IRREVERSIBLE check. It must be refused before any SQL.
    let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
    contaminate(&state, &adapter).await;
    let down = state.refs.agentic_dir().join("schema/002_bump.down.sql");
    std::fs::write(&down, "-- IRREVERSIBLE\nDROP TABLE messages;\n").unwrap();
    let tip = state.refs.read_branch("main").unwrap();
    let rm = generate_token_scoped(
        &test_key(),
        ApprovalScope::ReverseMigration,
        &baseline,
        1000,
        now_secs(),
    );
    for dry_run in [true, false] {
        let args = RollbackArgs {
            dry_run,
            ..real_args(&state, &baseline, Some(rm.clone()))
        };
        let err = rollback::execute(Arc::clone(&state), args, Some(1000))
            .await
            .expect_err("a reverse-migration token must not authorize accept_data_loss");
        assert!(matches!(
            downcast_approval(&err),
            ApprovalError::Rejected(agentic_core::approval::ApprovalRejection::InvalidSignature)
        ));
    }
    assert!(state.used_approval_tokens.lock().unwrap().is_empty());
    assert_untouched(&state, &adapter, tip).await;
}

#[tokio::test]
async fn either_scope_authorizes_a_plain_reverse_migration_rollback() {
    use agentic_core::approval::{generate_token_scoped, ApprovalScope};
    for scope in [ApprovalScope::ReverseMigration, ApprovalScope::DataLoss] {
        let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
        contaminate(&state, &adapter).await;
        let tok = generate_token_scoped(&test_key(), scope, &baseline, 1000, now_secs());
        let out = rollback::execute(
            Arc::clone(&state),
            plain_args(&state, &baseline, Some(tok), false),
            Some(1000),
        )
        .await
        .unwrap_or_else(|e| panic!("{scope:?}: {e:#}"));
        assert!(out.executed, "{scope:?}");
        assert_eq!(adapter.current_schema_version().await.unwrap(), "001_init");
    }
}

#[tokio::test]
async fn legacy_format_token_still_authorizes_accept_data_loss() {
    // A token built the way pre-ADR-0019 issuers built it (the signing input
    // is pinned in agentic-core), not through the scoped API.
    use hmac::{Hmac, Mac};
    let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
    contaminate(&state, &adapter).await;
    let now = now_secs();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&TEST_KEY_BYTES).unwrap();
    mac.update(format!("git.agentic/rollback-approval/v1:{baseline}:1000:{now}").as_bytes());
    let legacy = format!("{now}:{}", hex::encode(mac.finalize().into_bytes()));
    let out = rollback::execute(
        Arc::clone(&state),
        real_args(&state, &baseline, Some(legacy)),
        Some(1000),
    )
    .await
    .expect("a legacy token keeps working for accept_data_loss");
    assert!(out.executed);
}

#[tokio::test]
async fn missing_token_diagnostics_name_the_right_scope() {
    // A plain reverse-migration rollback must point operators at the
    // reverse-migration scope, never at a data-loss token, which a worker
    // could replay with accept_data_loss. accept_data_loss points at data-loss.
    let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
    contaminate(&state, &adapter).await;
    let plain = rollback::execute(
        Arc::clone(&state),
        plain_args(&state, &baseline, None, false),
        Some(1000),
    )
    .await
    .expect_err("no token");
    let msg = format!("{plain:#}");
    assert!(msg.contains("--scope reverse-migration"), "{msg}");
    assert!(!msg.contains("--scope data-loss"), "{msg}");

    let destructive = rollback::execute(
        Arc::clone(&state),
        destructive_args(&baseline, None),
        Some(1000),
    )
    .await
    .expect_err("no token");
    let msg = format!("{destructive:#}");
    assert!(msg.contains("--scope data-loss"), "{msg}");
    assert!(!msg.contains("--scope reverse-migration"), "{msg}");
}

#[derive(Clone, Default)]
struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

/// The process-wide log buffer. A global subscriber, installed once, because
/// a thread-local default can lose to callsite-interest caching when other
/// tests in the binary hit the same callsites first.
fn audit_log() -> &'static LogBuf {
    static LOG: std::sync::OnceLock<LogBuf> = std::sync::OnceLock::new();
    LOG.get_or_init(|| {
        let buf = LogBuf::default();
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_writer(buf.clone())
                .with_ansi(false)
                .finish(),
        );
        buf
    })
}

#[tokio::test]
async fn accepted_audit_event_names_the_scope_the_token_verified_under() {
    use agentic_core::approval::{generate_token_scoped, ApprovalScope};
    let buf = audit_log();
    for (scope, expect) in [
        (
            ApprovalScope::ReverseMigration,
            "scope=\"reverse_migration\"",
        ),
        (ApprovalScope::DataLoss, "scope=\"data_loss\""),
    ] {
        let (state, adapter, baseline, _dir) = setup_gate_state(true).await;
        contaminate(&state, &adapter).await;
        // A timestamp no other test uses (they mint at now_secs()), still
        // inside the TTL: tokens are deterministic in (key, scope, commit,
        // uid, timestamp), so another test sharing the baseline hash and the
        // second would otherwise mint a byte-identical token.
        let tok = generate_token_scoped(&test_key(), scope, &baseline, 1000, now_secs() - 37);
        // The audited token_prefix is the first 8 hex chars of the tag: unique
        // to this scope's token, even if another test shares the commit hash.
        let prefix = tok.split_once(':').unwrap().1[..8].to_string();
        rollback::execute(
            Arc::clone(&state),
            plain_args(&state, &baseline, Some(tok), false),
            Some(1000),
        )
        .await
        .unwrap();
        // Other tests log into the same buffer; find this run's event by it.
        let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let line = log
            .lines()
            .find(|l| {
                l.contains("RollbackForcedDataLoss")
                    && l.contains(&prefix)
                    && l.contains("reason=\"reverse_migration\"")
            })
            .unwrap_or_else(|| panic!("no audit event for {prefix} in: {log}"));
        assert!(line.contains("decision=\"accepted\""), "{line}");
        assert!(line.contains(expect), "{expect}: {line}");
        assert!(line.contains("reason=\"reverse_migration\""), "{line}");
    }
}
