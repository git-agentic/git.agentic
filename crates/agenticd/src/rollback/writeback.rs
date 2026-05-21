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
///
/// Path safety: each tree entry name is validated via
/// [`validate_tree_entry_name`] before being joined with the prompts directory,
/// rejecting absolute paths and `..` components. Content-addressing guarantees
/// the stored bytes match what was committed, but it does not prevent a
/// compromised commit-write path from storing adversarial names — so
/// validation is required unconditionally.
///
/// Symlinks and races are handled defensively by [`write_blob_safely`]:
/// if the destination already exists as a symlink we unlink it first
/// (so the write lands on a fresh regular file and can't be redirected
/// outside the prompts directory), and the actual write is done as
/// write-to-temp + atomic rename so a concurrent attacker can't plant a
/// symlink between the unlink and the write. (Copilot review on PR #51.)
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
        validate_tree_entry_name(name)?;
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let blob = load_blob(state, &r.hash)?;
        write_blob_safely(&path, &blob.bytes)?;
        wanted.insert(path);
    }

    sweep_orphans(&dir, &dir, &wanted)?;
    Ok(())
}

/// Write `bytes` to `path` safely:
///
/// 1. If `path` already exists as a symlink, unlink it. Any other
///    `symlink_metadata` error (besides `NotFound`) propagates — we
///    must not silently proceed if e.g. permissions deny inspection
///    of the destination, since the attack we're defending against
///    is a redirected write. Only `NotFound` is benign (the file
///    doesn't exist yet, which is the common case).
/// 2. Write the new bytes to a sibling `.tmp` file, then atomically
///    `rename(2)` it over `path`. This closes the TOCTOU window
///    between the symlink unlink and the regular file write: even if
///    an attacker re-plants a symlink at `path` between steps, the
///    rename replaces it as a regular file (it doesn't follow links).
///
/// Mirrors `agentic-core::store::FsObjectStore::write_at`'s
/// temp-then-rename pattern. (Copilot review on PR #51, second pass.)
fn write_blob_safely(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::remove_file(path)
                .with_context(|| format!("removing pre-existing symlink at {}", path.display()))?;
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "checking for symlink at {} before prompt write",
                    path.display()
                )
            });
        }
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing temp file {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Reject tree entry names that would escape the prompts directory or
/// otherwise resolve to anything except a relative path. Accepts only
/// names whose every `std::path::Component` is `Normal` — that excludes
/// absolute paths (`Component::RootDir`, `Component::Prefix`), parent
/// references (`Component::ParentDir`), and the current-dir component
/// (`Component::CurDir`). Empty names are also rejected.
fn validate_tree_entry_name(name: &str) -> anyhow::Result<()> {
    use std::path::Component;
    if name.is_empty() {
        return Err(anyhow!("tree entry has empty name"));
    }
    for component in Path::new(name).components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(anyhow!(
                    "tree entry name {:?} contains an unsafe path component; \
                     only relative names without '..' or absolute prefixes are allowed",
                    name
                ));
            }
        }
    }
    Ok(())
}

