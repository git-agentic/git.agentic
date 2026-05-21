//! Two-phase commit staging — the load-bearing plumbing.
//!
//! Per `docs/adr/0002-substrate-and-supercommit.md` §"Decision 3", every
//! commit MUST traverse these steps in this exact order:
//!
//!   1. Stage all non-Git blobs (prompts/tools/model/intent/plan/transcript/
//!      evals/memory-segments) to the content-addressed object store.
//!      Collect their content hashes.
//!   2. Construct the in-memory `Commit` referencing those hashes.
//!   3. Write the `Commit` blob to the object store. Capture its hash.
//!   4. Push to Git with the Commit hash referenced from a Git note attached
//!      to `code_sha`, OR via a `refs/agentic/manifests/<commit-hash>` ref.
//!      **This Git push is the single commit point** for MVP-with-Git mode.
//!      In Chunk A (this week) the Git push is a no-op; the single commit
//!      point degrades to step 3.
//!   5. Update the local branch ref (`refs/heads/<branch>`) to point at the
//!      new Commit hash.
//!
//! Failure-recovery contract:
//!   - Steps 1–3 fail: orphan blobs only; GC reclaims them. No public state.
//!   - Step 4 fails:   retry idempotently. Same content hashes → no duplicates.
//!   - Step 5 fails after Step 4 succeeds: the manifest is durable; advancing
//!     the branch ref on retry recovers.
//!
//! **Do not reorder these steps.** See ADR-0002 Decision 3 and `CLAUDE.md`
//! §"What not to do".

use crate::hash::Hash;
use crate::object::{Blob, Commit, Object, ObjectKind, Tree, TypedRef};
use crate::refs::Refs;
use crate::store::ObjectStore;
use crate::{Error, Result};

use std::collections::BTreeMap;

/// Inputs to a commit, gathered before staging. The daemon assembles this
/// from the SDK/CLI request and then calls [`stage_and_commit`].
#[derive(Debug, Default, Clone)]
pub struct CommitInputs {
    pub author: String,
    pub message: String,
    pub parent: Option<Hash>,
    pub code_sha: Option<String>,

    /// Path → contents for prompt files. Becomes a Tree of Blobs.
    pub prompts: BTreeMap<String, Vec<u8>>,

    /// Tool manifests keyed by tool name. Becomes a Tree of Blobs.
    pub tools: BTreeMap<String, Vec<u8>>,

    /// Model version string (e.g. `"anthropic:claude-opus:2026-05-01"`).
    pub model: Option<String>,

    /// Memory snapshot manifest hash, populated by `agentic-memory` in
    /// Chunk B. None in Chunk A.
    pub memory_snapshot: Option<Hash>,

    pub schema_version: Option<String>,

    // ADR-0002 platform-API extensions. Each is a raw blob; the daemon
    // stages it and threads the resulting hash into the Commit.
    pub intent: Option<Vec<u8>>,
    pub plan: Option<Vec<u8>>,
    pub transcript: Option<Vec<u8>>,
    pub evals: Option<Vec<u8>>,
    pub cost_cents: u32,

    /// UID of the daemon's socket peer; propagated into `Commit::peer_uid`.
    /// `None` when the daemon is running under `--insecure-allow-any-uid`
    /// or when commits originate from non-socket paths (e.g. unit tests).
    pub peer_uid: Option<u32>,
}

/// Outputs of a successful commit.
#[derive(Debug, Clone)]
pub struct CommitOutputs {
    pub commit_hash: Hash,
    pub branch: String,
}

/// Run the 2PC staging in the mandatory ADR-0002 order. Pure orchestration —
/// no Postgres, no Git wire I/O yet (Chunk A is local-only). The Git push
/// extension point (step 4) is a TODO marker that lands in Chunk B/C.
///
/// Reads wall-clock time once via `chrono::Utc::now()` and threads it
/// into the Commit's timestamp. Callers that need a deterministic
/// timestamp (idempotent-retry recovery; tests) should use
/// [`stage_and_commit_with_now`] directly. Audit anchor §B4 / §A3.
pub fn stage_and_commit<S: ObjectStore + ?Sized>(
    store: &S,
    refs: &Refs,
    branch: &str,
    inputs: CommitInputs,
) -> Result<CommitOutputs> {
    stage_and_commit_with_now(store, refs, branch, inputs, chrono::Utc::now())
}

