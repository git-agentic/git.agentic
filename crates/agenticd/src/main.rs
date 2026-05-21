//! agenticd — the git.agentic daemon.
//!
//! Long-lived Rust process. Listens on a Unix domain socket for SDK and
//! CLI requests, owns the object store, and orchestrates snapshots.

use agentic_core::refs::Refs;
use agentic_memory::postgres::TrackedTable;
use anyhow::{anyhow, Context};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UnixListener;

use agenticd::lifecycle::{reconcile_refs_on_startup, Lifecycle};
use agenticd::mcp::parse_mcp_spec;
use agenticd::objstore::ObjectStoreSpec;
use agenticd::server::{handle_connection, DaemonState};

fn parse_tracked_tables(spec: &[String]) -> anyhow::Result<Vec<TrackedTable>> {
    spec.iter()
        .map(|s| {
            let (name, pk) = s
                .split_once(':')
                .ok_or_else(|| anyhow!("--tables expects table:pk, got {s:?}"))?;
            Ok(TrackedTable {
                name: name.trim().to_string(),
                pk: pk.trim().to_string(),
            })
        })
        .collect()
}

#[derive(Parser, Debug)]
#[command(name = "agenticd", version, about = "git.agentic daemon")]
struct Args {
    /// Path to the repo root (parent of `.agentic/`).
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Socket path. Default: <repo>/.agentic/agenticd.sock.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Postgres URL to attach as the memory backend. If absent, commits
    /// land without a `memory_snapshot` dimension (Chunk A behaviour).
    #[arg(long)]
    postgres: Option<String>,

    /// Comma-separated `table:pk` pairs to track. Required when
    /// `--postgres` is set. Example: `episodes:id,user_facts:id`.
    #[arg(long, value_delimiter = ',')]
    tables: Vec<String>,

    /// Comma-separated `name=url` MCP servers to fingerprint on each
    /// commit. Each `tools/list` response gets canonicalized and hashed
    /// into the commit's `tools` dimension. Example:
    /// `--mcp search=http://localhost:8001,rag=http://localhost:8002/rpc`.
    #[arg(long, value_delimiter = ',')]
    mcp: Vec<String>,

    /// Object-store backend. `fs` (default) uses `<repo>/.agentic/objects`;
    /// `fs:///abs/path` uses an explicit on-disk root; `gcs://bucket[/prefix]`
    /// pushes every object through to Google Cloud Storage with a
    /// write-through local cache (ADR-0004 Decision 5). For GCS, set
    /// `AGENTIC_GCS_ENDPOINT` to override the host (e.g. fake-gcs-server)
    /// and `AGENTIC_GCS_TOKEN` for bearer auth.
    #[arg(long, default_value = "fs")]
    object_store: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let agentic_dir = args.repo.join(".agentic");

    let tables = parse_tracked_tables(&args.tables)?;
    let mcp_servers = parse_mcp_spec(&args.mcp)?;
    let store = ObjectStoreSpec::parse(&args.object_store, &agentic_dir)
        .context("parsing --object-store")?
        .open()
        .context("opening object store")?;
    tracing::info!(spec = %args.object_store, "object store ready");

    // Startup ref reconciliation (audit §A2 / R2). Runs before binding the
    // socket so a corrupted repo never gets traffic — operator sees the
    // error and intervenes.
    {
        let refs = Refs::open(&agentic_dir).context("opening refs at startup")?;
        reconcile_refs_on_startup(&refs, store.as_ref())
            .await
            .context("startup ref reconciliation")?;
    }

    let state = Arc::new(
        DaemonState::open(
            args.repo.clone(),
            agentic_dir.clone(),
            store,
            args.postgres.as_deref(),
            tables,
            mcp_servers,
        )
        .await?,
    );

    let socket_path = args
        .socket
        .unwrap_or_else(|| agentic_dir.join("agenticd.sock"));

    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("removing stale socket {}", socket_path.display()))?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;

    tracing::info!(
        socket = %socket_path.display(),
        repo = %args.repo.display(),
        "agenticd listening"
    );

    // Lifecycle: SIGTERM/SIGINT triggers graceful shutdown; the accept
    // loop watches the shutdown token via tokio::select! and breaks out
    // when raised. After the loop exits, `lifecycle.drain()` blocks on
    // the same `commit_lock` that `handle_commit` / `handle_rollback`
    // hold — guaranteeing no commit is mid-2PC when the process exits.
    // Audit §A2 / R2 / C2.
    //
    // The lifecycle SHARES the shutdown token with DaemonState so the
    // handlers' `state.check_shutdown()` calls see the same signal
    // raised by SIGTERM. Without this, drain would release the lock
    // and a queued waiter would start 2PC inside an exiting LocalSet
    // (Copilot review on PR #50, second pass).
    let lifecycle = Lifecycle::new(state.commit_lock.clone(), state.shutdown.clone());
    lifecycle.install_signal_handlers();
    let shutdown = state.shutdown.clone();

    // Connections are handled on the local task set — no Send bound required,
    // which avoids HRTB issues with sqlx 0.7 async fn signatures. The daemon
    // is a local Unix-socket server so single-threaded cooperative scheduling
    // is the right execution model anyway.
    //
    // The drain step MUST run inside `run_until` so the LocalSet keeps
    // driving any in-flight `spawn_local` commit/rollback task — those
    // tasks hold `commit_lock`, and if the LocalSet stops being polled
    // the drain would deadlock waiting on a lock no task can release.
    // (Spotted by Copilot review on PR #50.)
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::info!("shutdown signal received; closing accept loop");
                        break;
                    }
                    accept = listener.accept() => {
                        let (sock, _addr) = match accept {
                            Ok(pair) => pair,
                            Err(e) => {
                                tracing::warn!(error = %e, "accept failed");
                                continue;
                            }
                        };
                        let state = state.clone();
                        tokio::task::spawn_local(async move {
                            if let Err(e) = handle_connection(state, sock).await {
                                tracing::warn!(error = %format!("{e:#}"), "connection error");
                            }
                        });
                    }
                }
            }
            // Wait for any in-flight commit/rollback to complete its 2PC
            // sequence before the process exits. ADR-0002 Decision 3
            // promises atomic commits; this drain step is what makes that
            // promise survive operator-driven shutdowns (docker stop,
            // kubectl rollout, systemctl restart).
            lifecycle.drain().await;
        })
        .await;

    tracing::info!("agenticd shutdown complete");
    Ok(())
}
