//! Per-dimension diff between two commits.
//!
//! `diff(store, a, b)` resolves each commit's six ADR-0001 dimensions and
//! reports what changed. Output is structured so the CLI can render it
//! human-first and the SDK can consume it as JSON.
//!
//! Dimension-specific algorithms:
//!
//! | Dimension        | Comparator                                                            |
//! |------------------|-----------------------------------------------------------------------|
//! | `code_sha`       | string equality; reports `(old, new)`                                 |
//! | `prompts`        | walk both prompt Trees; added/modified/removed paths                  |
//! | `tools`          | walk both tools Trees; added/modified/removed server names            |
//! | `model`          | string equality on the blob bytes (UTF-8)                             |
//! | `memory_snapshot`| hash equality; structural diff lives in `agentic-memory::diff` (later)|
//! | `schema_version` | string equality                                                       |
//!
//! The diff is deterministic — re-running against the same two commits
//! produces byte-identical output.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::hash::Hash;
use crate::object::{Blob, Commit, Object, ObjectKind, Tree};
use crate::store::ObjectStore;
use crate::{Error, Result};

/// Top-level diff between two commits. Each field reports `None` when
/// both sides agree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitDiff {
    pub from: Hash,
    pub to: Hash,
    pub code_sha: Option<StringChange>,
    pub prompts: Option<TreeDiff>,
    pub tools: Option<TreeDiff>,
    pub model: Option<StringChange>,
    pub memory_snapshot: Option<HashChange>,
    pub schema_version: Option<StringChange>,
}

