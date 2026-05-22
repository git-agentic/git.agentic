//! agentic-memory
//!
//! Memory-backend adapters. For MVP the only first-class backend is
//! Postgres + pgvector. Other backends (Mem0, Zep, Letta) implement the
//! `MemoryAdapter` trait in v1.1.
//!
//! This crate also defines the `Segment` object — a content-addressed,
//! immutable, append-only chunk of a memory table. Snapshots are Merkle
//! trees over segments; see `docs/architecture/snapshot-model.md`.

pub mod adapter;
pub mod in_memory;
pub mod postgres;
pub mod restore;
pub mod segment;
pub mod streamer;
pub mod triggers;

pub use adapter::{MemoryAdapter, RestoreGuard, SnapshotHandle};
pub use segment::{Embedding, Segment, SegmentManifest, SegmentRef, DEFAULT_SEGMENT_TARGET_BYTES};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("backend error: {0}")]
    Backend(String),

    #[error("schema version mismatch: live {live:?}, target {target:?}")]
    SchemaMismatch { live: String, target: String },

    #[error("required reverse migration missing for: {0}")]
    MissingReverseMigration(String),

    #[error(transparent)]
    Core(#[from] agentic_core::Error),

    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
