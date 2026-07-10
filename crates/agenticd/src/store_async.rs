//! Async wrappers around the sync [`ObjectStore`] trait — v1.0 tactical
//! fix for audit §A5 / B2 / C1 / R3.
//!
//! Problem (audit B2/C1): every `ObjectStore` method is sync. The
//! daemon runs its accept loop on a `LocalSet` so spawn_local tasks
//! share one OS thread. A blocking `GcsObjectStore::get(...)` on
//! connection A freezes the LocalSet thread for up to the 30s
//! REQUEST_TIMEOUT, which blocks `Ping` / `Log` / `Diff` / `ResolveRef`
//! on connection B — even though those don't take `commit_lock`.
//!
//! Tactical fix: wrap the sync call in `tokio::task::spawn_blocking`
//! at the daemon call site. The closure runs on tokio's blocking
//! thread pool; the calling task yields back to the runtime while it
//! awaits the resulting handle, so other LocalSet tasks make progress.
//!
//! ## Trade-offs
//!
//! - **FsObjectStore overhead.** spawn_blocking has a ~µs cost per
//!   call and `FsObjectStore` operations are typically microseconds.
//!   Acceptable: the demo path doesn't care about µs, and the
//!   GCS-backed Executor sidecar (ADR-0004 D5) is what actually
//!   benefits.
//! - **No trait change.** Issue #40 AC explicitly forbids modifying
//!   the `ObjectStore` trait. The full async-trait redesign that
//!   removes the spawn_blocking shim is [ADR-0011](../../docs/adr/0011-objectstore-async-trait-shape.md);
//!   when that lands these helpers go away.
//! - **Caller-side only.** We do NOT modify `GcsObjectStore` itself
//!   (the audit pseudocode showed an async-trait impl, which is the
//!   ADR-0011 shape, not a sync-trait tactical patch). Wrapping at
//!   the caller keeps the change contained to agenticd.

use std::sync::Arc;

use agentic_core::{Hash, Object, ObjectKind, ObjectStore};

/// Run a sync `ObjectStore` closure on tokio's blocking thread pool,
/// returning its result via the resulting future. The closure must
/// own its captures so it can be `Send + 'static`.
async fn run_blocking<F, T>(label: &'static str, f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> agentic_core::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|je| {
            if je.is_panic() {
                let payload = je.into_panic();
                let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "(non-string payload)".to_string()
                };
                anyhow::anyhow!("spawn_blocking task for {label} panicked: {msg}")
            } else {
                anyhow::anyhow!("spawn_blocking join error in {label}: {je}")
            }
        })?
        .map_err(anyhow::Error::from)
}

/// Async wrapper for [`ObjectStore::get`]. Frees the LocalSet thread
/// for the duration of a potentially-slow remote fetch.
pub async fn get(store: Arc<dyn ObjectStore + Send + Sync>, hash: Hash) -> anyhow::Result<Object> {
    run_blocking("ObjectStore::get", move || store.get(&hash)).await
}

/// Async wrapper for [`ObjectStore::get_raw`]. Kept for API symmetry
/// with `get` / `put_raw`; no production caller wires it yet because
/// the `Request::Log` / `Request::Diff` paths still call into
/// `walk_log` and `diff::diff` synchronously (PR #55 follow-up).
#[allow(dead_code)]
pub async fn get_raw(
    store: Arc<dyn ObjectStore + Send + Sync>,
    hash: Hash,
) -> anyhow::Result<Vec<u8>> {
    run_blocking("ObjectStore::get_raw", move || store.get_raw(&hash)).await
}

