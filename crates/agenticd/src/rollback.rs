//! Rollback orchestrator.
//!
//! Implements `agentic rollback <ref>` end-to-end:
//!
//!   1. Resolve target ref → target Commit object.
//!   2. If the target's `schema_version` differs from the live database's,
//!      run reverse SQL migrations via `crate::migrate` to align the schema.
//!   3. Restore memory: load the target's SegmentManifest, hand it to
//!      `MemoryAdapter::restore` which TRUNCATEs each tracked table and
//!      re-INSERTs from the captured segments inside one transaction.
//!   4. Write prompt blobs back to disk under `<repo>/prompts/`. Any
//!      prompt file on disk that isn't in the target tree is removed.
//!   5. Tools / model: noted in the plan but not pushed to disk in MVP.
//!      Pinning into `.agentic/config.toml` lands when the operator-
//!      facing pin store does.
//!   6. Forward-record: create a new Commit C' whose dimensions match
//!      target A but whose parent is the current branch tip C. Update
//!      the branch ref to C'. History preserves the rollback action.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use agentic_core::commit::{stage_and_commit, CommitInputs};
use agentic_core::refs::HeadRef;
use agentic_core::{Blob, Commit, Hash, Object, ObjectKind, Tree};
use agentic_memory::adapter::SnapshotHandle;
use agentic_memory::segment::SegmentManifest;
use agentic_memory::MemoryAdapter;
use agentic_proto::RollbackOutput;
use anyhow::{anyhow, Context};

use crate::migrate;
use crate::server::DaemonState;

pub struct RollbackArgs {
    pub target: String,
    pub dry_run: bool,
    pub accept_data_loss: bool,
    /// Filesystem root of the repo (where `prompts/` lives).
    pub repo: PathBuf,
}

pub async fn execute(
    state: std::sync::Arc<DaemonState>,
    args: RollbackArgs,
) -> anyhow::Result<RollbackOutput> {
    let target_hash = state
        .refs
        .resolve(&args.target)?
        .ok_or_else(|| anyhow!("ref not found: {}", args.target))?;
    let target = load_commit(&state, &target_hash)?;
    validate_target_shape(&target, &target_hash)?;

    let mut plan: Vec<String> = Vec::new();

    let branch = match state.refs.read_head()? {
        Some(HeadRef::Branch(b)) => b,
        Some(HeadRef::Detached(_)) => {
            return Err(anyhow!(
                "rollback from a detached HEAD is not supported (Chunk C+1)"
            ));
        }
        None => {
            return Err(anyhow!(
                "repo has no HEAD; commit something before rolling back"
            ))
        }
    };
    let current_tip: Option<Hash> = state.refs.read_branch(&branch)?;

    plan.push(format!(
        "target commit {} ({})",
        target_hash.short(),
        target.message
    ));
    if let Some(c) = current_tip {
        plan.push(format!(
            "current branch tip {} → forward-record over it",
            c.short()
        ));
    }

    // -- Schema migrations ---------------------------------------------------
    // The MutexGuard is released before any filesystem I/O so the daemon
    // stays responsive and the future remains Send (no &adapter across awaits
    // in separate async fns).
    if let Some(ref target_schema) = target.schema_version {
        let memory: std::sync::Arc<tokio::sync::Mutex<agentic_memory::postgres::PostgresAdapter>> =
            std::sync::Arc::clone(state.memory.as_ref().ok_or_else(|| {
                anyhow!("target commit has a schema_version but no memory backend is attached")
            })?);

        // Phase 1: query DB for live schema version and pending migration names.
        // Guard is dropped at the end of this block.
        let (live_schema, migration_names) = {
            let adapter = std::sync::Arc::clone(&memory).lock_owned().await;
            let live = adapter
                .current_schema_version()
                .await
                .context("reading live schema version")?;
            let names = if live != *target_schema {
                adapter
                    .migrations_after(target_schema)
                    .await
                    .context("querying pending reverse migrations")?
            } else {
                Vec::new()
            };
            (live, names)
        };

        if live_schema != *target_schema {
            plan.push(format!(
                "reverse schema migrations: {live_schema} → {target_schema}"
            ));
            // Phase 2: synchronous filesystem I/O — no lock held.
            // `accept_data_loss` is forwarded here so `check_irreversible` can
            // honor the operator's opt-in for IRREVERSIBLE-marked migrations.
            let steps = migrate::load_steps(
                state.refs.agentic_dir(),
                &migration_names,
                args.accept_data_loss,
            )
            .context("loading reverse migration files")?;

            // Phase 3: execute migrations — re-acquire lock.
            if !args.dry_run {
                let adapter = std::sync::Arc::clone(&memory).lock_owned().await;
                migrate::run_reverse(&adapter, steps)
                    .await
                    .context("running reverse migrations")?;
            }
        } else {
            plan.push(format!(
                "schema already at {target_schema} — no migrations needed"
            ));
        }

        // -- Memory ----------------------------------------------------------
        if let Some(manifest_hash) = target.memory_snapshot {
            plan.push(format!(
                "restore memory from manifest {}",
                manifest_hash.short()
            ));
            if !args.dry_run {
                let manifest = load_manifest(&state, &manifest_hash)?;
                let handle = SnapshotHandle {
                    manifest,
                    schema_version: target_schema.clone(),
                };
                let adapter = std::sync::Arc::clone(&memory).lock_owned().await;
                adapter
                    .restore(&handle)
                    .await
                    .context("restoring memory snapshot")?;
            }
        } else {
            plan.push("no memory snapshot in target — skipping memory data restore".into());
        }
    } else {
        plan.push(
            "no schema_version in target — skipping schema migration and memory restore".into(),
        );
    }

    // -- Prompts -------------------------------------------------------------
    if let Some(prompts_hash) = target.prompts {
        plan.push(format!(
            "rewrite {} from target prompts tree {}",
            args.repo.join("prompts").display(),
            prompts_hash.short()
        ));
        if !args.dry_run {
            restore_prompts(&state, &args.repo, &prompts_hash)?;
        }
    } else {
        plan.push("no prompts in target — skipping prompts rewrite".into());
    }

    // -- Tools / model -------------------------------------------------------
    if target.tools.is_some() {
        plan.push(
            "tools manifest present in target; pinning into .agentic/config.toml is a follow-up"
                .into(),
        );
    }
    if target.model.is_some() {
        plan.push("model version captured in target — operator must redeploy if it changed".into());
    }

    if args.dry_run {
        return Ok(RollbackOutput {
            planned_steps: plan,
            executed: false,
            new_head_hash: None,
        });
    }

    // -- Forward-record ------------------------------------------------------
    // Build a fresh Commit whose dimensions match the target but whose
    // parent is the current branch tip. History shows C → C' even though
    // C' contains A's state.
    let prompts_payload = read_prompts_for_commit(&state, &target)?;
    let tools_payload = read_tools_for_commit(&state, &target)?;
    let model_text = read_model_text(&state, &target)?;

    let inputs = CommitInputs {
        author: "agentic-rollback".to_string(),
        message: format!("Rollback to {}", target_hash.short()),
        parent: current_tip,
        code_sha: target.code_sha.clone(),
        prompts: prompts_payload,
        tools: tools_payload,
        model: model_text,
        memory_snapshot: target.memory_snapshot,
        schema_version: target.schema_version.clone(),
        intent: None,
        plan: None,
        transcript: None,
        evals: None,
        cost_cents: 0,
    };
    let out = stage_and_commit(state.store.as_ref(), &state.refs, &branch, inputs)
        .context("forward-recording rollback commit")?;
    plan.push(format!(
        "forward-recorded rollback as {} on {}",
        out.commit_hash.short(),
        out.branch
    ));

    Ok(RollbackOutput {
        planned_steps: plan,
        executed: true,
        new_head_hash: Some(out.commit_hash.to_hex()),
    })
}

