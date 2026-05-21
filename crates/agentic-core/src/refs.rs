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

    /// List all branch names that have a ref file under `refs/heads/`.
    /// Returns them in arbitrary order; callers that need determinism
    /// should sort. Used by the startup ref-reconciler in
    /// `agenticd::lifecycle::reconcile_refs_on_startup` to find branch
    /// refs whose tip hash needs to be verified against the object store.
    pub fn list_branches(&self) -> Result<Vec<String>> {
        let dir = self.agentic_dir.join("refs").join("heads");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut branches = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            // `write_atomic` leaves `.tmp` files only mid-write; skip them
            // in case we race a concurrent writer.
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".tmp") {
                    continue;
                }
                branches.push(name.to_string());
            }
        }
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
}