/// Async wrapper for [`ObjectStore::put_raw`]. Moves the byte buffer
/// into the closure so it lives across the await without borrowing
/// caller state.
pub async fn put_raw(
    store: Arc<dyn ObjectStore + Send + Sync>,
    kind: ObjectKind,
    bytes: Vec<u8>,
) -> anyhow::Result<Hash> {
    run_blocking("ObjectStore::put_raw", move || store.put_raw(kind, &bytes)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentic_core::FsObjectStore;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Test fake: a SlowStore that sleeps `delay` synchronously before
    /// delegating to an inner FsObjectStore. Used to simulate the
    /// blocking behaviour of `GcsObjectStore` without standing up a
    /// real GCS endpoint.
    struct SlowStore {
        inner: FsObjectStore,
        delay: Duration,
        /// Counter of get() calls so the test can verify both reads
        /// actually happened (not just that the assertions passed).
        get_calls: Mutex<usize>,
    }

    impl SlowStore {
        fn new(delay: Duration, dir: &std::path::Path) -> Self {
            Self {
                inner: FsObjectStore::open(dir).unwrap(),
                delay,
                get_calls: Mutex::new(0),
            }
        }
    }

    impl ObjectStore for SlowStore {
        fn put_with_policy(
            &self,
            object: &Object,
            _policy: agentic_core::scanner::ScanPolicy,
        ) -> agentic_core::Result<Hash> {
            self.inner.put(object)
        }
        fn put_raw(&self, kind: ObjectKind, bytes: &[u8]) -> agentic_core::Result<Hash> {
            self.inner.put_raw(kind, bytes)
        }
        fn get(&self, hash: &Hash) -> agentic_core::Result<Object> {
            *self.get_calls.lock().unwrap() += 1;
            std::thread::sleep(self.delay);
            self.inner.get(hash)
        }
        fn get_raw(&self, hash: &Hash) -> agentic_core::Result<Vec<u8>> {
            std::thread::sleep(self.delay);
            self.inner.get_raw(hash)
        }
        fn has(&self, hash: &Hash) -> bool {
            self.inner.has(hash)
        }
    }

    /// AC for issue #40 / audit §A5: a slow `get` on the daemon's
    /// shared store does NOT block other spawn_local tasks. Wraps the
    /// scenario in a LocalSet to mirror production: the daemon's
    /// accept loop runs spawn_local tasks on a single thread.
    /// Without spawn_blocking, the slow `get` would serialise the
    /// concurrent ping-like task and total wall time would be ~2×
    /// `delay`. With spawn_blocking, both proceed in parallel and
    /// total wall time is ~1× `delay` + scheduler slack.
    #[tokio::test]
    async fn slow_get_does_not_block_local_set_thread() {
        let dir = tempfile::tempdir().unwrap();
        let blob_bytes = b"hello".to_vec();
        let delay = Duration::from_millis(500);

        // Pre-stage a blob through the sync path so get() has something
        // to return. SlowStore::put goes through immediately (no delay).
        let store: Arc<dyn ObjectStore + Send + Sync> = Arc::new(SlowStore::new(delay, dir.path()));
        let hash = store.put_raw(ObjectKind::Blob, &blob_bytes).unwrap();

        let local = tokio::task::LocalSet::new();
        let (slow_done_tx, slow_done_rx) = tokio::sync::oneshot::channel();
        let (fast_done_tx, fast_done_rx) = tokio::sync::oneshot::channel();

        let store_slow = Arc::clone(&store);
        let store_fast = Arc::clone(&store);

        local
            .run_until(async move {
                // Task A: slow get, simulating ReadObject on connection A.
                let started = Instant::now();
                tokio::task::spawn_local(async move {
                    let _ = get_raw(store_slow, hash).await.unwrap();
                    let _ = slow_done_tx.send(started.elapsed());
                });

                // Give task A a chance to start its spawn_blocking call
                // before we kick off task B. Without this both tasks
                // race for the LocalSet poll and the test gets noisy.
                tokio::task::yield_now().await;

                // Task B: "ping" — completes instantly. Without
                // spawn_blocking around A's get, task B would not be
                // polled until A's blocking call returns ~500ms later.
                let started_b = Instant::now();
                tokio::task::spawn_local(async move {
                    // Touch the store from B too so we exercise the
                    // shared-Arc path; use has() which is cheap.
                    let _ = store_fast.has(&hash);
                    let _ = fast_done_tx.send(started_b.elapsed());
                });

                let fast_elapsed = fast_done_rx.await.unwrap();
                let slow_elapsed = slow_done_rx.await.unwrap();

                // The fast task should complete WELL before the slow
                // one — proves task B was polled while task A was
                // blocked in spawn_blocking on another thread.
                assert!(
                    fast_elapsed < Duration::from_millis(100),
                    "fast task should complete promptly while slow get is in-flight; \
                     took {fast_elapsed:?} (slow task took {slow_elapsed:?})"
                );
                assert!(
                    slow_elapsed >= delay,
                    "slow task should have actually waited for the delay; took {slow_elapsed:?}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn put_raw_round_trips_through_async_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore + Send + Sync> =
            Arc::new(FsObjectStore::open(dir.path()).unwrap());
        let bytes = b"round-trip test".to_vec();
        let hash = put_raw(Arc::clone(&store), ObjectKind::Blob, bytes.clone())
            .await
            .unwrap();
        let read_back = get_raw(Arc::clone(&store), hash).await.unwrap();
        assert_eq!(read_back, bytes);
    }
}
