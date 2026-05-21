//! Daemon lifecycle: graceful shutdown + startup ref reconciliation.
//!
//! Addresses audit findings [§A2 / C2 / B7 / R2](../../../docs/ops/2026-05-21-agenticd-architectural-analysis.md#a2)
//! (issue [#36](https://github.com/git-agentic/git.agentic/issues/36)):
//!
//! - **C2** — no SIGTERM handler in `main.rs` meant `docker stop` mid-commit
//!   killed the process before the 2PC staging order completed, leaving
//!   Postgres memory state and the on-disk branch ref pointing at
//!   different versions of reality. ADR-0002 Decision 3 promises atomic
//!   commits; without a drain step that promise was conditional on the
//!   operator never interrupting the daemon.
//! - **B7** — a related case fixed at the call-site (see
//!   `server::handle_commit`): on first-ever commit, the old code wrote
//!   HEAD before any staging step ran. If staging failed (or the process
//!   was killed mid-stage), HEAD pointed at a branch ref that was never
//!   published — a phantom HEAD. The fix moves the HEAD-write to AFTER
//!   `stage_and_commit` returns Ok; the reconciler here is the
//!   defence-in-depth net for legacy or out-of-band-corrupted repos.
//!
//! The module exposes a single struct [`Lifecycle`] that owns the
//! shutdown signal and the shared `commit_lock`, plus a free function
//! [`reconcile_refs_on_startup`] the binary calls before binding its
//! socket.

use std::sync::Arc;

use agentic_core::refs::Refs;
use agentic_core::ObjectStore;
use anyhow::{anyhow, Context};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Lifecycle owns the daemon's shutdown signal and (a clone of) the
/// shared commit_lock. The binary creates one of these at startup and
/// uses [`Lifecycle::install_signal_handlers`] to wire SIGTERM/SIGINT
/// into the shutdown token; the accept loop watches
/// [`Lifecycle::shutdown_token`] via `tokio::select!` to break out
/// cleanly; before exiting, [`Lifecycle::drain`] is awaited so any
/// in-flight commit completes its 2PC sequence before the process dies.
pub struct Lifecycle {
    shutdown: CancellationToken,
    commit_lock: Arc<Mutex<()>>,
}

impl Lifecycle {
    /// Build a lifecycle that shares both `commit_lock` AND the
    /// `shutdown` token with [`crate::server::DaemonState`]. The token
    /// must be the SAME `CancellationToken` (or an `Arc`-clone of it)
    /// the state holds, so that signal-driven cancellation reaches the
    /// write-path handlers' `state.check_shutdown()` calls. (Copilot
    /// review on PR #50: without sharing the token, drain releases the
    /// lock and a queued waiter starts a 2PC the LocalSet is about to
    /// abort.)
    pub fn new(commit_lock: Arc<Mutex<()>>, shutdown: CancellationToken) -> Self {
        Self {
            shutdown,
            commit_lock,
        }
    }

    /// Clone the shutdown token. The accept loop selects on this; any
    /// task that wants to react to shutdown can call `.cancelled().await`.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Spawn a background task that fires the shutdown token when the
    /// process receives SIGTERM or SIGINT (or Ctrl+C on non-Unix).
    /// Idempotent on the signal — both raise the same shutdown.
    ///
    /// If signal handler installation fails (extremely rare on
    /// supported platforms), the error is logged and the task falls
    /// back to whichever handler did install, or — if both fail — to
    /// `ctrl_c`. The shutdown token is still cancelled when ANY signal
    /// path fires, so the daemon never silently loses its ability to
    /// shut down gracefully. (Copilot review on PR #50: previously
    /// used `expect(...)` which would have panicked the detached task
    /// and stranded the shutdown token forever.)
    pub fn install_signal_handlers(&self) {
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            shutdown.cancel();
        });
    }

    /// Block until any in-flight commit finishes. Acquires the same
    /// `commit_lock` that `handle_commit` and `handle_rollback` hold
    /// while running their 2PC sequences — so when this returns, the
    /// daemon is in a "no commit in progress" state and can exit
    /// without violating ADR-0002 Decision 3's atomicity guarantee.
    pub async fn drain(&self) {
        tracing::info!("draining commit_lock for graceful shutdown");
        let _g = self.commit_lock.lock().await;
        tracing::info!("commit_lock acquired; no commit in progress — exiting");
    }
}

