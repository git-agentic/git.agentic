//! Filesystem write-back + Commit-tree readers used by rollback's
//! forward-record step.
//!
//! Two related concerns live together here:
//!
//!   * `restore_prompts` + `sweep_orphans` write a target Commit's
//!     prompts tree back to `<repo>/prompts/` and delete any files on
//!     disk that aren't in the target. Synchronous `std::fs` I/O —
//!     callers should release the memory `Mutex` before invoking.
//!   * `read_text_blobs` + `read_model_text` read the in-memory bytes
//!     of a Commit's tree-typed dimensions so the forward-record step
//!     can re-stage them. The collapsed `read_text_blobs(state, Option<Hash>)`
//!     replaces the previous near-identical `read_prompts_for_commit` /
//!     `read_tools_for_commit` pair (audit §S7).
//!
//! Audit §S3 / §A4: this module owns the FS-write-back half of the
//! previous `rollback.rs` split.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use agentic_core::{Commit, Hash, ObjectKind};
use anyhow::{anyhow, Context};

use super::loaders::{load_blob, load_tree};
use crate::server::DaemonState;

/// Write target prompts back to `<repo>/prompts/`. Files present on disk
/// but not in the target tree are removed so the working set matches.
pub(super) fn restore_prompts(
    state: &DaemonState,
    repo: &Path,
    prompts_hash: &Hash,
) -> anyhow::Result<()> {
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

/// Materialise a tree-typed Commit dimension (prompts, tools) into a
/// `name -> bytes` map. Returns an empty map if the dimension is `None`.
///
/// Collapses the previous `read_prompts_for_commit` / `read_tools_for_commit`
/// pair: they differed only by which `Option<Hash>` field they read.
/// Audit §S7.
pub(super) fn read_text_blobs(
    state: &DaemonState,
    hash: Option<Hash>,
) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let Some(hash) = hash else {
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

pub(super) fn read_model_text(
    state: &DaemonState,
    target: &Commit,
) -> anyhow::Result<Option<String>> {
    let Some(hash) = target.model else {
        return Ok(None);
    };
    let blob = load_blob(state, &hash)?;
    Ok(Some(String::from_utf8_lossy(&blob.bytes).into_owned()))
}
