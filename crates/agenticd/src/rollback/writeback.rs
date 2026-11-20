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
use std::sync::atomic::{AtomicU64, Ordering};

use agentic_core::{Commit, Hash, ObjectKind};
use anyhow::{anyhow, Context};

use super::loaders::{load_blob, load_tree};
use crate::server::DaemonState;

/// Check, without touching the filesystem, that [`restore_prompts`] would
/// accept this tree: every entry name is valid and neither `<repo>/prompts`
/// nor any existing directory above a target is a symlink. Rollback runs it
/// before migrations or memory restore so a bad tree can't tear a rollback.
pub(super) fn preflight_prompts(
    state: &DaemonState,
    repo: &Path,
    prompts_hash: &Hash,
) -> anyhow::Result<()> {
    let dir = repo.join("prompts");
    reject_symlinked_dirs(repo, &dir)?;
    let tmp_dir = ensure_tmp_dir(state.refs.agentic_dir())?;
    reject_other_device(&tmp_dir, &dir)?;
    let tree = load_tree(state, prompts_hash)?;
    let names: Vec<&String> = tree.entries.keys().collect();
    reject_file_dir_conflicts(&names)?;
    for name in names {
        validate_tree_entry_name(name)?;
        if let Some(parent) = dir.join(name).parent() {
            reject_symlinked_dirs(repo, parent)?;
            reject_other_device(&tmp_dir, parent)?;
        }
    }
    Ok(())
}

/// Reject a tree where one entry is both a file and a directory of another
/// (`a` and `a/b`): the write would fail halfway, after the database changed.
/// Names are compared as path components, lowercased, so `a//b` and a
/// case-folding filesystem cannot hide the conflict.
fn reject_file_dir_conflicts(names: &[&String]) -> anyhow::Result<()> {
    let key = |n: &str| -> Vec<String> {
        Path::new(n)
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
            .collect()
    };
    let keys: Vec<Vec<String>> = names.iter().map(|n| key(n)).collect();
    for (i, file) in keys.iter().enumerate() {
        if let Some(j) = keys
            .iter()
            .position(|dir| dir.len() > file.len() && dir.starts_with(file))
        {
            return Err(anyhow!(
                "prompt tree entry {:?} is both a file and the directory of {:?}",
                names[i],
                names[j]
            ));
        }
    }
    Ok(())
}

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
///
/// Ancestor directories get the same treatment: `<repo>/prompts` and every
/// existing directory between it and a write target must not be a symlink
/// ([`reject_symlinked_dirs`]), checked before and after `create_dir_all`.
/// This covers links that exist before the call. A concurrent swap during
/// the call is a residual race for a worker that can write the repo. It is
/// accepted because the deployment must not let the worker write the
/// daemon's repo or state directory (docs/integration/executor-sidecar.md);
/// a full fix needs an openat-style walk.
pub(super) fn restore_prompts(
    state: &DaemonState,
    repo: &Path,
    prompts_hash: &Hash,
) -> anyhow::Result<()> {
    let dir = repo.join("prompts");
    reject_symlinked_dirs(repo, &dir)?;
    std::fs::create_dir_all(&dir)?;
    reject_symlinked_dirs(repo, &dir)?;
    let tmp_dir = ensure_tmp_dir(state.refs.agentic_dir())?;

    let tree = load_tree(state, prompts_hash)?;
    let mut wanted: BTreeSet<PathBuf> = BTreeSet::new();
    for (name, r) in &tree.entries {
        validate_tree_entry_name(name)?;
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            reject_symlinked_dirs(repo, parent)?;
            std::fs::create_dir_all(parent)?;
            reject_symlinked_dirs(repo, parent)?;
        }
        let blob = load_blob(state, &r.hash)?;
        write_blob_safely(&tmp_dir, &path, &blob.bytes)?;
        wanted.insert(path);
    }

    sweep_orphans(&dir, &dir, &wanted)?;
    Ok(())
}