/// Reject target Commits whose `(memory_snapshot, schema_version)` pair is
/// in a shape the daemon should not have produced.
///
/// Audit B9 found that rollback silently skipped the memory restore branch
/// when `target.memory_snapshot.is_some()` and `target.schema_version.is_none()`
/// because the outer `if let Some(ref target_schema)` gated both the schema
/// migration and the memory-restore branches together. The commit-write code
/// path (`crates/agenticd/src/server.rs`) only produces commits with both
/// fields `Some` together or both `None` together — the mixed state is
/// unreachable through v1.0's normal write paths. Rather than silently
/// skipping (the bug) or pretending to restore with a fabricated
/// `schema_version` (option ii in source.md §Q2), we reject the state loudly
/// and direct the operator to file a v1.1 issue if they reached it through a
/// custom SDK or a legacy commit.
///
/// See `docs/plans/a8-reverse-migration/artifacts/implementation-decision-log.md#d-1`
/// for the full rationale and rejected alternatives.
fn validate_target_shape(target: &Commit, target_hash: &Hash) -> anyhow::Result<()> {
    if target.memory_snapshot.is_some() && target.schema_version.is_none() {
        return Err(anyhow!(
            "target commit {} has memory_snapshot but no schema_version; \
             this state should not be reachable through normal commit-write paths \
             (see ADR-0002 and docs/plans/a8-reverse-migration/source.md §Q2). \
             Refusing rollback. If you reached this through a custom SDK or a \
             legacy commit, please file a v1.1 issue.",
            target_hash.short()
        ));
    }
    Ok(())
}

fn load_commit(state: &DaemonState, hash: &Hash) -> anyhow::Result<Commit> {
    match state.store.get(hash)? {
        Object::Commit(c) => Ok(*c),
        other => Err(anyhow!(
            "expected commit at {}, got {:?}",
            hash,
            other.kind()
        )),
    }
}

fn load_tree(state: &DaemonState, hash: &Hash) -> anyhow::Result<Tree> {
    match state.store.get(hash)? {
        Object::Tree(t) => Ok(t),
        other => Err(anyhow!("expected tree at {}, got {:?}", hash, other.kind())),
    }
}

