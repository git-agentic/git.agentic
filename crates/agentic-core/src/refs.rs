//! Branch refs and `HEAD`, written atomically.
//!
//! Layout on disk (under `<repo>/.agentic/`):
//!
//! ```text
//! HEAD                          # either `ref: refs/heads/<name>` or a raw hash
//! refs/
//!   heads/
//!     main                      # contains a 64-hex BLAKE3 commit hash + newline
//!     ab-test                   # ditto
//! ```
//!
//! Writes go through a tmp-file-plus-`rename(2)` so a crash mid-update never
//! leaves a torn ref. This matches Git's discipline and is the basis for
//! ADR-0002 Decision 3 step 5 ("update the branch ref").

use crate::hash::{Hash, ParseHashError};
use crate::{Error, Result};

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Repo-local ref manager. Owns the `.agentic/` directory and writes refs
/// under it.
#[derive(Debug, Clone)]
pub struct Refs {
    /// Root of the `.agentic/` directory (NOT the repo root).
    agentic_dir: PathBuf,
}

/// What `HEAD` currently points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadRef {
    /// Symbolic ref: `ref: refs/heads/<name>`.
    Branch(String),
    /// Detached HEAD: a direct commit hash.
    Detached(Hash),
}

impl Refs {
    /// Create a refs manager rooted at `<repo>/.agentic/`. Creates the
    /// directory if absent.
    pub fn open(agentic_dir: impl Into<PathBuf>) -> Result<Self> {
        let agentic_dir = agentic_dir.into();
        fs::create_dir_all(agentic_dir.join("refs").join("heads"))?;
        Ok(Self { agentic_dir })
    }

    /// Path to the `.agentic/` directory this `Refs` is rooted at.
    pub fn agentic_dir(&self) -> &std::path::Path {
        &self.agentic_dir
    }

    fn head_path(&self) -> PathBuf {
        self.agentic_dir.join("HEAD")
    }

    fn branch_path(&self, name: &str) -> PathBuf {
        self.agentic_dir.join("refs").join("heads").join(name)
    }

    /// Write `HEAD` as a symbolic ref pointing at `refs/heads/<branch>`.
    pub fn write_head_symbolic(&self, branch: &str) -> Result<()> {
        let contents = format!("ref: refs/heads/{branch}\n");
        write_atomic(&self.head_path(), contents.as_bytes())
    }

    /// Read `HEAD`, returning either the branch name it points at or a
    /// detached commit hash. Returns `None` if `HEAD` is missing.
    pub fn read_head(&self) -> Result<Option<HeadRef>> {
        let path = self.head_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)?;
        let trimmed = raw.trim();
        if let Some(rest) = trimmed.strip_prefix("ref: refs/heads/") {
            Ok(Some(HeadRef::Branch(rest.to_string())))
        } else {
            let hash: Hash = trimmed
                .parse()
                .map_err(|e: ParseHashError| Error::Other(anyhow::anyhow!(e)))?;
            Ok(Some(HeadRef::Detached(hash)))
        }
    }

    /// Write a branch ref atomically.
    pub fn write_branch(&self, branch: &str, hash: &Hash) -> Result<()> {
        let path = self.branch_path(branch);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = format!("{}\n", hash.to_hex());
        write_atomic(&path, contents.as_bytes())
    }

    /// Read a branch ref. Returns `None` if the branch does not exist.
    pub fn read_branch(&self, branch: &str) -> Result<Option<Hash>> {
        let path = self.branch_path(branch);
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)?;
        let trimmed = raw.trim();
        let hash: Hash = trimmed
            .parse()
            .map_err(|e: ParseHashError| Error::Other(anyhow::anyhow!(e)))?;
        Ok(Some(hash))
    }

    /// List all branch names under `refs/heads/`, recursively. Nested
    /// names like `feature/foo` are reported with `/` separators
    /// regardless of platform, so the result round-trips through
    /// `read_branch` / `write_branch`. Returns names in arbitrary order;
    /// callers that need determinism should sort. Used by the startup
    /// ref-reconciler in
    /// `agenticd::lifecycle::reconcile_refs_on_startup` to find branch
    /// refs whose tip hash needs to be verified against the object store.
    ///
    /// The recursive form is required because `write_branch` calls
    /// `create_dir_all(parent)` and writes nested branches to nested
    /// directories. A flat listing would silently skip them. (Spotted by
    /// Copilot review on PR #50.)
    pub fn list_branches(&self) -> Result<Vec<String>> {
        let root = self.agentic_dir.join("refs").join("heads");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut branches = Vec::new();
        collect_branch_files(&root, &root, &mut branches)?;
        Ok(branches)
    }

    /// Resolve a ref name. Accepts `"HEAD"`, a branch name, or a raw hex hash.
    pub fn resolve(&self, name: &str) -> Result<Option<Hash>> {
        if name == "HEAD" {
            return match self.read_head()? {
                None => Ok(None),
                Some(HeadRef::Detached(h)) => Ok(Some(h)),
                Some(HeadRef::Branch(b)) => self.read_branch(&b),
            };
        }
        if name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit()) {
            let h: Hash = name
                .parse()
                .map_err(|e: ParseHashError| Error::Other(anyhow::anyhow!(e)))?;
            return Ok(Some(h));
        }
        self.read_branch(name)
    }

    /// Take a consistent-read snapshot of HEAD and every branch ref. The
    /// returned [`RefsSnapshot`] resolves names from its frozen map
    /// rather than re-reading the filesystem — so repeated `resolve`
    /// calls against the same snapshot can never disagree about which
    /// commit a branch points at, even if a concurrent writer advances
    /// the branch ref between calls.
    ///
    /// Atomicity caveat: `snapshot()` itself reads files sequentially.
    /// Callers must hold a serialising lock (in agenticd, that's
    /// `DaemonState.commit_lock` — every commit acquires it before
    /// writing refs, so taking the snapshot under the same lock is
    /// sufficient) to guarantee the snapshot's contents reflect a
    /// single point in time. Without external serialisation a concurrent
    /// commit can still interleave between two `read_branch` calls
    /// inside `snapshot()`; the snapshot then describes one ref's
    /// pre-commit state and another's post-commit state. The lock is
    /// the discipline that closes that window.
    pub fn snapshot(&self) -> Result<RefsSnapshot> {
        let head = self.read_head()?;
        let mut branches = BTreeMap::new();
        for name in self.list_branches()? {
            if let Some(hash) = self.read_branch(&name)? {
                branches.insert(name, hash);
            }
        }
        Ok(RefsSnapshot { head, branches })
    }
}

