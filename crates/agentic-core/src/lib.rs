//! agentic-core
//!
//! The content-addressed object store and snapshot engine for git.agentic.
//!
//! This crate implements the object model described in
//! `docs/architecture/snapshot-model.md`: blobs, trees, segments, and commits,
//! each identified by the BLAKE3 hash of their canonical serialization.
//!
//! MVP status: object kinds and the hash type are defined. The on-disk store
//! and snapshot/restore algorithms are stubbed and land in weeks 1–5 of the
//! roadmap.

pub mod commit;
pub mod hash;
pub mod object;
pub mod refs;
pub mod store;

pub use hash::Hash;
pub use object::{Attestation, Blob, Commit, Object, ObjectKind, Tree, TypedRef};
pub use refs::Refs;
pub use store::{FsObjectStore, ObjectStore};

/// Crate-level error type. Individual modules contribute their own variants.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("object not found: {0}")]
    NotFound(Hash),

    #[error("object kind mismatch: expected {expected:?}, got {actual:?}")]
    KindMismatch {
        expected: ObjectKind,
        actual: ObjectKind,
    },

    #[error("object integrity error: declared {declared}, computed {computed}")]
    IntegrityError { declared: Hash, computed: Hash },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
