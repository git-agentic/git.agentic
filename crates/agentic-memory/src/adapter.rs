//! The trait every memory backend implements.
//!
//! Stays small on purpose. A backend is responsible for:
//!   1. Streaming new writes into segments (the "writer").
//!   2. Producing a snapshot handle on demand (the "snapshotter").
//!   3. Restoring state from a snapshot handle (the "restorer").
//!
//! Everything else — content-addressing, manifest assembly, object-store
//! integration — lives in agentic-core.

use crate::segment::SegmentManifest;
use crate::Result;

/// Opaque handle to a memory snapshot, embedded in a commit object.
#[derive(Clone, Debug)]
pub struct SnapshotHandle {
    pub manifest: SegmentManifest,
    pub schema_version: String,
}

/// The contract a backend implements. Async for the obvious reasons.
#[async_trait::async_trait]
pub trait MemoryAdapter: Send + Sync {
    /// Bring up the adapter against an existing user database. Runs any
    /// one-time setup (replication slot, helper functions) and begins
    /// streaming new writes into segments.
    async fn init(&mut self) -> Result<()>;

    /// Capture a coherent point-in-time snapshot. Must complete in <2s on
    /// 1M-row tables for MVP. Pauses writes only for the brief copy-on-write
    /// window; the rest is read-mostly.
    async fn snapshot(&self) -> Result<SnapshotHandle>;

    /// Restore state to a previous snapshot. Computes a segment diff,
    /// streams differing rows back into the user's database, and runs
    /// reverse schema migrations as needed.
    async fn restore(&self, target: &SnapshotHandle) -> Result<()>;

    /// Return the live schema version (read from the user's database).
    async fn current_schema_version(&self) -> Result<String>;
}