/// Frozen view of HEAD + every branch ref as of [`Refs::snapshot`].
///
/// Use when two or more ref resolutions must agree on a single point
/// in time (e.g. `agentic diff from..to` — without the snapshot, a
/// commit landing between the two `resolve` calls would produce a
/// diff that mixes one branch's pre-commit state with another's
/// post-commit state).
#[derive(Debug, Clone)]
pub struct RefsSnapshot {
    head: Option<HeadRef>,
    branches: BTreeMap<String, Hash>,
}

impl RefsSnapshot {
    /// Resolve a ref name from the frozen view. Accepts `"HEAD"`, a
    /// branch name, or a raw 64-hex hash. Mirrors [`Refs::resolve`]'s
    /// semantics — but without any filesystem read.
    pub fn resolve(&self, name: &str) -> Result<Option<Hash>> {
        if name == "HEAD" {
            return Ok(match &self.head {
                None => None,
                Some(HeadRef::Detached(h)) => Some(*h),
                Some(HeadRef::Branch(b)) => self.branches.get(b).copied(),
            });
        }
        if name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit()) {
            let h: Hash = name
                .parse()
                .map_err(|e: ParseHashError| Error::Other(anyhow::anyhow!(e)))?;
            return Ok(Some(h));
        }
        Ok(self.branches.get(name).copied())
    }

    /// What `HEAD` pointed at when the snapshot was taken.
    pub fn head(&self) -> Option<&HeadRef> {
        self.head.as_ref()
    }

    /// All branch refs at snapshot time, keyed by branch name.
    pub fn branches(&self) -> &BTreeMap<String, Hash> {
        &self.branches
    }
}

/// Walk `dir` (recursively) collecting branch names relative to `root`.
/// Skips `.tmp` files left by `write_atomic` mid-write. Path separators
/// in the returned names are always `/`, regardless of platform.
fn collect_branch_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect_branch_files(root, &entry.path(), out)?;
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.ends_with(".tmp") {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|e| Error::Other(anyhow::anyhow!(e)))?
            .to_string_lossy()
            .replace('\\', "/");
        out.push(rel);
    }
    Ok(())
}