/// `stage_and_commit` with the wall-clock injection point exposed. The
/// `now` argument becomes the Commit's `timestamp`. With the same
/// `(inputs, now)`, this function produces the same `commit_hash` on
/// every call — that's what makes ADR-0002 D3 step-4 retry idempotent
/// (same content → same hash → no duplicate Commit blob), and what
/// makes a determinism test possible (audit anchor §B4 / §A3).
pub fn stage_and_commit_with_now<S: ObjectStore + ?Sized>(
    store: &S,
    refs: &Refs,
    branch: &str,
    inputs: CommitInputs,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<CommitOutputs> {
    // -- Step 1: stage all non-Git blobs ---------------------------------
    let prompts_hash = stage_blob_tree(store, &inputs.prompts)?;
    let tools_hash = stage_blob_tree(store, &inputs.tools)?;
    let model_hash = stage_optional_blob(store, inputs.model.as_deref().map(str::as_bytes))?;
    let intent_hash = stage_optional_blob(store, inputs.intent.as_deref())?;
    let plan_hash = stage_optional_blob(store, inputs.plan.as_deref())?;
    let transcript_hash = stage_optional_blob(store, inputs.transcript.as_deref())?;
    let evals_hash = stage_optional_blob(store, inputs.evals.as_deref())?;

    // -- Step 2: construct the in-memory Commit --------------------------
    let commit = Commit {
        parent: inputs.parent,
        author: inputs.author,
        timestamp: now,
        message: inputs.message,
        code_sha: inputs.code_sha,
        prompts: prompts_hash,
        tools: tools_hash,
        model: model_hash,
        memory_snapshot: inputs.memory_snapshot,
        schema_version: inputs.schema_version,
        intent: intent_hash,
        plan: plan_hash,
        transcript: transcript_hash,
        evals: evals_hash,
        cost_cents: inputs.cost_cents,
        signatures: Vec::new(),
        peer_uid: inputs.peer_uid,
    };

    // -- Step 3: write the Commit blob to the object store ---------------
    let commit_hash = store.put(&Object::Commit(Box::new(commit)))?;

    // -- Step 4: Git push as the single commit point ---------------------
    // TODO(chunk-c): push `commit_hash` as a Git note on `code_sha` or as
    // `refs/agentic/manifests/<commit-hash>`. For Chunk A the single commit
    // point degrades to Step 3 above, since we do not yet touch a Git remote.

    // -- Step 5: update the branch ref -----------------------------------
    refs.write_branch(branch, &commit_hash)?;

    Ok(CommitOutputs {
        commit_hash,
        branch: branch.to_string(),
    })
}

/// Stage a `BTreeMap<String, Vec<u8>>` as a Tree of Blob refs. Returns
/// `None` for an empty map (no tree object written, no hash) so empty
/// dimensions don't pollute the object graph.
fn stage_blob_tree<S: ObjectStore + ?Sized>(
    store: &S,
    entries: &BTreeMap<String, Vec<u8>>,
) -> Result<Option<Hash>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut tree = Tree::new();
    for (name, bytes) in entries {
        let blob_hash = store.put(&Object::Blob(Blob::new(bytes.clone())))?;
        tree.insert(
            name.clone(),
            TypedRef {
                kind: ObjectKind::Blob,
                hash: blob_hash,
            },
        );
    }
    let tree_hash = store.put(&Object::Tree(tree))?;
    Ok(Some(tree_hash))
}

/// Stage an optional opaque payload as a Blob. Returns `None` if input is `None`.
fn stage_optional_blob<S: ObjectStore + ?Sized>(
    store: &S,
    payload: Option<&[u8]>,
) -> Result<Option<Hash>> {
    match payload {
        None => Ok(None),
        Some(bytes) => {
            let h = store.put(&Object::Blob(Blob::new(bytes.to_vec())))?;
            Ok(Some(h))
        }
    }
}