/// Block the current task until SIGTERM, SIGINT (on Unix), or Ctrl+C
/// (on non-Unix) fires. Signal-handler installation failures are logged
/// and the function degrades gracefully to whichever handler did install.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let sigterm = signal(SignalKind::terminate());
        let sigint = signal(SignalKind::interrupt());
        match (sigterm, sigint) {
            (Ok(mut term), Ok(mut int)) => {
                tokio::select! {
                    _ = term.recv() => {
                        tracing::info!("SIGTERM received; initiating graceful shutdown");
                    }
                    _ = int.recv() => {
                        tracing::info!("SIGINT received; initiating graceful shutdown");
                    }
                }
            }
            (Ok(mut term), Err(e)) => {
                tracing::error!(error = %e, "failed to install SIGINT handler; SIGTERM remains active");
                let _ = term.recv().await;
                tracing::info!("SIGTERM received; initiating graceful shutdown");
            }
            (Err(e), Ok(mut int)) => {
                tracing::error!(error = %e, "failed to install SIGTERM handler; SIGINT remains active");
                let _ = int.recv().await;
                tracing::info!("SIGINT received; initiating graceful shutdown");
            }
            (Err(term_e), Err(int_e)) => {
                tracing::error!(
                    sigterm_error = %term_e,
                    sigint_error = %int_e,
                    "failed to install SIGTERM and SIGINT handlers; falling back to ctrl_c"
                );
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("Ctrl-C received via fallback; initiating graceful shutdown");
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "ctrl_c handler returned error; shutting down anyway");
        }
        tracing::info!("Ctrl-C received; initiating graceful shutdown");
    }
}

