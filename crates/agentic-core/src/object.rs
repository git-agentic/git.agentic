//! The four object kinds: Blob, Tree, Segment (placeholder), Commit.
//!
//! See `docs/architecture/snapshot-model.md` for semantics. This module
//! covers serialization, hashing, and validation. Segments live in their
//! own module (`agentic-memory`) because they own backend-specific logic;
//! a placeholder type is included here so commits can refer to them.

use crate::hash::Hash;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The four top-level object kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectKind {
    Blob,
    Tree,
    Segment,
    Commit,
}

/// A typed reference to another object: kind + content address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedRef {
    pub kind: ObjectKind,
    pub hash: Hash,
}

/// Opaque byte content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blob {
    pub bytes: Vec<u8>,
}

impl Blob {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    pub fn hash(&self) -> Hash {
        // Blobs hash their raw bytes directly — no envelope — so blob
        // hashes are identical to a plain BLAKE3 of the content.
        Hash::of(&self.bytes)
    }
}

/// A sorted map from name → typed reference.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tree {
    pub entries: BTreeMap<String, TypedRef>,
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, r: TypedRef) {
        self.entries.insert(name.into(), r);
    }

    pub fn hash(&self) -> Hash {
        // Canonicalize via JSON of the sorted BTreeMap. Stable across platforms.
        let bytes = serde_json::to_vec(self).expect("Tree serialization cannot fail");
        Hash::of(&bytes)
    }
}

/// A platform attestation over a commit. ADR-0002 Decision 2 — signatures
/// chain authorship between the originating agent platform and any human
/// reviewers. Opaque payload bytes plus an issuer string keep us neutral on
/// signing schemes for MVP (the verifier ships in v1.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    /// Free-form issuer identifier, e.g. `"cursor.com/v1"` or `"reviewer:toni"`.
    pub issuer: String,
    /// Opaque signature bytes. Scheme is documented per-issuer.
    pub signature: Vec<u8>,
}

/// The top-level commit object. Holds the agent state tuple plus the
/// ADR-0002 platform-API fields.
///
/// Per `docs/adr/0002-substrate-and-supercommit.md` §"Decision 2", the
/// Commit object IS the platform API contract. Extending it requires a
/// new ADR. The tuple dimensions stay `Option` until the corresponding
/// subsystem lands (memory in week 5, schema in week 6, intent/plan/etc.
/// progressively as the platform-PR surface is wired).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub parent: Option<Hash>,
    pub author: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub message: String,

    // --- ADR-0001 tuple dimensions ---
    /// SHA of the Git commit that this snapshot's code corresponds to.
    pub code_sha: Option<String>,
    /// Hash of a Tree of prompt blobs.
    pub prompts: Option<Hash>,
    /// Hash of a Tree of tool manifests (per-tool blobs).
    pub tools: Option<Hash>,
    /// Hash of a Blob containing the model version string.
    pub model: Option<Hash>,
    /// Hash of the memory snapshot manifest.
    pub memory_snapshot: Option<Hash>,
    /// Semver of the memory schema at commit time.
    pub schema_version: Option<String>,

    // --- ADR-0002 platform-API extensions ---
    /// Blob: what the agent was asked to do (natural-language prompt or task spec).
    #[serde(default)]
    pub intent: Option<Hash>,
    /// Blob: what the agent decided to do (plan / outline).
    #[serde(default)]
    pub plan: Option<Hash>,
    /// Blob: tool transcript — reads, edits, errors, retries.
    #[serde(default)]
    pub transcript: Option<Hash>,
    /// Blob: standardized eval results. Schema TBD (ADR-0002 Action 7).
    #[serde(default)]
    pub evals: Option<Hash>,
    /// Compute cost in cents that produced this commit.
    #[serde(default)]
    pub cost_cents: u32,
    /// Platform + reviewer attestations chained over this commit.
    #[serde(default)]
    pub signatures: Vec<Attestation>,
}

impl Commit {
    pub fn hash(&self) -> Hash {
        let bytes = serde_json::to_vec(self).expect("Commit serialization cannot fail");
        Hash::of(&bytes)
    }
}

/// Type-erased object union, used at the store boundary.
///
/// `Commit` is boxed because its ADR-0002-extended size is meaningfully
/// larger than the other variants; the indirection keeps the enum cheap to
/// pass by value. Serialization is unaffected — `Box<Commit>` flattens to
/// the same on-disk bytes as `Commit`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Object {
    Blob(Blob),
    Tree(Tree),
    Commit(Box<Commit>),
    // Segment lives in agentic-memory; the store sees it as opaque bytes.
}

impl Object {
    pub fn kind(&self) -> ObjectKind {
        match self {
            Object::Blob(_) => ObjectKind::Blob,
            Object::Tree(_) => ObjectKind::Tree,
            Object::Commit(_) => ObjectKind::Commit,
        }
    }

    pub fn hash(&self) -> Hash {
        match self {
            Object::Blob(b) => b.hash(),
            Object::Tree(t) => t.hash(),
            Object::Commit(c) => c.hash(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_hash_matches_raw_blake3() {
        let b = Blob::new(b"hello".to_vec());
        assert_eq!(b.hash(), Hash::of(b"hello"));
    }

    #[test]
    fn tree_hash_is_stable_across_insertion_order() {
        let mut t1 = Tree::new();
        t1.insert(
            "a",
            TypedRef {
                kind: ObjectKind::Blob,
                hash: Hash::of(b"x"),
            },
        );
        t1.insert(
            "b",
            TypedRef {
                kind: ObjectKind::Blob,
                hash: Hash::of(b"y"),
            },
        );

        let mut t2 = Tree::new();
        t2.insert(
            "b",
            TypedRef {
                kind: ObjectKind::Blob,
                hash: Hash::of(b"y"),
            },
        );
        t2.insert(
            "a",
            TypedRef {
                kind: ObjectKind::Blob,
                hash: Hash::of(b"x"),
            },
        );

        assert_eq!(t1.hash(), t2.hash(), "BTreeMap must canonicalize order");
    }
}