/// Walk parent pointers from `start` and yield up to `limit` commits.
pub fn walk_log<S: ObjectStore + ?Sized>(
    store: &S,
    start: Hash,
    limit: usize,
) -> Result<Vec<(Hash, Commit)>> {
    let mut out = Vec::new();
    let mut cursor = Some(start);
    while let Some(h) = cursor {
        if out.len() >= limit {
            break;
        }
        let obj = store.get(&h)?;
        let commit = match obj {
            Object::Commit(c) => *c,
            other => {
                return Err(Error::KindMismatch {
                    expected: ObjectKind::Commit,
                    actual: other.kind(),
                });
            }
        };
        cursor = commit.parent;
        out.push((h, commit));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FsObjectStore;

    fn fresh_inputs(message: &str) -> CommitInputs {
        let mut prompts = BTreeMap::new();
        prompts.insert("system.txt".to_string(), b"you are helpful".to_vec());
        CommitInputs {
            author: "test".to_string(),
            message: message.to_string(),
            parent: None,
            code_sha: Some("0000000000000000000000000000000000000000".to_string()),
            prompts,
            tools: BTreeMap::new(),
            model: Some("anthropic:claude-opus:2026-05-01".to_string()),
            memory_snapshot: None,
            schema_version: None,
            intent: None,
            plan: None,
            transcript: None,
            evals: None,
            cost_cents: 0,
            peer_uid: None,
        }
    }

    #[test]
    fn commit_writes_blobs_then_tree_then_commit() {
        let dir = tempfile::tempdir().unwrap();
        let agentic = dir.path().join(".agentic");
        let store = FsObjectStore::open(agentic.join("objects")).unwrap();
        let refs = Refs::open(&agentic).unwrap();
        refs.write_head_symbolic("main").unwrap();

        let out = stage_and_commit(&store, &refs, "main", fresh_inputs("init")).unwrap();

        assert_eq!(refs.read_branch("main").unwrap(), Some(out.commit_hash));

        let obj = store.get(&out.commit_hash).unwrap();
        let commit = match obj {
            Object::Commit(c) => *c,
            _ => panic!("expected Commit"),
        };
        let prompts_hash = commit.prompts.expect("prompts tree");
        let prompts_obj = store.get(&prompts_hash).unwrap();
        let tree = match prompts_obj {
            Object::Tree(t) => t,
            _ => panic!("expected Tree"),
        };
        assert!(tree.entries.contains_key("system.txt"));
    }

    #[test]
    fn second_commit_links_parent_and_walks() {
        let dir = tempfile::tempdir().unwrap();
        let agentic = dir.path().join(".agentic");
        let store = FsObjectStore::open(agentic.join("objects")).unwrap();
        let refs = Refs::open(&agentic).unwrap();
        refs.write_head_symbolic("main").unwrap();

        let first = stage_and_commit(&store, &refs, "main", fresh_inputs("one")).unwrap();

        let mut second_in = fresh_inputs("two");
        second_in.parent = Some(first.commit_hash);
        second_in
            .prompts
            .insert("system.txt".into(), b"changed".to_vec());
        let second = stage_and_commit(&store, &refs, "main", second_in).unwrap();

        assert_ne!(first.commit_hash, second.commit_hash);

        let log = walk_log(&store, second.commit_hash, 10).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].0, second.commit_hash);
        assert_eq!(log[1].0, first.commit_hash);
        assert_eq!(log[0].1.parent, Some(first.commit_hash));
    }

    #[test]
    fn idempotent_blob_tree() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let a = stage_blob_tree(
            &store,
            &BTreeMap::from([("p.txt".to_string(), b"hi".to_vec())]),
        )
        .unwrap()
        .unwrap();
        let b = stage_blob_tree(
            &store,
            &BTreeMap::from([("p.txt".to_string(), b"hi".to_vec())]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(a, b, "identical inputs produce identical tree hash");
    }

    // AC for issue #38 / audit §A3 / §B4: same CommitInputs + same now
    // → same commit_hash. Without `stage_and_commit_with_now` the
    // public `stage_and_commit` reads `chrono::Utc::now()` internally,
    // so two back-to-back calls with the same logical inputs produce
    // different timestamps and therefore different commit blobs —
    // breaking the retry-idempotency claim in the module docstring.
    #[test]
    fn stage_and_commit_with_now_is_deterministic() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let store_a = FsObjectStore::open(dir_a.path().join("objects")).unwrap();
        let store_b = FsObjectStore::open(dir_b.path().join("objects")).unwrap();
        let refs_a = Refs::open(dir_a.path()).unwrap();
        let refs_b = Refs::open(dir_b.path()).unwrap();

        let fixed_now = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let inputs = || CommitInputs {
            author: "tester".to_string(),
            message: "deterministic".to_string(),
            parent: None,
            code_sha: Some("deadbeef".to_string()),
            prompts: BTreeMap::from([("system.md".to_string(), b"hello".to_vec())]),
            tools: BTreeMap::new(),
            model: Some("anthropic:claude-opus:2026-05-01".to_string()),
            memory_snapshot: None,
            schema_version: None,
            intent: None,
            plan: None,
            transcript: None,
            evals: None,
            cost_cents: 0,
            peer_uid: None,
        };

        let a = stage_and_commit_with_now(&store_a, &refs_a, "main", inputs(), fixed_now).unwrap();
        let b = stage_and_commit_with_now(&store_b, &refs_b, "main", inputs(), fixed_now).unwrap();

        assert_eq!(
            a.commit_hash, b.commit_hash,
            "same inputs + same now must produce same commit hash"
        );
    }

    // Companion: same inputs but DIFFERENT `now` produce different
    // commit hashes — the timestamp is part of the Commit blob's
    // content, by design (commit history needs wall-clock attribution).
    #[test]
    fn stage_and_commit_with_now_differs_when_timestamp_differs() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let store_a = FsObjectStore::open(dir_a.path().join("objects")).unwrap();
        let store_b = FsObjectStore::open(dir_b.path().join("objects")).unwrap();
        let refs_a = Refs::open(dir_a.path()).unwrap();
        let refs_b = Refs::open(dir_b.path()).unwrap();

        let inputs = || CommitInputs {
            author: "tester".to_string(),
            message: "deterministic".to_string(),
            parent: None,
            code_sha: Some("deadbeef".to_string()),
            prompts: BTreeMap::new(),
            tools: BTreeMap::new(),
            model: None,
            memory_snapshot: None,
            schema_version: None,
            intent: None,
            plan: None,
            transcript: None,
            evals: None,
            cost_cents: 0,
            peer_uid: None,
        };

        let t1 = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        let t2 = chrono::DateTime::<chrono::Utc>::from_timestamp(1_700_000_001, 0).unwrap();
        let a = stage_and_commit_with_now(&store_a, &refs_a, "main", inputs(), t1).unwrap();
        let b = stage_and_commit_with_now(&store_b, &refs_b, "main", inputs(), t2).unwrap();

        assert_ne!(a.commit_hash, b.commit_hash);
    }
}