/// Remove every entry under `here` that isn't in the `wanted` set.
/// Directories are recursed into first and then removed if they end up
/// empty (except `root` itself). Files, symlinks, and any other
/// non-directory file types are removed via `remove_file` if absent
/// from `wanted` — symlinks must be removable orphans too (Copilot
/// review on PR #51: the prior `ft.is_file()` branch silently left
/// dangling symlinks behind even though the docs claimed everything
/// not in the target tree is removed).
fn sweep_orphans(root: &Path, here: &Path, wanted: &BTreeSet<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(here)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            sweep_orphans(root, &path, wanted)?;
            if path != root && std::fs::read_dir(&path)?.next().is_none() {
                // remove_dir can race a concurrent writer (or fail if the
                // directory got something new since the read_dir check);
                // log + continue rather than silently swallowing or
                // failing the whole rollback. The next rollback will try
                // again. (Copilot review on PR #51, second pass.)
                if let Err(e) = std::fs::remove_dir(&path) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "could not remove empty orphan directory; will retry on next rollback"
                    );
                }
            }
        } else if !wanted.contains(&path) {
            // Files, symlinks, FIFOs, sockets — anything that isn't a
            // directory and isn't in the target tree is an orphan.
            // `remove_file` operates on the link itself for symlinks,
            // not the target, which is what we want.
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
    // Reject invalid UTF-8 loudly rather than silently replacing the
    // offending bytes with U+FFFD — a non-UTF-8 model blob is a
    // corruption signal we want to surface to the operator, not
    // silently corrupt by lossy decoding into the forward-record
    // Commit. (Copilot review on PR #51, second pass.)
    String::from_utf8(blob.bytes)
        .with_context(|| format!("model blob at {hash} contains invalid UTF-8"))
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_normal_relative_names() {
        assert!(validate_tree_entry_name("system.md").is_ok());
        assert!(validate_tree_entry_name("subdir/system.md").is_ok());
        assert!(validate_tree_entry_name("a/b/c/deeply/nested.txt").is_ok());
    }

    #[test]
    fn validate_rejects_empty_name() {
        let err = validate_tree_entry_name("").unwrap_err();
        assert!(err.to_string().contains("empty name"));
    }

    #[test]
    fn validate_rejects_absolute_paths() {
        let err = validate_tree_entry_name("/etc/passwd").unwrap_err();
        assert!(err.to_string().contains("unsafe path component"));
    }

    #[test]
    fn validate_rejects_parent_dir_components() {
        for evil in &["../escape", "subdir/../../escape", "..", "./../sneaky"] {
            let err = validate_tree_entry_name(evil).unwrap_err();
            assert!(
                err.to_string().contains("unsafe path component"),
                "name {evil:?} should be rejected; got {err}"
            );
        }
    }

    #[test]
    fn validate_rejects_current_dir_component() {
        let err = validate_tree_entry_name("./local").unwrap_err();
        assert!(err.to_string().contains("unsafe path component"));
    }

    // Sweep should remove dangling symlinks under prompts/ that aren't
    // in the target tree. Without the fix the symlink would survive
    // (since DirEntry::file_type does not follow links and the prior
    // code only matched is_file()).
    #[cfg(unix)]
    #[test]
    fn sweep_orphans_removes_dangling_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("prompts");
        std::fs::create_dir_all(&dir).unwrap();
        let wanted_path = dir.join("system.md");
        std::fs::write(&wanted_path, b"hello").unwrap();
        // A symlink to something outside the dir — also not in `wanted`.
        let orphan_link = dir.join("orphan-link");
        symlink("/etc/hostname", &orphan_link).unwrap();

        let wanted: BTreeSet<PathBuf> = std::iter::once(wanted_path.clone()).collect();
        sweep_orphans(&dir, &dir, &wanted).unwrap();

        assert!(wanted_path.exists(), "wanted file should remain");
        assert!(
            !orphan_link.exists() && std::fs::symlink_metadata(&orphan_link).is_err(),
            "orphan symlink should have been removed"
        );
    }

    // The pre-write symlink guard: if `<repo>/prompts/system.md` is
    // already a symlink pointing outside the dir, `write_blob_safely`
    // must unlink it and write a regular file at the destination —
    // never follow the symlink. The outside target must remain
    // untouched. This is the writeback path's primary defence against
    // a pre-planted-symlink redirected-write attack. (Copilot review on
    // PR #51, second pass.)
    #[cfg(unix)]
    #[test]
    fn write_blob_safely_unlinks_pre_existing_symlink_and_writes_regular_file() {
        use std::os::unix::fs::symlink;
        let prompts_tmp = tempfile::tempdir().unwrap();
        let outside_tmp = tempfile::tempdir().unwrap();
        let outside_target = outside_tmp.path().join("secret.txt");
        std::fs::write(
            &outside_target,
            b"outside content - must not be overwritten",
        )
        .unwrap();

        // Pre-plant a malicious symlink at the destination.
        let dest = prompts_tmp.path().join("system.md");
        symlink(&outside_target, &dest).unwrap();
        assert!(
            std::fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink(),
            "pre-condition: dest must start as a symlink"
        );

        // Write the new blob.
        write_blob_safely(&dest, b"restored prompt content").unwrap();

        // The symlink is gone; dest is now a regular file with the new
        // bytes.
        let meta = std::fs::symlink_metadata(&dest).unwrap();
        assert!(meta.file_type().is_file(), "dest should be a regular file");
        assert!(
            !meta.file_type().is_symlink(),
            "dest must no longer be a symlink"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"restored prompt content",
            "dest should contain the new blob bytes"
        );

        // The original symlink target outside the prompts directory
        // must NOT have been overwritten.
        assert_eq!(
            std::fs::read(&outside_target).unwrap(),
            b"outside content - must not be overwritten",
            "outside target must be untouched"
        );

        // No leftover temp file in the prompts dir.
        let tmp_path = dest.with_extension("tmp");
        assert!(
            !tmp_path.exists(),
            "temp file should have been renamed away"
        );
    }

    #[test]
    fn write_blob_safely_creates_new_file_when_dest_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("new.md");
        assert!(!dest.exists());
        write_blob_safely(&dest, b"hello").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn write_blob_safely_replaces_regular_file_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("existing.md");
        std::fs::write(&dest, b"old content").unwrap();
        write_blob_safely(&dest, b"new content").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new content");
        // Regular file (not turned into anything weird).
        assert!(std::fs::symlink_metadata(&dest)
            .unwrap()
            .file_type()
            .is_file());
    }
}