/// Walk every branch ref under `<agentic_dir>/refs/heads/` and verify
/// the tip hash exists in the object store. A missing object means
/// either the object store was corrupted, or a previous daemon process
/// crashed between the 2PC `put_raw` and `write_branch` steps.
///
/// We deliberately do NOT silently "rewind one parent back": without
/// the commit blob we can't read the parent hash, and a non-malicious
/// crash usually leaves the BRANCH ref still pointing at the previous
/// (valid) tip — the orphan is just the in-progress commit blob. The
/// safe behavior is to detect the inconsistency, log it loudly with
/// enough detail for the operator to clean up, and refuse to start.
///
/// Returns `Ok(())` for a healthy repo or one with no branches yet
/// (fresh daemon). Returns `Err` if any branch's tip is missing from
/// the store.
pub async fn reconcile_refs_on_startup(refs: &Refs, store: &dyn ObjectStore) -> anyhow::Result<()> {
    let branches = refs
        .list_branches()
        .context("listing branches at startup")?;
    let mut broken: Vec<(String, String)> = Vec::new();
    for branch in &branches {
        let Some(tip) = refs
            .read_branch(branch)
            .with_context(|| format!("reading branch ref {branch}"))?
        else {
            continue;
        };
        if !store.has(&tip) {
            broken.push((branch.clone(), tip.to_hex()));
        }
    }
    if broken.is_empty() {
        tracing::info!(
            branches = branches.len(),
            "startup ref reconciliation passed"
        );
        return Ok(());
    }
    for (branch, tip) in &broken {
        tracing::error!(
            branch = %branch,
            missing_tip = %tip,
            "branch ref tip is missing from the object store — refusing to start"
        );
    }
    Err(anyhow!(
        "startup ref reconciliation failed: {} branch ref(s) point at object(s) missing from the store \
         ({}). The daemon was likely killed mid-commit before the ref update completed, or the object \
         store was corrupted. Inspect <.agentic/refs/heads/>; the safe recovery is to manually rewind \
         the affected branch ref(s) to their previous tip (or to delete them if no recoverable parent \
         exists).",
        broken.len(),
        broken
            .iter()
            .map(|(b, t)| format!("{b}@{}", &t[..16]))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentic_core::{FsObjectStore, Hash, ObjectKind};
    use std::time::Duration;

    fn open_refs_and_store() -> (tempfile::TempDir, Refs, FsObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::open(dir.path()).unwrap();
        let store = FsObjectStore::open(dir.path().join("objects")).unwrap();
        (dir, refs, store)
    }

    #[tokio::test]
    async fn reconcile_passes_on_fresh_repo() {
        let (_dir, refs, store) = open_refs_and_store();
        // No branches yet — should pass cleanly.
        reconcile_refs_on_startup(&refs, &store).await.unwrap();
    }

    #[tokio::test]
    async fn reconcile_passes_when_branch_tip_is_in_store() {
        let (_dir, refs, store) = open_refs_and_store();
        let h = store
            .put_raw(
                ObjectKind::Blob,
                b"a commit blob (any bytes work for the has() check)",
            )
            .unwrap();
        refs.write_branch("main", &h).unwrap();
        reconcile_refs_on_startup(&refs, &store).await.unwrap();
    }

    // AC for issue #36: a branch ref pointing at a hash that's not in
    // the object store is the visible signature of a daemon killed
    // mid-commit (after `put_raw` then before the next write fired, or
    // — pre-fix — between write_head_symbolic and stage_and_commit).
    // The reconciler must detect this and refuse to start.
    #[tokio::test]
    async fn reconcile_rejects_branch_ref_with_missing_tip() {
        let (_dir, refs, store) = open_refs_and_store();
        // Fabricate a hash that won't be in the store.
        let phantom = Hash::of(b"this-commit-was-never-published");
        refs.write_branch("main", &phantom).unwrap();
        let err = reconcile_refs_on_startup(&refs, &store).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("missing from the store"),
            "error should mention the missing-from-store condition; got: {msg}"
        );
        assert!(
            msg.contains("main"),
            "error should name the affected branch; got: {msg}"
        );
    }

    #[tokio::test]
    async fn reconcile_lists_every_broken_branch() {
        let (_dir, refs, store) = open_refs_and_store();
        // One healthy branch, two broken.
        let healthy = store.put_raw(ObjectKind::Blob, b"ok").unwrap();
        refs.write_branch("main", &healthy).unwrap();
        refs.write_branch("feature-a", &Hash::of(b"phantom-a"))
            .unwrap();
        refs.write_branch("feature-b", &Hash::of(b"phantom-b"))
            .unwrap();
        let err = reconcile_refs_on_startup(&refs, &store).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("feature-a"));
        assert!(msg.contains("feature-b"));
        assert!(
            !msg.contains("main@"),
            "healthy branch should not appear in the error: {msg}"
        );
    }

    // CI-stability note: tests below use `tokio::sync::Notify` for
    // handshakes between the main task and a spawned "commit" task,
    // rather than `tokio::time::sleep`. Sleep-based handshakes are flaky
    // on contended CI runners (Copilot review on PR #50, second pass).
    use tokio::sync::Notify;

    fn new_lifecycle() -> (Arc<Mutex<()>>, Lifecycle) {
        let lock = Arc::new(Mutex::new(()));
        let lifecycle = Lifecycle::new(lock.clone(), CancellationToken::new());
        (lock, lifecycle)
    }

    #[tokio::test]
    async fn drain_returns_promptly_when_no_commit_in_flight() {
        let (_lock, lifecycle) = new_lifecycle();
        // 5s is enough to distinguish "returns quickly" from "deadlocks"
        // on any realistic CI runner. The previous 50ms guard was too
        // tight under heavy load.
        let result = tokio::time::timeout(Duration::from_secs(5), lifecycle.drain()).await;
        assert!(
            result.is_ok(),
            "drain should return promptly when commit_lock is uncontended"
        );
    }

    // The atomicity-promise-keeping behavior: if a commit is in progress
    // (commit_lock held), drain blocks until it releases — i.e., until
    // the 2PC sequence completes. The daemon process therefore never
    // exits while a partial commit is on the wire.
    #[tokio::test]
    async fn drain_waits_for_in_flight_commit_to_finish() {
        let (lock, lifecycle) = new_lifecycle();

        let acquired = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let commit_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let lock_for_commit = lock.clone();
        let acquired_signal = acquired.clone();
        let release_wait = release.clone();
        let flag = commit_finished.clone();
        tokio::spawn(async move {
            let _g = lock_for_commit.lock_owned().await;
            acquired_signal.notify_one();
            // Wait for the test to permit release — deterministic
            // ordering, no time-based race.
            release_wait.notified().await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        // Deterministic handshake: wait for the spawned task to confirm
        // it has the lock before we start drain.
        acquired.notified().await;

        // drain() must now block. Confirm it doesn't complete while the
        // lock is still held by the "commit" task.
        let drain_fut = lifecycle.drain();
        tokio::pin!(drain_fut);
        tokio::select! {
            _ = &mut drain_fut => {
                panic!("drain returned while commit_lock was still held");
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                // Expected — drain is correctly blocked.
            }
        }

        // Now let the commit finish. drain should unblock shortly after.
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), drain_fut)
            .await
            .expect("drain did not complete after commit released the lock");

        assert!(
            commit_finished.load(std::sync::atomic::Ordering::SeqCst),
            "drain returned before in-flight commit finished — violates ADR-0002 D3 atomicity"
        );
    }

    #[tokio::test]
    async fn shutdown_token_initially_not_cancelled() {
        let (_lock, lifecycle) = new_lifecycle();
        let token = lifecycle.shutdown_token();
        assert!(!token.is_cancelled());
    }
}
