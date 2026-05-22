//! The trait every memory backend implements.
//!
//! Stays small on purpose. A backend is responsible for:
//!   1. Streaming new writes into segments (the "writer").
//!   2. Producing a snapshot handle on demand (the "snapshotter").
//!   3. Restoring state from a snapshot handle (the "restorer").
//!   4. Running reverse schema migrations as part of a rollback.
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

/// Resume-on-drop token returned by [`MemoryAdapter::begin_restore`].
///
/// Adapters that need to pause background work for the restore window
/// (Postgres pauses its trigger poller so user-side TRUNCATE+INSERT
/// doesn't get re-streamed) wrap their internal token here; backends
/// with no quiesce requirement return [`RestoreGuard::noop`]. In either
/// case the guard's `Drop` runs the adapter's resume side — there is no
/// `resume()` method, only `drop`.
pub struct RestoreGuard {
    // Box of trait-object payload: the vtable carries the inner type's
    // destructor, so when the guard drops the payload drops, which fires
    // the adapter-specific resume Drop impl.
    _payload: Box<dyn Send + Sync>,
}

impl RestoreGuard {
    /// Construct a guard that owns `payload`. When the guard drops,
    /// `payload`'s `Drop` impl runs — adapters put their resume logic
    /// in the payload's destructor.
    pub fn new<T: Send + Sync + 'static>(payload: T) -> Self {
        Self {
            _payload: Box::new(payload),
        }
    }

    /// No-op guard for backends that don't pause anything (in-memory,
    /// stub, test fixture).
    pub fn noop() -> Self {
        Self::new(())
    }
}

/// The contract a backend implements. Async for the obvious reasons.
///
/// Every method past `init` takes `&self` so adapters can be wrapped in
/// `Arc<dyn MemoryAdapter>` without an outer `Mutex` — backends use
/// interior mutability for any state that must change between calls.
/// Application-level serialisation (e.g. agenticd's `commit_lock`) is
/// the contract that prevents racing snapshots and restores; the trait
/// does not impose it itself.
///
/// Reverse-migration application is intentionally not a trait method
/// in v1.0 — `PostgresAdapter` exposes
/// `begin_reverse_tx` / `apply_down_migration_tx` as inherent methods
/// that thread a `sqlx::Transaction<'_, Postgres>` through the
/// caller, and `agenticd::migrate::run_reverse` uses those directly.
/// Lifting that to a trait method runs into the sqlx 0.8 + async_trait
/// + `Executor<'c>` HRTB incompatibility (the `&mut PgConnection`'s
/// per-borrow lifetime can't unify across the boxed future's elision).
/// When the second real backend lands and we have evidence for the
/// right abstraction, this gets reopened.
#[async_trait::async_trait]
pub trait MemoryAdapter: Send + Sync {
    /// Bring up the adapter against an existing user database. Runs any
    /// one-time setup (replication slot, helper functions) and begins
    /// streaming new writes into segments. Called once before the
    /// adapter is wrapped in `Arc<dyn MemoryAdapter>`.
    async fn init(&mut self) -> Result<()>;

    /// Capture a coherent point-in-time snapshot. Must complete in <2s on
    /// 1M-row tables for MVP. Pauses writes only for the brief copy-on-write
    /// window; the rest is read-mostly.
    async fn snapshot(&self) -> Result<SnapshotHandle>;

    /// Restore state to a previous snapshot. Convenience entry-point —
    /// equivalent to `begin_restore` followed by `restore_with_guard`.
    /// Callers that need the quiesce window visible at the call site
    /// (e.g. agenticd's rollback path) use the two methods directly.
    async fn restore(&self, target: &SnapshotHandle) -> Result<()>;

    /// Return the live schema version (read from the user's database).
    async fn current_schema_version(&self) -> Result<String>;

    /// Return migration names applied after `target_name`, ordered from
    /// most-recent to least-recent — the order they must be reversed.
    ///
    /// `target_name == "0.0.0"` is the convention for "before any
    /// migration"; adapters return every recorded migration in that case.
    /// If `target_name` is not in the adapter's bookkeeping and is not
    /// the baseline, the adapter must error (reversing an unknown target
    /// is unsafe).
    async fn migrations_after(&self, target_name: &str) -> Result<Vec<String>>;

    /// Begin a restore window. Returns a [`RestoreGuard`] whose existence
    /// proves background work (trigger polling, write streaming) is
    /// paused. The guard's `Drop` resumes it.
    async fn begin_restore(&self) -> Result<RestoreGuard>;

    /// Restore the snapshot while holding a `RestoreGuard`. The schema
    /// gate fires first — restore aborts with `Error::SchemaMismatch` if
    /// the live schema is not the same as the target's. Backends are
    /// responsible for replaying the manifest atomically (so a partial
    /// failure does not leave the user database in an intermediate state).
    async fn restore_with_guard(&self, guard: &RestoreGuard, target: &SnapshotHandle)
        -> Result<()>;
}