fn load_blob(state: &DaemonState, hash: &Hash) -> anyhow::Result<Blob> {
    match state.store.get(hash)? {
        Object::Blob(b) => Ok(b),
        other => Err(anyhow!("expected blob at {}, got {:?}", hash, other.kind())),
    }
}

fn load_manifest(state: &DaemonState, hash: &Hash) -> anyhow::Result<SegmentManifest> {
    let bytes = state.store.get_raw(hash)?;
    let manifest: SegmentManifest =
        serde_json::from_slice(&bytes).with_context(|| format!("decoding manifest {hash}"))?;
    Ok(manifest)
}

/// Write target prompts back to `<repo>/prompts/`. Files present on disk
/// but not in the target tree are removed so the working set matches.
fn restore_prompts(state: &DaemonState, repo: &Path, prompts_hash: &Hash) -> anyhow::Result<()> {
    let dir = repo.join("prompts");
    std::fs::create_dir_all(&dir)?;

    let tree = load_tree(state, prompts_hash)?;
    let mut wanted: BTreeSet<PathBuf> = BTreeSet::new();
    for (name, r) in &tree.entries {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let blob = load_blob(state, &r.hash)?;
        std::fs::write(&path, &blob.bytes)
            .with_context(|| format!("writing {}", path.display()))?;
        wanted.insert(path);
    }

    sweep_orphans(&dir, &dir, &wanted)?;
    Ok(())
}

fn sweep_orphans(root: &Path, here: &Path, wanted: &BTreeSet<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(here)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            sweep_orphans(root, &path, wanted)?;
            if path != root && std::fs::read_dir(&path)?.next().is_none() {
                let _ = std::fs::remove_dir(&path);
            }
        } else if ft.is_file() && !wanted.contains(&path) {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing orphan prompt {}", path.display()))?;
        }
    }
    Ok(())
}

fn read_prompts_for_commit(
    state: &DaemonState,
    target: &Commit,
) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let Some(hash) = target.prompts else {
        return Ok(BTreeMap::new());
    };
    tree_to_map(state, &hash)
}

fn read_tools_for_commit(
    state: &DaemonState,
    target: &Commit,
) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let Some(hash) = target.tools else {
        return Ok(BTreeMap::new());
    };
    tree_to_map(state, &hash)
}

fn tree_to_map(state: &DaemonState, hash: &Hash) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let tree = load_tree(state, hash)?;
    let mut out = BTreeMap::new();
    for (name, r) in tree.entries {
        if r.kind != ObjectKind::Blob {
            return Err(anyhow!("non-blob entry {name} in tree {hash}"));
        }
        let blob = load_blob(state, &r.hash)?;
        out.insert(name, blob.bytes);
    }
    Ok(out)
}

fn read_model_text(state: &DaemonState, target: &Commit) -> anyhow::Result<Option<String>> {
    let Some(hash) = target.model else {
        return Ok(None);
    };
    let blob = load_blob(state, &hash)?;
    Ok(Some(String::from_utf8_lossy(&blob.bytes).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_hash() -> Hash {
        // Any concrete Hash works for shape validation — the validator
        // doesn't read the object store.
        Hash::of(b"a8-test-fixture")
    }

    fn commit_with(memory_snapshot: Option<Hash>, schema_version: Option<String>) -> Commit {
        Commit {
            parent: None,
            author: "test".into(),
            timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
            message: "test".into(),
            code_sha: None,
            prompts: None,
            tools: None,
            model: None,
            memory_snapshot,
            schema_version,
            intent: None,
            plan: None,
            transcript: None,
            evals: None,
            cost_cents: 0,
            signatures: Vec::new(),
        }
    }

    // AC2 — the malformed (memory_snapshot=Some, schema_version=None) shape
    // is rejected loudly. See docs/plans/a8-reverse-migration/source.md §Q2.
    #[test]
    fn validate_rejects_memory_without_schema() {
        let target = commit_with(Some(empty_hash()), None);
        let err = validate_target_shape(&target, &empty_hash()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("memory_snapshot but no schema_version"),
            "error message should name the contradiction; got: {msg}"
        );
        assert!(
            msg.contains("Q2") || msg.contains("v1.1"),
            "error message should point at the planning rationale / v1.1 work item; got: {msg}"
        );
    }

    // Sanity / regression: every other shape passes the validation.
    #[test]
    fn validate_accepts_both_some() {
        let target = commit_with(Some(empty_hash()), Some("003_add_embeddings".into()));
        assert!(validate_target_shape(&target, &empty_hash()).is_ok());
    }

    #[test]
    fn validate_accepts_both_none() {
        let target = commit_with(None, None);
        assert!(validate_target_shape(&target, &empty_hash()).is_ok());
    }

    #[test]
    fn validate_accepts_schema_without_memory() {
        // This shape is what a code-only commit with a schema baseline looks
        // like; rollback to it is well-defined (no memory restore needed).
        let target = commit_with(None, Some("002_baseline".into()));
        assert!(validate_target_shape(&target, &empty_hash()).is_ok());
    }
}