/// Fail if any existing component of `target` below `base` is a symlink.
/// `base` itself (and anything above it) is trusted: the repo root may
/// legitimately live under a symlinked path. Components that don't exist
/// yet are fine; any other `symlink_metadata` error propagates.
fn reject_symlinked_dirs(base: &Path, target: &Path) -> anyhow::Result<()> {
    let mut cur = base.to_path_buf();
    for component in target.strip_prefix(base)?.components() {
        cur.push(component);
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(anyhow!(
                    "refusing prompt write-back: {} is a symlink",
                    cur.display()
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e).with_context(|| format!("inspecting {}", cur.display()));
            }
        }
    }
    Ok(())
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const TMP_ATTEMPTS: usize = 16;

/// `<agentic_dir>/tmp`, created (mode 0700 on unix) if missing. Temp files
/// live here, outside the prompts tree, so no prompt entry can occupy a
/// temp name.
fn ensure_tmp_dir(agentic_dir: &Path) -> anyhow::Result<PathBuf> {
    let dir = agentic_dir.join("tmp");
    match std::fs::symlink_metadata(&dir) {
        // An existing entry must be a real directory (never a symlink), and is
        // tightened to 0700 in case an older run or another tool loosened it.
        Ok(meta) if meta.file_type().is_dir() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("restricting temp directory {}", dir.display()))?;
            }
        }
        Ok(_) => {
            return Err(anyhow!(
                "temp directory {} exists but is not a directory (symlink or file); refusing to use it",
                dir.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
                .create(&dir)
                .with_context(|| format!("creating temp directory {}", dir.display()))?;
        }
        Err(e) => {
            return Err(e).with_context(|| format!("inspecting temp directory {}", dir.display()));
        }
    }
    Ok(dir)
}

