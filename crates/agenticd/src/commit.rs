//! Commit orchestrator — the daemon-side phases of an `agentic commit`.
//!
//! Audit §A3 / §S2: this module replaces the previous inlined
//! `handle_commit` in `server.rs`. The work splits cleanly into five
//! named phases that mirror `rollback::execute`'s structure (acquire
//! lock at the dispatch layer, run phased orchestration, return typed
//! output):
//!
//!   1. [`snapshot_memory`] — capture the Postgres memory snapshot (or
//!      `(None, None)` if `--no-memory` was set or no backend is
//!      attached) and persist its manifest as a raw object.
//!   2. [`fingerprint_tools`] — call every configured MCP server's
//!      `tools/list` endpoint, canonicalise the responses, and key them
//!      by server name so the downstream tree builder is deterministic.
//!   3. [`assemble_inputs`] — fold the daemon-side phase outputs plus
//!      the wire `CommitInput` into an `agentic_core::CommitInputs`.
//!   4. **stage_and_commit** (delegated to `agentic_core::commit`) —
//!      stage every dimension's blob, build the Commit blob, push it,
//!      update the branch ref. The single commit point of ADR-0002 D3.
//!   5. [`publish_head`] — on a first-ever commit, write `HEAD ->
//!      refs/heads/<branch>` AFTER the commit is durable. Phantom-HEAD
//!      avoidance is structural: the HEAD write happens only on the
//!      success path (audit §B7, fixed in A2's PR #50).
//!
//! Phases 1 and 2 are agenticd-specific (memory + MCP integration).
//! Phases 3–4 are pure orchestration over `agentic-core::stage_and_commit`.
//! Phase 5 is a single-line ref write that lives here so the success
//! path is visible at the orchestrator level.

use std::collections::BTreeMap;
use std::sync::Arc;

use agentic_core::commit::{stage_and_commit_with_now, CommitInputs};
use agentic_core::refs::HeadRef;
use agentic_core::ObjectKind;
use agentic_memory::MemoryAdapter;
use agentic_proto::{CommitInput, CommitOutput};
use anyhow::Context;

use crate::mcp::fingerprint_all;
use crate::server::DaemonState;

/// Run the commit orchestration end-to-end. Called from the dispatcher
/// in `server.rs` after `commit_lock` is acquired and the shutdown
/// gate has been checked.
pub async fn execute(
    state: Arc<DaemonState>,
    input: CommitInput,
    peer_uid: Option<u32>,
) -> anyhow::Result<CommitOutput> {
    execute_with_now(state, input, peer_uid, chrono::Utc::now()).await
}