/// Atomic write via tmp-file plus `rename(2)`. The rename is atomic on POSIX
/// filesystems; readers see either the old contents or the new, never torn.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_roundtrip_symbolic() {
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::open(dir.path()).unwrap();
        assert!(refs.read_head().unwrap().is_none());
        refs.write_head_symbolic("main").unwrap();
        assert_eq!(
            refs.read_head().unwrap(),
            Some(HeadRef::Branch("main".to_string()))
        );
    }

    #[test]
    fn branch_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::open(dir.path()).unwrap();
        let h = Hash::of(b"some-commit");
        assert!(refs.read_branch("main").unwrap().is_none());
        refs.write_branch("main", &h).unwrap();
        assert_eq!(refs.read_branch("main").unwrap(), Some(h));
    }

    #[test]
    fn list_branches_returns_existing_names() {
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::open(dir.path()).unwrap();
        assert!(refs.list_branches().unwrap().is_empty());
        refs.write_branch("main", &Hash::of(b"m")).unwrap();
        refs.write_branch("feature", &Hash::of(b"f")).unwrap();
        let mut branches = refs.list_branches().unwrap();
        branches.sort();
        assert_eq!(branches, vec!["feature".to_string(), "main".to_string()]);
    }

    // Regression guard: write_branch supports nested names like
    // `feature/foo` via create_dir_all(parent), so list_branches must
    // recurse and report them with `/` separators on all platforms.
    // (Copilot review on PR #50.)
    #[test]
    fn list_branches_recurses_into_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::open(dir.path()).unwrap();
        refs.write_branch("main", &Hash::of(b"m")).unwrap();
        refs.write_branch("feature/foo", &Hash::of(b"ff")).unwrap();
        refs.write_branch("feature/bar", &Hash::of(b"fb")).unwrap();
        refs.write_branch("releases/v1.0/rc", &Hash::of(b"rc"))
            .unwrap();

        let mut branches = refs.list_branches().unwrap();
        branches.sort();
        assert_eq!(
            branches,
            vec![
                "feature/bar".to_string(),
                "feature/foo".to_string(),
                "main".to_string(),
                "releases/v1.0/rc".to_string(),
            ]
        );

        // The listed names must round-trip through read_branch.
        assert_eq!(
            refs.read_branch("feature/foo").unwrap(),
            Some(Hash::of(b"ff"))
        );
        assert_eq!(
            refs.read_branch("releases/v1.0/rc").unwrap(),
            Some(Hash::of(b"rc"))
        );
    }

    #[test]
    fn resolve_head_chases_symbolic_ref() {
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::open(dir.path()).unwrap();
        let h = Hash::of(b"top");
        refs.write_head_symbolic("main").unwrap();
        refs.write_branch("main", &h).unwrap();
        assert_eq!(refs.resolve("HEAD").unwrap(), Some(h));
    }

    #[test]
    fn resolve_accepts_raw_hex() {
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::open(dir.path()).unwrap();
        let h = Hash::of(b"raw");
        assert_eq!(refs.resolve(&h.to_hex()).unwrap(), Some(h));
    }

    #[test]
    fn snapshot_captures_head_and_all_branches() {
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::open(dir.path()).unwrap();
        let main = Hash::of(b"main");
        let feature = Hash::of(b"feature");
        refs.write_branch("main", &main).unwrap();
        refs.write_branch("feature/x", &feature).unwrap();
        refs.write_head_symbolic("main").unwrap();

        let snap = refs.snapshot().unwrap();
        assert_eq!(snap.resolve("main").unwrap(), Some(main));
        assert_eq!(snap.resolve("feature/x").unwrap(), Some(feature));
        assert_eq!(
            snap.resolve("HEAD").unwrap(),
            Some(main),
            "HEAD must chase through the snapshot's frozen branch map"
        );
        assert_eq!(snap.resolve("nonexistent").unwrap(), None);
        // Branches accessor sees both refs.
        assert_eq!(snap.branches().len(), 2);
    }

    #[test]
    fn snapshot_resolve_uses_frozen_map_not_filesystem() {
        // The whole point of the snapshot: after we capture it, writes to
        // the underlying ref files must NOT be observable through the
        // snapshot. A concurrent commit landing between two resolves on
        // the same snapshot can't yield disagreeing results — that's
        // the diff-atomicity invariant #45 lands.
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::open(dir.path()).unwrap();
        let v1 = Hash::of(b"v1");
        let v2 = Hash::of(b"v2");
        refs.write_branch("main", &v1).unwrap();

        let snap = refs.snapshot().unwrap();
        // Simulate a concurrent commit advancing main to v2.
        refs.write_branch("main", &v2).unwrap();

        // The snapshot still sees v1 — frozen at construction time.
        assert_eq!(snap.resolve("main").unwrap(), Some(v1));
        // The live Refs sees v2.
        assert_eq!(refs.resolve("main").unwrap(), Some(v2));
    }

    #[test]
    fn snapshot_resolve_accepts_raw_hex_without_filesystem_read() {
        // A 64-hex name resolves to itself even if it's not a stored ref.
        // Mirrors Refs::resolve's behaviour so callers can pass commit
        // hashes directly to `agentic diff <hash> HEAD`.
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::open(dir.path()).unwrap();
        let snap = refs.snapshot().unwrap();
        let h = Hash::of(b"some-commit");
        assert_eq!(snap.resolve(&h.to_hex()).unwrap(), Some(h));
    }

    #[test]
    fn snapshot_handles_detached_head() {
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::open(dir.path()).unwrap();
        let h = Hash::of(b"detached");
        // Manually write a detached HEAD (no symbolic ref).
        std::fs::write(
            dir.path().join("HEAD"),
            format!("{}\n", h.to_hex()).as_bytes(),
        )
        .unwrap();

        let snap = refs.snapshot().unwrap();
        assert_eq!(snap.resolve("HEAD").unwrap(), Some(h));
    }
}