/// Refuse a target whose nearest existing ancestor is on a different
/// filesystem than the temp directory: the final `rename` would fail with
/// EXDEV after the database changed.
#[cfg(unix)]
fn reject_other_device(tmp_dir: &Path, target: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let tmp_dev = std::fs::metadata(tmp_dir)
        .with_context(|| format!("inspecting {}", tmp_dir.display()))?
        .dev();
    let Some(existing) = target.ancestors().find(|p| p.exists()) else {
        return Ok(());
    };
    let dev = std::fs::metadata(existing)
        .with_context(|| format!("inspecting {}", existing.display()))?
        .dev();
    if dev != tmp_dev {
        return Err(anyhow!(
            "{} is on a different filesystem than {}; .agentic and prompts must share one filesystem",
            existing.display(),
            tmp_dir.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_other_device(_tmp_dir: &Path, _target: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Exclusively create a temp file in `tmp_dir` (`.agentic-tmp-<pid>-<n>`,
/// `n` from `next`). `create_new` (`O_EXCL`) fails on anything already
/// there, including a planted symlink, and never follows one; on
/// `AlreadyExists` we try the next `n`, up to [`TMP_ATTEMPTS`] times. The
/// returned path is ours alone, so callers may remove it on failure.
fn create_temp(
    tmp_dir: &Path,
    mut next: impl FnMut() -> u64,
) -> anyhow::Result<(PathBuf, std::fs::File)> {
    for _ in 0..TMP_ATTEMPTS {
        let tmp = tmp_dir.join(format!(".agentic-tmp-{}-{}", std::process::id(), next()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(e).with_context(|| format!("creating temp file {}", tmp.display()));
            }
        }
    }
    Err(anyhow!(
        "no free temp file name in {} after {TMP_ATTEMPTS} attempts",
        tmp_dir.display()
    ))
}

/// Write `bytes` to `path` safely:
///
/// 1. If `path` already exists as a symlink, unlink it. Any other
///    `symlink_metadata` error (besides `NotFound`) propagates — we
///    must not silently proceed if e.g. permissions deny inspection
///    of the destination, since the attack we're defending against
///    is a redirected write. Only `NotFound` is benign (the file
///    doesn't exist yet, which is the common case).
/// 2. Write the new bytes to a uniquely named temp file in `tmp_dir`
///    (created exclusively, so a pre-planted symlink is never followed;
///    `tmp_dir` must be on the same filesystem as `path`), then atomically
///    `rename(2)` it over `path`. This closes the TOCTOU window
///    between the symlink unlink and the regular file write: even if
///    an attacker re-plants a symlink at `path` between steps, the
///    rename replaces it as a regular file (it doesn't follow links).
///
/// Mirrors `agentic-core::store::FsObjectStore::write_at`'s
/// temp-then-rename pattern. (Copilot review on PR #51, second pass.)
fn write_blob_safely(tmp_dir: &Path, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    write_blob_with(tmp_dir, path, bytes, || {
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    })
}

fn write_blob_with(
    tmp_dir: &Path,
    path: &Path,
    bytes: &[u8],
    next: impl FnMut() -> u64,
) -> anyhow::Result<()> {
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
    let (tmp, mut file) = create_temp(tmp_dir, next)?;
    // The temp is ours from here on, so it is safe to remove on failure.
    let result = std::io::Write::write_all(&mut file, bytes)
        .with_context(|| format!("writing temp file {}", tmp.display()))
        .and_then(|()| {
            std::fs::rename(&tmp, path).map_err(|e| {
                if e.kind() == std::io::ErrorKind::CrossesDevices {
                    anyhow!(
                        "cannot rename {} into {}: `.agentic` and `prompts` must be on \
                         the same filesystem",
                        tmp.display(),
                        path.display()
                    )
                } else {
                    anyhow::Error::new(e).context(format!(
                        "renaming {} → {}",
                        tmp.display(),
                        path.display()
                    ))
                }
            })
        });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
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
///
/// Refuses to run on a symlinked `here` and never recurses through one
/// (`DirEntry::file_type` doesn't follow links, so a symlinked directory
/// is unlinked as an orphan, not descended into).
fn sweep_orphans(root: &Path, here: &Path, wanted: &BTreeSet<PathBuf>) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(here)?.file_type().is_symlink() {
        return Err(anyhow!(
            "refusing prompt sweep: {} is a symlink",
            here.display()
        ));
    }
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
    fn file_dir_conflict_is_rejected() {
        let (a, ab, c) = ("a".to_string(), "a/b".to_string(), "ab".to_string());
        assert!(reject_file_dir_conflicts(&[&a, &ab]).is_err());
        assert!(reject_file_dir_conflicts(&[&a, &c]).is_ok());
        assert!(reject_file_dir_conflicts(&[&ab, &c]).is_ok());
        let (p, pab, upper) = ("p//a".to_string(), "p/a/b".to_string(), "P/A".to_string());
        assert!(reject_file_dir_conflicts(&[&p, &pab]).is_err());
        assert!(reject_file_dir_conflicts(&[&upper, &pab]).is_err());
    }

    // Test helper: write through a fresh temp dir outside `path`'s directory.
    fn wb(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
        let tmp = tempfile::tempdir().unwrap();
        write_blob_safely(tmp.path(), path, bytes)
    }

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
        wb(&dest, b"restored prompt content").unwrap();

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
        wb(&dest, b"hello").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello");
    }

    #[test]
    fn write_blob_safely_replaces_regular_file_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("existing.md");
        std::fs::write(&dest, b"old content").unwrap();
        wb(&dest, b"new content").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new content");
        // Regular file (not turned into anything weird).
        assert!(std::fs::symlink_metadata(&dest)
            .unwrap()
            .file_type()
            .is_file());
    }

    // Build a DaemonState over a fresh object store, commit `files` as a
    // prompts tree, and return (state, tree hash, repo dir guard).
    async fn state_with_prompts(files: &[(&str, &[u8])]) -> (DaemonState, Hash, tempfile::TempDir) {
        use agentic_core::{Blob, FsObjectStore, Object, ObjectStore, Tree, TypedRef};
        use std::sync::Arc;
        let repo = tempfile::tempdir().unwrap();
        let agentic_dir = repo.path().join(".agentic");
        std::fs::create_dir_all(agentic_dir.join("objects")).unwrap();
        let store: Arc<dyn ObjectStore + Send + Sync> =
            Arc::new(FsObjectStore::open(agentic_dir.join("objects")).unwrap());
        let mut tree = Tree::new();
        for (name, bytes) in files {
            let hash = store.put(&Object::Blob(Blob::new(bytes.to_vec()))).unwrap();
            tree.insert(
                *name,
                TypedRef {
                    kind: ObjectKind::Blob,
                    hash,
                },
            );
        }
        let tree_hash = store.put(&Object::Tree(tree)).unwrap();
        let state = DaemonState::open(
            repo.path().to_path_buf(),
            agentic_dir,
            store,
            None,
            Vec::new(),
            Vec::new(),
            Arc::new(crate::peer_auth::PeerAuthPolicy::InsecureAllowAny),
        )
        .await
        .unwrap();
        (state, tree_hash, repo)
    }

    // Outside dir pre-populated with a file the sweep would delete if it
    // followed a link, plus a name the tree would write.
    #[cfg(unix)]
    fn outside_dir() -> tempfile::TempDir {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep.txt"), b"outside").unwrap();
        outside
    }

    #[cfg(unix)]
    fn assert_outside_untouched(outside: &Path) {
        let mut names: Vec<_> = std::fs::read_dir(outside)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["keep.txt"], "outside dir must be untouched");
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_prompts_rejects_symlinked_subdirectory() {
        let (state, tree, repo) = state_with_prompts(&[("sub/x", b"payload")]).await;
        let outside = outside_dir();
        std::fs::create_dir_all(repo.path().join("prompts")).unwrap();
        std::os::unix::fs::symlink(outside.path(), repo.path().join("prompts/sub")).unwrap();

        let err = restore_prompts(&state, repo.path(), &tree).unwrap_err();
        assert!(err.to_string().contains("symlink"), "got {err}");
        assert_outside_untouched(outside.path());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_prompts_rejects_symlinked_prompts_root() {
        let (state, tree, repo) = state_with_prompts(&[("x", b"payload")]).await;
        let outside = outside_dir();
        std::os::unix::fs::symlink(outside.path(), repo.path().join("prompts")).unwrap();

        let err = restore_prompts(&state, repo.path(), &tree).unwrap_err();
        assert!(err.to_string().contains("symlink"), "got {err}");
        assert_outside_untouched(outside.path());
    }

    #[cfg(unix)]
    #[test]
    fn sweep_orphans_refuses_symlinked_root() {
        let outside = outside_dir();
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("prompts");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        assert!(sweep_orphans(&link, &link, &BTreeSet::new()).is_err());
        assert_outside_untouched(outside.path());
    }

    #[tokio::test]
    async fn restore_prompts_handles_tmp_and_md_siblings() {
        // `a.txt` sorts after `a.tmp`, so the old `with_extension("tmp")` temp
        // name for `a.txt` clobbered the already-restored `a.tmp`.
        let (state, tree, repo) = state_with_prompts(&[
            ("a.md", b"markdown"),
            ("a.tmp", b"tmpfile"),
            ("a.txt", b"text"),
        ])
        .await;
        restore_prompts(&state, repo.path(), &tree).unwrap();
        let dir = repo.path().join("prompts");
        assert_eq!(std::fs::read(dir.join("a.md")).unwrap(), b"markdown");
        assert_eq!(std::fs::read(dir.join("a.tmp")).unwrap(), b"tmpfile");
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"text");
        // Temp files live outside prompts/ and are renamed away.
        for d in [dir.clone(), repo.path().join(".agentic/tmp")] {
            let leftovers: Vec<_> = std::fs::read_dir(&d)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains("agentic-tmp"))
                .collect();
            assert!(
                leftovers.is_empty(),
                "temp files left in {d:?}: {leftovers:?}"
            );
        }
    }

    // A symlink planted at the old temp name (`a.tmp` for `a.md`) must not
    // be followed: the outside file stays intact and `a.md` is written.
    #[cfg(unix)]
    #[test]
    fn write_blob_safely_does_not_follow_symlink_at_legacy_temp_name() {
        let dir = tempfile::tempdir().unwrap();
        let outside = outside_dir();
        let victim = outside.path().join("keep.txt");
        std::os::unix::fs::symlink(&victim, dir.path().join("a.tmp")).unwrap();

        wb(&dir.path().join("a.md"), b"restored").unwrap();

        assert_eq!(std::fs::read(dir.path().join("a.md")).unwrap(), b"restored");
        assert_outside_untouched(outside.path());
    }

    // O_EXCL + retry: files or symlinks already sitting at the first
    // candidate temp names in the tmp dir are left untouched and the write
    // still succeeds on the next free name. A fixed counter makes the
    // candidates known.
    #[cfg(unix)]
    #[test]
    fn write_blob_safely_skips_pre_planted_temp_names() {
        let prompts = tempfile::tempdir().unwrap();
        let tmp_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir();
        let pid = std::process::id();
        let planted_file = tmp_dir.path().join(format!(".agentic-tmp-{pid}-0"));
        std::fs::write(&planted_file, b"planted").unwrap();
        let planted_link = tmp_dir.path().join(format!(".agentic-tmp-{pid}-1"));
        std::os::unix::fs::symlink(outside.path().join("keep.txt"), &planted_link).unwrap();

        let mut n = 0;
        write_blob_with(
            tmp_dir.path(),
            &prompts.path().join("a.md"),
            b"restored",
            || {
                n += 1;
                n - 1
            },
        )
        .unwrap();

        assert_eq!(
            std::fs::read(prompts.path().join("a.md")).unwrap(),
            b"restored"
        );
        assert_eq!(std::fs::read(&planted_file).unwrap(), b"planted");
        assert!(std::fs::symlink_metadata(&planted_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_outside_untouched(outside.path());
    }

    #[test]
    fn write_blob_safely_gives_up_after_bounded_attempts() {
        let prompts = tempfile::tempdir().unwrap();
        let tmp_dir = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        for i in 0..TMP_ATTEMPTS {
            std::fs::write(tmp_dir.path().join(format!(".agentic-tmp-{pid}-{i}")), b"x").unwrap();
        }
        let mut n = 0;
        let err = write_blob_with(tmp_dir.path(), &prompts.path().join("a.md"), b"y", || {
            n += 1;
            n - 1
        })
        .unwrap_err();
        assert!(err.to_string().contains("no free temp file name"), "{err}");
        assert!(!prompts.path().join("a.md").exists());
        // Planted files must not have been removed by the error path.
        for i in 0..TMP_ATTEMPTS {
            assert!(tmp_dir
                .path()
                .join(format!(".agentic-tmp-{pid}-{i}"))
                .exists());
        }
    }

    // A committed tree can name entries `.agentic-tmp-<pid>-<n>`; since temp
    // files live outside prompts/, restore still succeeds and keeps them.
    #[tokio::test]
    async fn restore_prompts_succeeds_when_tree_names_temp_candidates() {
        let pid = std::process::id();
        let names: Vec<String> = (0..300)
            .map(|i| format!(".agentic-tmp-{pid}-{i}"))
            .collect();
        let files: Vec<(&str, &[u8])> = names
            .iter()
            .map(|n| (n.as_str(), b"entry".as_slice()))
            .chain(std::iter::once(("a.md", b"markdown".as_slice())))
            .collect();
        let (state, tree, repo) = state_with_prompts(&files).await;
        // Twice: the second run finds the entries already on disk, which is
        // what used to exhaust the in-tree temp candidates.
        restore_prompts(&state, repo.path(), &tree).unwrap();
        restore_prompts(&state, repo.path(), &tree).unwrap();
        let dir = repo.path().join("prompts");
        for n in &names {
            assert_eq!(std::fs::read(dir.join(n)).unwrap(), b"entry");
        }
        assert_eq!(std::fs::read(dir.join("a.md")).unwrap(), b"markdown");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_tmp_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let agentic = tempfile::tempdir().unwrap();
        let dir = ensure_tmp_dir(agentic.path()).unwrap();
        assert_eq!(dir, agentic.path().join("tmp"));
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        ensure_tmp_dir(agentic.path()).unwrap(); // idempotent
    }

    #[cfg(unix)]
    #[test]
    fn ensure_tmp_dir_refuses_symlink_and_tightens_existing() {
        use std::os::unix::fs::PermissionsExt;
        let outside = tempfile::tempdir().unwrap();
        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), linked.path().join("tmp")).unwrap();
        assert!(ensure_tmp_dir(linked.path()).is_err());

        let loose = tempfile::tempdir().unwrap();
        let tmp = loose.path().join("tmp");
        std::fs::create_dir(&tmp).unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o777)).unwrap();
        ensure_tmp_dir(loose.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn same_device_target_passes_the_device_check() {
        let agentic = tempfile::tempdir().unwrap();
        let tmp = ensure_tmp_dir(agentic.path()).unwrap();
        reject_other_device(&tmp, &agentic.path().join("prompts/a/b")).unwrap();
    }
}
