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

pub struct RollbackArgs<'a> {
    pub target: &'a str,
    pub dry_run: bool,
    pub accept_data_loss: bool,
    /// Filesystem root of the repo (where `prompts/` lives).
    pub repo: &'a Path,
}

pub async fn execute(
    state: &DaemonState,
    args: RollbackArgs<'_>,
) -> anyhow::Result<RollbackOutput> {
    let target_hash = state
        .refs
        .resolve(args.target)?
        .ok_or_else(|| anyhow!("ref not found: {}", args.target))?;
    let target = load_commit(state, &target_hash)?;

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
    if let Some(ref target_schema) = target.schema_version {
        let memory = state.memory.as_ref().ok_or_else(|| {
            anyhow!("target commit has a schema_version but no memory backend is attached")
        })?;
        let adapter = memory.lock().await;
        let live_schema = adapter
            .current_schema_version()
            .await
            .context("reading live schema version")?;

        if live_schema != *target_schema {
            plan.push(format!(
                "reverse schema migrations: {live_schema} → {target_schema}"
            ));
            if !args.dry_run {
                let steps =
                    migrate::plan_reverse(&adapter, state.refs.agentic_dir(), target_schema)
                        .await
                        .context("planning reverse migrations")?;
                migrate::run_reverse(&adapter, &steps)
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
                let manifest = load_manifest(state, &manifest_hash)?;
                let handle = SnapshotHandle {
                    manifest,
                    schema_version: target_schema.clone(),
                };
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
            restore_prompts(state, args.repo, &prompts_hash)?;
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
    let prompts_payload = read_prompts_for_commit(state, &target)?;
    let tools_payload = read_tools_for_commit(state, &target)?;
    let model_text = read_model_text(state, &target)?;

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
    let _ = args.accept_data_loss; // reserved for the migration runner

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