impl CommitDiff {
    /// True when every dimension is unchanged.
    pub fn is_empty(&self) -> bool {
        self.code_sha.is_none()
            && self.prompts.is_none()
            && self.tools.is_none()
            && self.model.is_none()
            && self.memory_snapshot.is_none()
            && self.schema_version.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StringChange {
    pub from: Option<String>,
    pub to: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashChange {
    pub from: Option<Hash>,
    pub to: Option<Hash>,
}

/// Per-entry diff of two name-keyed Trees. `modified` carries both hashes
/// so callers can fetch the underlying blobs and render line-diffs if they
/// want — that's the CLI's job, not ours.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeDiff {
    pub added: Vec<TreeEntry>,
    pub removed: Vec<TreeEntry>,
    pub modified: Vec<TreeModification>,
}

impl TreeDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub name: String,
    pub hash: Hash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeModification {
    pub name: String,
    pub from: Hash,
    pub to: Hash,
}

/// Diff two commits by walking each dimension. Caller passes commit
/// *hashes*; we resolve the rest.
pub fn diff<S: ObjectStore + ?Sized>(store: &S, from: Hash, to: Hash) -> Result<CommitDiff> {
    let a = load_commit(store, &from)?;
    let b = load_commit(store, &to)?;

    Ok(CommitDiff {
        from,
        to,
        code_sha: string_change(a.code_sha.clone(), b.code_sha.clone()),
        prompts: diff_tree(store, a.prompts, b.prompts)?,
        tools: diff_tree(store, a.tools, b.tools)?,
        model: diff_model_blob(store, a.model, b.model)?,
        memory_snapshot: hash_change(a.memory_snapshot, b.memory_snapshot),
        schema_version: string_change(a.schema_version.clone(), b.schema_version.clone()),
    })
}

fn load_commit<S: ObjectStore + ?Sized>(store: &S, hash: &Hash) -> Result<Commit> {
    match store.get(hash)? {
        Object::Commit(c) => Ok(*c),
        other => Err(Error::KindMismatch {
            expected: ObjectKind::Commit,
            actual: other.kind(),
        }),
    }
}

fn load_tree<S: ObjectStore + ?Sized>(store: &S, hash: &Hash) -> Result<Tree> {
    match store.get(hash)? {
        Object::Tree(t) => Ok(t),
        other => Err(Error::KindMismatch {
            expected: ObjectKind::Tree,
            actual: other.kind(),
        }),
    }
}

fn load_blob<S: ObjectStore + ?Sized>(store: &S, hash: &Hash) -> Result<Blob> {
    match store.get(hash)? {
        Object::Blob(b) => Ok(b),
        other => Err(Error::KindMismatch {
            expected: ObjectKind::Blob,
            actual: other.kind(),
        }),
    }
}

fn string_change(a: Option<String>, b: Option<String>) -> Option<StringChange> {
    if a == b {
        None
    } else {
        Some(StringChange { from: a, to: b })
    }
}

fn hash_change(a: Option<Hash>, b: Option<Hash>) -> Option<HashChange> {
    if a == b {
        None
    } else {
        Some(HashChange { from: a, to: b })
    }
}

fn diff_tree<S: ObjectStore + ?Sized>(
    store: &S,
    a: Option<Hash>,
    b: Option<Hash>,
) -> Result<Option<TreeDiff>> {
    if a == b {
        return Ok(None);
    }
    let a_entries = match a {
        Some(h) => load_tree(store, &h)?.entries,
        None => BTreeMap::new(),
    };
    let b_entries = match b {
        Some(h) => load_tree(store, &h)?.entries,
        None => BTreeMap::new(),
    };

    let mut diff = TreeDiff::default();
    let all_names: BTreeSet<&String> = a_entries.keys().chain(b_entries.keys()).collect();
    for name in all_names {
        match (a_entries.get(name), b_entries.get(name)) {
            (None, Some(b_ref)) => diff.added.push(TreeEntry {
                name: name.clone(),
                hash: b_ref.hash,
            }),
            (Some(a_ref), None) => diff.removed.push(TreeEntry {
                name: name.clone(),
                hash: a_ref.hash,
            }),
            (Some(a_ref), Some(b_ref)) if a_ref.hash != b_ref.hash => {
                diff.modified.push(TreeModification {
                    name: name.clone(),
                    from: a_ref.hash,
                    to: b_ref.hash,
                });
            }
            _ => {}
        }
    }
    if diff.is_empty() {
        Ok(None)
    } else {
        Ok(Some(diff))
    }
}

/// Model is a Blob containing the version string. We render the diff as
/// a `StringChange` so the CLI can present it the same way as
/// `schema_version`.
fn diff_model_blob<S: ObjectStore + ?Sized>(
    store: &S,
    a: Option<Hash>,
    b: Option<Hash>,
) -> Result<Option<StringChange>> {
    if a == b {
        return Ok(None);
    }
    let to_str = |h: Option<Hash>| -> Result<Option<String>> {
        match h {
            None => Ok(None),
            Some(h) => {
                let blob = load_blob(store, &h)?;
                Ok(Some(String::from_utf8_lossy(&blob.bytes).into_owned()))
            }
        }
    };
    let a_str = to_str(a)?;
    let b_str = to_str(b)?;
    if a_str == b_str {
        Ok(None)
    } else {
        Ok(Some(StringChange {
            from: a_str,
            to: b_str,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commit::{stage_and_commit, CommitInputs};
    use crate::refs::Refs;
    use crate::store::FsObjectStore;

    fn inputs(message: &str, prompt: &str, model: Option<&str>) -> CommitInputs {
        let mut prompts = BTreeMap::new();
        prompts.insert("system.txt".to_string(), prompt.as_bytes().to_vec());
        CommitInputs {
            author: "test".into(),
            message: message.into(),
            parent: None,
            code_sha: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            prompts,
            tools: BTreeMap::new(),
            model: model.map(|s| s.into()),
            memory_snapshot: None,
            schema_version: None,
            intent: None,
            plan: None,
            transcript: None,
            evals: None,
            cost_cents: 0,
        }
    }

    fn fresh() -> (tempfile::TempDir, FsObjectStore, Refs) {
        let dir = tempfile::tempdir().unwrap();
        let agentic = dir.path().join(".agentic");
        let store = FsObjectStore::open(agentic.join("objects")).unwrap();
        let refs = Refs::open(&agentic).unwrap();
        refs.write_head_symbolic("main").unwrap();
        (dir, store, refs)
    }

    #[test]
    fn identical_commits_have_no_diff() {
        let (_d, store, refs) = fresh();
        let a = stage_and_commit(&store, &refs, "main", inputs("a", "hi", Some("m1"))).unwrap();
        let b = stage_and_commit(&store, &refs, "main", inputs("b", "hi", Some("m1"))).unwrap();
        let d = diff(&store, a.commit_hash, b.commit_hash).unwrap();
        assert!(
            d.is_empty(),
            "no dimension changes between identical inputs"
        );
    }

    #[test]
    fn prompt_change_shows_modified_entry() {
        let (_d, store, refs) = fresh();
        let a = stage_and_commit(&store, &refs, "main", inputs("a", "hi", Some("m1"))).unwrap();
        let b = stage_and_commit(&store, &refs, "main", inputs("b", "hello", Some("m1"))).unwrap();
        let d = diff(&store, a.commit_hash, b.commit_hash).unwrap();
        let pd = d.prompts.expect("prompts changed");
        assert_eq!(pd.added.len(), 0);
        assert_eq!(pd.removed.len(), 0);
        assert_eq!(pd.modified.len(), 1);
        assert_eq!(pd.modified[0].name, "system.txt");
    }

    #[test]
    fn prompt_added_and_removed_are_detected() {
        let (_d, store, refs) = fresh();
        let mut in_a = inputs("a", "hi", Some("m1"));
        in_a.prompts
            .insert("rules.txt".into(), b"be brief".to_vec());
        let a = stage_and_commit(&store, &refs, "main", in_a).unwrap();

        let mut in_b = inputs("b", "hi", Some("m1"));
        in_b.prompts.insert("style.txt".into(), b"be warm".to_vec());
        let b = stage_and_commit(&store, &refs, "main", in_b).unwrap();

        let d = diff(&store, a.commit_hash, b.commit_hash).unwrap();
        let pd = d.prompts.expect("prompts changed");
        let added_names: Vec<_> = pd.added.iter().map(|e| e.name.as_str()).collect();
        let removed_names: Vec<_> = pd.removed.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(added_names, vec!["style.txt"]);
        assert_eq!(removed_names, vec!["rules.txt"]);
    }

    #[test]
    fn model_change_renders_as_string_change() {
        let (_d, store, refs) = fresh();
        let a = stage_and_commit(&store, &refs, "main", inputs("a", "hi", Some("m1"))).unwrap();
        let b = stage_and_commit(&store, &refs, "main", inputs("b", "hi", Some("m2"))).unwrap();
        let d = diff(&store, a.commit_hash, b.commit_hash).unwrap();
        let sc = d.model.expect("model changed");
        assert_eq!(sc.from.as_deref(), Some("m1"));
        assert_eq!(sc.to.as_deref(), Some("m2"));
    }
}