/// `execute` with the wall-clock injection point exposed for the
/// determinism test in this module. Threads `now` through to
/// `stage_and_commit_with_now` so identical `(input, now)` yields the
/// same `commit_hash`. Audit §B4.
pub async fn execute_with_now(
    state: Arc<DaemonState>,
    input: CommitInput,
    peer_uid: Option<u32>,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<CommitOutput> {
    let head = state
        .refs
        .read_head()
        .context("reading HEAD ref before commit")?;
    let branch = input.branch.clone().unwrap_or_else(|| match &head {
        Some(HeadRef::Branch(b)) => b.clone(),
        _ => "main".to_string(),
    });
    // Audit §B7 (A2 fix carried over): on a first-ever commit defer
    // the HEAD write until after `stage_and_commit_with_now` returns
    // Ok. Tracks whether we need to write HEAD on the success path.
    let needs_head_write = head.is_none();

    let parent = state
        .refs
        .read_branch(&branch)
        .with_context(|| format!("reading branch ref {branch:?} before commit"))?;

    // -- Phase 1: snapshot memory ---------------------------------------
    let (memory_snapshot, schema_version) = snapshot_memory(&state, input.no_memory).await?;

    // -- Phase 2: fingerprint MCP tools ---------------------------------
    let tools = fingerprint_tools(&state).await?;

    // -- Phase 3: assemble core CommitInputs ----------------------------
    let inputs = assemble_inputs(
        input,
        parent,
        memory_snapshot,
        schema_version,
        tools,
        peer_uid,
    );

    // -- Phase 4: stage + commit + ref update (agentic-core) ------------
    //
    // `stage_and_commit_with_now` is fully synchronous and, under
    // GcsObjectStore, blocks the calling thread on every put/put_raw.
    // Wrap the whole 2PC sequence in spawn_blocking so the LocalSet
    // thread stays free for other connections during the staging
    // window. The closure owns its captures (Arc<DaemonState> clone,
    // CommitInputs, branch name) so it can be `Send + 'static`.
    // Audit §A5 / B2 / C1 / R3.
    let out = {
        let state_for_staging = Arc::clone(&state);
        let branch_owned = branch.clone();
        tokio::task::spawn_blocking(move || {
            stage_and_commit_with_now(
                state_for_staging.store.as_ref(),
                &state_for_staging.refs,
                &branch_owned,
                inputs,
                now,
            )
        })
        .await
        .with_context(|| format!("spawn_blocking join error during 2PC on branch {branch:?}"))?
        .with_context(|| format!("2PC staging on branch {branch:?}"))?
    };

    // -- Phase 5: publish HEAD on first commit --------------------------
    publish_head(&state, &branch, &out.commit_hash, needs_head_write);

    Ok(CommitOutput {
        commit_hash: out.commit_hash.to_hex(),
        branch: out.branch,
    })
}

/// Capture a memory snapshot under the commit lock and persist its
/// manifest to the object store. Returns `(None, None)` when
/// `no_memory` was requested, when no memory backend is attached, or
/// when both conditions hold. The two outputs are always either both
/// `Some` or both `None` — server.rs:302-312's invariant the
/// rollback path's B9 validation depends on (audit §B9 / A8 D-1).
async fn snapshot_memory(
    state: &Arc<DaemonState>,
    no_memory: bool,
) -> anyhow::Result<(Option<agentic_core::Hash>, Option<String>)> {
    if no_memory {
        return Ok((None, None));
    }
    let Some(memory) = state.memory.as_ref().map(Arc::clone) else {
        return Ok((None, None));
    };
    let adapter = memory.lock_owned().await;
    let handle = adapter.snapshot().await.context("taking memory snapshot")?;
    let manifest_bytes = handle.manifest.to_canonical_bytes();
    // Wrap the put_raw in spawn_blocking — under GcsObjectStore this is
    // a blocking HTTP PUT that would otherwise freeze the LocalSet
    // thread. Audit §A5 / B3 / R3.
    let manifest_hash =
        crate::store_async::put_raw(Arc::clone(&state.store), ObjectKind::Tree, manifest_bytes)
            .await
            .context("persisting segment manifest")?;
    Ok((Some(manifest_hash), Some(handle.schema_version)))
}

/// Fingerprint every configured MCP server in turn. Returns an empty
/// map when no servers are configured; otherwise the canonical
/// manifest bytes keyed by server name. A per-server failure is
/// propagated — partial success would corrupt the tools-tree hash
/// relative to the supposed commit state.
async fn fingerprint_tools(state: &Arc<DaemonState>) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    if state.mcp_servers.is_empty() {
        return Ok(BTreeMap::new());
    }
    let fingerprints = fingerprint_all(&state.http, &state.mcp_servers).await;
    let mut tools_map = BTreeMap::new();
    for (spec, result) in state.mcp_servers.iter().zip(fingerprints) {
        let fp = result.with_context(|| format!("fingerprinting MCP server {}", spec.name))?;
        tools_map.insert(fp.name, fp.canonical_manifest);
    }
    Ok(tools_map)
}

