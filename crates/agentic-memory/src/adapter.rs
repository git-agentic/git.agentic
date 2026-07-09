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

/// One step in a reverse-migration plan.
///
/// Produced by the daemon's `.down.sql` loader
/// (`agenticd::migrate::load_steps`) in the order returned by
/// [`MemoryAdapter::migrations_after`] — most-recent first. `sql` is the
/// pre-read file content so backends never do filesystem I/O.
#[derive(Debug, Clone)]
pub struct MigrationStep {
    /// Migration stem name as recorded by the backend's bookkeeping
    /// (e.g. `"003_add_embeddings"` in Postgres's `agentic_migrations`).
    pub name: String,
    /// Reverse-migration content. SQL for SQL backends; other backends
    /// may ignore it and reverse by name alone.
    pub sql: String,
}

/// Resume-on-drop token returned by [`MemoryAdapter::begin_restore`].
///
/// Adapters that need to pause background work for the restore window
/// (Postgres pauses its trigger poller so user-side TRUNCATE+INSERT
/// doesn't get re-streamed) wrap their internal token here; backends
/// with no quiesce requirement return [`RestoreGuard::noop`]. In either
/// case the guard's `Drop` runs the adapter's resume side — there is
/// no `resume()` method, only `drop`.
///
/// **`Send + Sync` is required of the inner payload, but Rust runs an
/// owned type's `Drop` impl whenever a `Box<T>` is dropped (drop glue,
/// not a vtable method).** So dropping the guard drops the payload,
/// which fires the adapter's resume `Drop`.
#[must_use = "dropping this guard immediately resumes the adapter's background work; \
              hold it for the entire restore window or the restored state may diverge \
              from actual storage"]
pub struct RestoreGuard {
    _payload: Box<dyn Send + Sync>,
}

impl RestoreGuard {
    /// Construct a guard that owns `payload`. When the guard drops,
    /// `payload`'s `Drop` impl runs — adapters put their resume logic
    /// in the payload's destructor.
    ///
    /// `pub(crate)` so external callers can't forge a "quiesced" guard
    /// and hand it to an adapter's `restore_with_guard`. Adapters
    /// inside this crate construct their own guards; outside callers
    /// only ever get one from `MemoryAdapter::begin_restore` or
    /// [`RestoreGuard::noop`].
    pub(crate) fn new<T: Send + Sync + 'static>(payload: T) -> Self {
        Self {
            _payload: Box::new(payload),
        }
    }

    /// No-op guard for backends that don't pause anything (in-memory,
    /// stub, test fixture). Safe to hand to any
    /// [`MemoryAdapter::restore_with_guard`] *whose backend itself does
    /// not require quiescing* — passing a `noop` to a Postgres-backed
    /// adapter is a logic error.
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
/// Reverse-migration application is a single coarse method
/// ([`Self::apply_reverse_migrations`]) rather than a begin/apply/commit
/// triple: sqlx 0.8's `Executor<'c>` HRTBs don't unify across
/// async_trait's boxed-future elision, so a transaction handle cannot
/// cross the trait boundary. Each backend owns its own atomicity
/// mechanism instead (audit §A9, issue #43).
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
    /// paused. The guard's `Drop` resumes it — drop it too early and
    /// the adapter's data-capture machinery runs concurrently with the
    /// restore's TRUNCATE+INSERT.
    #[must_use = "the returned guard must be held for the entire restore window; \
                  see RestoreGuard's #[must_use] for the failure mode"]
    async fn begin_restore(&self) -> Result<RestoreGuard>;

    /// Restore the snapshot while holding a `RestoreGuard`. The schema
    /// gate fires first — restore aborts with `Error::SchemaMismatch` if
    /// the live schema is not the same as the target's. Backends are
    /// responsible for replaying the manifest atomically (so a partial
    /// failure does not leave the user database in an intermediate state).
    ///
    /// **Precondition (not enforceable at compile time):** `guard` must
    /// have been produced by *this adapter's* [`Self::begin_restore`].
    /// Passing a guard from a different adapter, or a
    /// [`RestoreGuard::noop`] to a backend whose data capture needs
    /// quiescing, is a logic error — the type system can't catch it
    /// because the guard is opaque. Callers thread the guard straight
    /// from `begin_restore` to `restore_with_guard`; agenticd's
    /// `rollback::execute` is the only production caller and follows
    /// this pattern.
    async fn restore_with_guard(&self, guard: &RestoreGuard, target: &SnapshotHandle)
        -> Result<()>;

    /// Apply reverse (down) migrations, in the given order, atomically.
    ///
    /// All-or-nothing: if any step fails, the backend's observable state
    /// (schema, data, and migration bookkeeping) must be unchanged.
    /// Postgres implements this as one transaction committed only after
    /// every step succeeds. An empty `steps` slice is a no-op.
    ///
    /// `steps` must be in the order returned by
    /// [`Self::migrations_after`] — most-recent first.
    async fn apply_reverse_migrations(&self, steps: &[MigrationStep]) -> Result<()>;
}
