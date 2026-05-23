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

    /// The streamer task has exited and its mpsc receiver is dropped.
    /// Permanent failure mode — a retry can't reopen the channel; the
    /// adapter needs to be torn down and re-bootstrapped. The poller
    /// matches this variant explicitly so it can terminate its own
    /// task instead of looping forever and starving every
    /// `Quiesceable::pause()` caller (restore deadlock).
    #[error("streamer task has shut down (channel closed)")]
    StreamerShutdown,

    #[error(transparent)]
    Core(#[from] agentic_core::Error),

    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
