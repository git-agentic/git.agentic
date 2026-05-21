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
pub async fn execute(state: Arc<DaemonState>, input: CommitInput) -> anyhow::Result<CommitOutput> {
    execute_with_now(state, input, chrono::Utc::now()).await
}

/// `execute` with the wall-clock injection point exposed for the
/// determinism test in this module. Threads `now` through to
/// `stage_and_commit_with_now` so identical `(input, now)` yields the
/// same `commit_hash`. Audit §B4.
pub async fn execute_with_now(
    state: Arc<DaemonState>,
    input: CommitInput,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<CommitOutput> {
    let head = state.refs.read_head()?;
    let branch = input.branch.clone().unwrap_or_else(|| match &head {
        Some(HeadRef::Branch(b)) => b.clone(),
        _ => "main".to_string(),
    });
    // Audit §B7 (A2 fix carried over): on a first-ever commit defer
    // the HEAD write until after `stage_and_commit_with_now` returns
    // Ok. Tracks whether we need to write HEAD on the success path.
    let needs_head_write = head.is_none();

    let parent = state.refs.read_branch(&branch)?;

    // -- Phase 1: snapshot memory ---------------------------------------
    let (memory_snapshot, schema_version) = snapshot_memory(&state, input.no_memory).await?;

    // -- Phase 2: fingerprint MCP tools ---------------------------------
    let tools = fingerprint_tools(&state).await?;

    // -- Phase 3: assemble core CommitInputs ----------------------------
    let inputs = assemble_inputs(input, parent, memory_snapshot, schema_version, tools);

    // -- Phase 4: stage + commit + ref update (agentic-core) ------------
    let out = stage_and_commit_with_now(state.store.as_ref(), &state.refs, &branch, inputs, now)?;

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
    let manifest_hash = state
        .store
        .put_raw(ObjectKind::Tree, &manifest_bytes)
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
        let out_a = execute_with_now(state_a, commit_input("deterministic"), fixed_now)
            .await
            .unwrap();
        let out_b = execute_with_now(state_b, commit_input("deterministic"), fixed_now)
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

        let out = execute(state.clone(), commit_input("first")).await.unwrap();

        let head = state.refs.read_head().unwrap();
        assert!(
            matches!(head, Some(HeadRef::Branch(ref b)) if b == "main"),
            "HEAD should chase refs/heads/main after first commit; got {head:?}"
        );
        assert_eq!(out.branch, "main");
    }

    // Two distinct commits on the same branch: parent linkage works
    // and HEAD is only written on the first one (no error on second).
    #[tokio::test]
    async fn execute_chains_commits_on_same_branch() {
        let dir = tempfile::tempdir().unwrap();
        let state = make_state(dir.path()).await;

        let first = execute(state.clone(), commit_input("first")).await.unwrap();
        let second = execute(state.clone(), commit_input("second"))
            .await
            .unwrap();

        assert_ne!(first.commit_hash, second.commit_hash);
        let tip = state.refs.read_branch("main").unwrap().unwrap();
        assert_eq!(tip.to_hex(), second.commit_hash);
    }
}