/// Fold daemon-side phase outputs plus the wire `CommitInput` into an
/// `agentic_core::CommitInputs`. Pure data plumbing — no I/O.
fn assemble_inputs(
    input: CommitInput,
    parent: Option<agentic_core::Hash>,
    memory_snapshot: Option<agentic_core::Hash>,
    schema_version: Option<String>,
    tools: BTreeMap<String, Vec<u8>>,
    peer_uid: Option<u32>,
) -> CommitInputs {
    let prompts = input
        .prompts
        .into_iter()
        .map(|(name, body)| (name, body.into_bytes()))
        .collect();
    CommitInputs {
        author: input.author.unwrap_or_else(|| "unknown".to_string()),
        message: input.message,
        parent,
        code_sha: input.code_sha,
        prompts,
        tools,
        model: input.model,
        memory_snapshot,
        schema_version,
        intent: None,
        plan: None,
        transcript: None,
        evals: None,
        cost_cents: 0,
        peer_uid,
    }
}

/// On a first-ever commit, publish `HEAD -> refs/heads/<branch>` AFTER
/// the branch ref has been pointed at the new commit. On non-first
/// commits this is a no-op.
///
/// Failure to write HEAD is logged but does NOT propagate. The commit
/// is already durable on the branch ref; failing the response would
/// tell the client "your commit didn't happen" and a naive retry would
/// create a duplicate on top of the new tip. HEAD is operator-recoverable
/// metadata (`echo 'ref: refs/heads/<branch>' > .agentic/HEAD`).
/// Audit §B7 / Copilot review on PR #50.
fn publish_head(
    state: &DaemonState,
    branch: &str,
    commit_hash: &agentic_core::Hash,
    needs_head_write: bool,
) {
    if !needs_head_write {
        return;
    }
    if let Err(e) = state.refs.write_head_symbolic(branch) {
        tracing::error!(
            error = %e,
            branch = %branch,
            commit_hash = %commit_hash,
            "commit succeeded on branch ref but HEAD write failed; \
             HEAD remains uninitialised. Operator can fix with: \
             `echo 'ref: refs/heads/<branch>' > .agentic/HEAD`"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentic_core::{FsObjectStore, ObjectStore};
    use std::sync::Arc;

    /// Build a minimal DaemonState pointing at a tempdir: no Postgres,
    /// no MCP servers, no MCP HTTP traffic. Suitable for unit tests of
    /// the commit orchestrator that don't exercise phases 1 or 2.
    async fn make_state(repo: &std::path::Path) -> Arc<DaemonState> {
        let agentic_dir = repo.join(".agentic");
        std::fs::create_dir_all(&agentic_dir).unwrap();
        let store: Arc<dyn ObjectStore + Send + Sync> =
            Arc::new(FsObjectStore::open(agentic_dir.join("objects")).unwrap());
        Arc::new(
            DaemonState::open(
                repo.to_path_buf(),
                agentic_dir,
                store,
                None,       // no postgres
                Vec::new(), // no tracked tables
                Vec::new(), // no MCP servers
                Arc::new(crate::peer_auth::PeerAuthPolicy::InsecureAllowAny),
            )
            .await
            .unwrap(),
        )
    }

    fn commit_input(message: &str) -> CommitInput {
        let mut prompts = std::collections::BTreeMap::new();
        prompts.insert("system.md".to_string(), "you are helpful".to_string());
        CommitInput {
            message: message.to_string(),
            author: Some("tester".to_string()),
            code_sha: Some("deadbeef".to_string()),
            branch: Some("main".to_string()),
            prompts,
            mcp_servers: Vec::new(),
            model: Some("anthropic:claude-opus:2026-05-01".to_string()),
            no_memory: true,
        }
    }

    // AC for issue #38 / audit §A3 / §B4: same input + same `now`
    // produces the same `commit_hash`. The orchestrator threads `now`
    // through to agentic-core's `stage_and_commit_with_now`; this
    // test exercises the full agenticd-level dispatch including
    // memory snapshot (no-op here) and MCP fingerprinting (no-op
    // here) and asserts hash equality across two independent
    // tempdirs.
    #[tokio::test]
    async fn execute_with_now_is_deterministic() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let state_a = make_state(dir_a.path()).await;
        let state_b = make_state(dir_b.path()).await;

        let fixed_now = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let out_a = execute_with_now(state_a, commit_input("deterministic"), None, fixed_now)
            .await
            .unwrap();
        let out_b = execute_with_now(state_b, commit_input("deterministic"), None, fixed_now)
            .await
            .unwrap();

        assert_eq!(
            out_a.commit_hash, out_b.commit_hash,
            "same CommitInput + same now must produce same commit hash"
        );
        assert_eq!(out_a.branch, "main");
    }

    // The HEAD-publish phase: on a first commit, HEAD is written
    // symbolically to refs/heads/<branch> AFTER stage_and_commit
    // succeeds. Confirms the B7 fix from A2 is preserved across the
    // extraction.
    #[tokio::test]
    async fn execute_publishes_head_on_first_commit() {
        let dir = tempfile::tempdir().unwrap();
        let state = make_state(dir.path()).await;
        assert!(
            state.refs.read_head().unwrap().is_none(),
            "fresh repo has no HEAD"
        );

        let out = execute(state.clone(), commit_input("first"), None)
            .await
            .unwrap();

        let head = state.refs.read_head().unwrap();
        assert!(
            matches!(head, Some(HeadRef::Branch(ref b)) if b == "main"),
            "HEAD should chase refs/heads/main after first commit; got {head:?}"
        );
        assert_eq!(out.branch, "main");
    }

    // Two distinct commits on the same branch: parent linkage works
    // and HEAD is only written on the first one (no error on second).
    // Reads the second commit's blob back via walk_log so the assertion
    // actually verifies the parent pointer rather than just inequality
    // of hashes. (Test-analyzer review on PR #52.)
    #[tokio::test]
    async fn execute_chains_commits_on_same_branch() {
        use agentic_core::commit::walk_log;
        let dir = tempfile::tempdir().unwrap();
        let state = make_state(dir.path()).await;

        let first = execute(state.clone(), commit_input("first"), None)
            .await
            .unwrap();
        let second = execute(state.clone(), commit_input("second"), None)
            .await
            .unwrap();

        assert_ne!(first.commit_hash, second.commit_hash);
        let tip = state.refs.read_branch("main").unwrap().unwrap();
        assert_eq!(tip.to_hex(), second.commit_hash);

        // Walk the log: second.parent must be first.commit_hash.
        let first_hash: agentic_core::Hash = first.commit_hash.parse().unwrap();
        let second_hash: agentic_core::Hash = second.commit_hash.parse().unwrap();
        let log = walk_log(state.store.as_ref(), second_hash, 10).unwrap();
        assert_eq!(log.len(), 2, "two commits should be visible in the log");
        assert_eq!(log[0].0, second_hash);
        assert_eq!(log[1].0, first_hash);
        assert_eq!(
            log[0].1.parent,
            Some(first_hash),
            "second commit's parent must point at the first commit"
        );
    }

    // assemble_inputs is a pure function — no I/O, no async. Verifies
    // the wire→core fold preserves all dimensions, defaults author to
    // "unknown" when CommitInput.author is None, encodes prompt
    // strings to Vec<u8>, hardcodes intent/plan/transcript/evals to
    // None, and zeroes cost_cents. (Test-analyzer review on PR #52.)
    #[test]
    fn assemble_inputs_folds_wire_input_into_core_inputs() {
        let mut prompts = std::collections::BTreeMap::new();
        prompts.insert("system.md".to_string(), "you are helpful".to_string());
        prompts.insert("user.md".to_string(), "do the thing".to_string());

        let mut tools = BTreeMap::new();
        tools.insert("search".to_string(), b"{\"tools\":[]}".to_vec());

        let parent_hash = agentic_core::Hash::of(b"parent-fixture");
        let manifest_hash = agentic_core::Hash::of(b"manifest-fixture");

        let input = CommitInput {
            message: "hello".to_string(),
            author: None, // exercise the "unknown" default
            code_sha: Some("abc123".to_string()),
            branch: Some("main".to_string()),
            prompts,
            mcp_servers: Vec::new(),
            model: Some("anthropic:claude-opus:2026-05-01".to_string()),
            no_memory: false,
        };

        let out = assemble_inputs(
            input,
            Some(parent_hash),
            Some(manifest_hash),
            Some("003_add_embeddings".to_string()),
            tools.clone(),
            Some(1234),
        );

        assert_eq!(
            out.author, "unknown",
            "missing author defaults to 'unknown'"
        );
        assert_eq!(out.message, "hello");
        assert_eq!(out.parent, Some(parent_hash));
        assert_eq!(out.code_sha.as_deref(), Some("abc123"));
        assert_eq!(out.prompts.len(), 2);
        assert_eq!(
            out.prompts.get("system.md").map(Vec::as_slice),
            Some(b"you are helpful".as_slice()),
            "prompt String must encode to Vec<u8>"
        );
        assert_eq!(out.tools, tools, "tools map is forwarded verbatim");
        assert_eq!(
            out.model.as_deref(),
            Some("anthropic:claude-opus:2026-05-01")
        );
        assert_eq!(out.memory_snapshot, Some(manifest_hash));
        assert_eq!(out.schema_version.as_deref(), Some("003_add_embeddings"));
        // Dimensions the daemon doesn't yet populate — must stay None
        // so the Commit blob is stable across versions until ADR-0002's
        // platform extensions are wired through the daemon.
        assert!(out.intent.is_none());
        assert!(out.plan.is_none());
        assert!(out.transcript.is_none());
        assert!(out.evals.is_none());
        assert_eq!(out.cost_cents, 0);
        // peer_uid is propagated verbatim from the dispatch context.
        assert_eq!(out.peer_uid, Some(1234));
    }

    #[test]
    fn assemble_inputs_uses_explicit_author_when_supplied() {
        let mut prompts = std::collections::BTreeMap::new();
        prompts.insert("system.md".to_string(), "x".to_string());
        let input = CommitInput {
            message: "hi".to_string(),
            author: Some("alice@example.com".to_string()),
            code_sha: None,
            branch: None,
            prompts,
            mcp_servers: Vec::new(),
            model: None,
            no_memory: true,
        };
        let out = assemble_inputs(input, None, None, None, BTreeMap::new(), None);
        assert_eq!(out.author, "alice@example.com");
        assert_eq!(out.peer_uid, None);
    }

    // Branch inference: when CommitInput.branch is None and HEAD already
    // points at a branch, that branch wins. (Test-analyzer review on PR #52.)
    #[tokio::test]
    async fn execute_infers_branch_from_existing_head() {
        let dir = tempfile::tempdir().unwrap();
        let state = make_state(dir.path()).await;

        // Set HEAD -> refs/heads/feature-x manually (no branch ref yet —
        // first commit on this branch will land cleanly).
        state.refs.write_head_symbolic("feature-x").unwrap();

        let mut input = commit_input("on-feature");
        input.branch = None; // force inference from HEAD

        let out = execute(state.clone(), input, None).await.unwrap();
        assert_eq!(out.branch, "feature-x");
        // HEAD remains pointing at feature-x; branch ref now exists.
        let head = state.refs.read_head().unwrap();
        assert!(matches!(head, Some(HeadRef::Branch(b)) if b == "feature-x"));
        assert!(state.refs.read_branch("feature-x").unwrap().is_some());
    }

    // Branch inference: when CommitInput.branch is None and HEAD is
    // unset (fresh repo), the orchestrator defaults to "main".
    // (Test-analyzer review on PR #52.)
    #[tokio::test]
    async fn execute_defaults_to_main_when_no_branch_and_no_head() {
        let dir = tempfile::tempdir().unwrap();
        let state = make_state(dir.path()).await;
        assert!(state.refs.read_head().unwrap().is_none());

        let mut input = commit_input("fresh-repo");
        input.branch = None;

        let out = execute(state.clone(), input, None).await.unwrap();
        assert_eq!(out.branch, "main");
        // HEAD got published on this first commit (B7 fix path).
        let head = state.refs.read_head().unwrap();
        assert!(matches!(head, Some(HeadRef::Branch(b)) if b == "main"));
    }
}
