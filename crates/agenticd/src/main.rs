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
use agenticd::peer_auth::PeerAuthPolicy;
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

    /// UID allowed to connect to the socket. Repeatable. Required in
    /// production deployments; the daemon refuses to start without at
    /// least one --allowed-uid unless --insecure-allow-any-uid is
    /// explicitly passed. Per ADR-0012.
    #[arg(long = "allowed-uid")]
    allowed_uids: Vec<u32>,

    /// Disable peer-UID enforcement on the socket. Demo and macOS-
    /// native development only — production deployments MUST NOT use
    /// this flag. Logged loudly at startup.
    #[arg(long)]
    insecure_allow_any_uid: bool,

    /// Path to the secret-scanner allowlist (TOML). ADR-0013 D4.
    /// Default: `<repo>/.agentic/scanner-allowlist.toml`. A missing
    /// file is treated as an empty allowlist; only invalid TOML or
    /// invalid blob_hash entries are fatal.
    #[arg(long)]
    scanner_allowlist: Option<PathBuf>,

    /// Path to the 32-byte operator approval key for destructive rollback
    /// (ADR-0014). Without it, every `accept_data_loss = true` rollback is
    /// rejected fail-closed. If the flag is passed but the file exists and
    /// is readable yet is not exactly 32 bytes, the daemon aborts startup
    /// (an operator who configured a key but got it wrong should find out
    /// loudly); an absent, unreadable, or empty file leaves the daemon
    /// running with destructive rollback disabled.
    #[arg(long)]
    approval_key_file: Option<PathBuf>,

    /// Global cap on concurrently open socket connections. Issue #118.
    #[arg(long, default_value_t = 64)]
    max_connections: usize,

    /// Per-UID cap on concurrently open socket connections. Keys on the
    /// observed SO_PEERCRED UID in both auth modes. Issue #118.
    #[arg(long, default_value_t = 16)]
    max_connections_per_uid: usize,

    /// Per-UID request rate budget in requests/second (burst capacity
    /// is 2x). Exhaustion gets a retryable Concurrency-class reply;
    /// the connection survives. Issue #118.
    #[arg(long, default_value_t = 200)]
    rate_per_uid: u32,

    /// Max requests queued-or-executing on the daemon's commit lock.
    /// Overflow gets a retryable Concurrency-class reply. Issue #118.
    #[arg(long, default_value_t = 8)]
    commit_queue_depth: usize,

    /// Seconds a connection may go without completing an inbound frame
    /// before it is closed. Per-frame clock. Issue #118 (promotes the
    /// previously hardcoded 30s read-idle timeout).
    #[arg(long, default_value_t = 30)]
    read_idle_secs: u64,

    /// Seconds a response write may stall (peer not reading) before the
    /// connection is closed. Issue #118.
    #[arg(long, default_value_t = 30)]
    write_idle_secs: u64,
}

/// Load the ADR-0014 approval key per Decisions 4 and 6.
///
/// Reconciliation of the two decisions, keeping the abort case narrow so a
/// key that only gates destructive rollback can't take down all daemon
/// traffic:
/// * flag absent, or file unreadable, or file empty → `None` (fail-closed;
///   destructive rollback rejected, daemon runs — Decision 4).
/// * file readable and non-empty but not exactly 32 bytes → abort startup
///   (clear operator misconfiguration — Decision 6).
fn load_approval_key(
    path: Option<&std::path::Path>,
) -> anyhow::Result<Option<agentic_core::approval::ApprovalKey>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "agenticd::approval",
                path = %path.display(),
                error = %e,
                "approval key file unreadable; destructive rollback disabled (fail-closed)"
            );
            return Ok(None);
        }
    };
    if bytes.is_empty() {
        tracing::warn!(
            target: "agenticd::approval",
            path = %path.display(),
            "approval key file is empty; destructive rollback disabled (fail-closed)"
        );
        return Ok(None);
    }
    let key = agentic_core::approval::ApprovalKey::from_bytes(&bytes).with_context(|| {
        format!(
            "approval key file {} is malformed; refusing to start",
            path.display()
        )
    })?;
    tracing::info!(target: "agenticd::approval", "approval key loaded; destructive rollback enabled");
    Ok(Some(key))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();

    // ADR-0012: build the peer-auth policy from CLI flags BEFORE any I/O.
    // The daemon refuses to start without an explicit policy choice;
    // --insecure-allow-any-uid is loudly warned about at startup.
    let peer_auth = match (args.allowed_uids.is_empty(), args.insecure_allow_any_uid) {
        (true, false) => {
            return Err(anyhow::anyhow!(
                "agenticd refuses to start without peer-UID enforcement.\n\
                 Pass --allowed-uid <UID> (repeatable) to enable the allowlist,\n\
                 or pass --insecure-allow-any-uid explicitly to disable enforcement\n\
                 (demo and macOS-native development only — never in production)."
            ));
        }
        (false, true) => {
            return Err(anyhow::anyhow!(
                "--allowed-uid and --insecure-allow-any-uid are mutually exclusive.\n\
                 Pass one or the other, not both."
            ));
        }
        (true, true) => PeerAuthPolicy::InsecureAllowAny,
        (false, false) => PeerAuthPolicy::Allowlist(args.allowed_uids.iter().copied().collect()),
    };

    if matches!(peer_auth, PeerAuthPolicy::InsecureAllowAny) {
        tracing::warn!(
            target: "agenticd::accept",
            "running with --insecure-allow-any-uid; every socket connection is \
             accepted regardless of peer UID. Production deployments MUST set \
             --allowed-uid instead."
        );
    }

    // Issue #118: limits are validated before any I/O so a zeroed-out
    // flag aborts startup instead of running a daemon that rejects
    // everything.
    let limits = agenticd::limits::LimitsConfig {
        max_connections: args.max_connections,
        max_connections_per_uid: args.max_connections_per_uid,
        rate_per_uid: args.rate_per_uid,
        commit_queue_depth: args.commit_queue_depth,
        read_idle: std::time::Duration::from_secs(args.read_idle_secs),
        write_idle: std::time::Duration::from_secs(args.write_idle_secs),
    };
    limits.validate().context("validating socket limit flags")?;
    tracing::info!(
        target: "agenticd::limits",
        max_connections = limits.max_connections,
        max_connections_per_uid = limits.max_connections_per_uid,
        rate_per_uid = limits.rate_per_uid,
        commit_queue_depth = limits.commit_queue_depth,
        read_idle_secs = limits.read_idle.as_secs(),
        write_idle_secs = limits.write_idle.as_secs(),
        "socket limits in force"
    );

    let agentic_dir = args.repo.join(".agentic");

    let tables = parse_tracked_tables(&args.tables)?;
    let mcp_servers = parse_mcp_spec(&args.mcp)?;

    // ADR-0013: load the scanner allowlist at startup. The allowlist is
    // NOT reloaded dynamically; operators bounce the daemon to pick up
    // new exceptions. A missing file degrades to an empty allowlist so
    // the common case (no exceptions on file) just works.
    let allowlist_path = args
        .scanner_allowlist
        .clone()
        .unwrap_or_else(|| agentic_dir.join("scanner-allowlist.toml"));
    let allowlist = agentic_core::scanner::Allowlist::load_from_path(&allowlist_path)
        .with_context(|| {
            format!(
                "loading scanner allowlist from {}",
                allowlist_path.display()
            )
        })?;
    tracing::info!(
        target: "agenticd::scanner",
        allowlist_entries = allowlist.len(),
        path = %allowlist_path.display(),
        "scanner allowlist loaded"
    );

    let store = ObjectStoreSpec::parse(&args.object_store, &agentic_dir)
        .context("parsing --object-store")?
        .open(allowlist)
        .context("opening object store")?;
    tracing::info!(spec = %args.object_store, "object store ready");

    // Resolve the socket path AND remove any stale socket file before
    // any path that can return Err — including the reconciler below.
    // Without this, an early-exit (reconciler failure, DaemonState::open
    // failure, etc.) would leave a stale socket lying around and clients
    // / health checks would see ECONNREFUSED on connect instead of the
    // honest ENOENT. (Copilot review on PR #50, third pass.)
    let socket_path = args
        .socket
        .clone()
        .unwrap_or_else(|| agentic_dir.join("agenticd.sock"));
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket parent directory {}", parent.display()))?;
    }
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("removing stale socket {}", socket_path.display()))?;
    }

    // Startup ref reconciliation (audit §A2 / R2). Runs before binding the
    // socket so a corrupted repo never gets traffic — operator sees the
    // error and intervenes.
    {
        let refs = Refs::open(&agentic_dir).context("opening refs at startup")?;
        reconcile_refs_on_startup(&refs, store.as_ref())
            .await
            .context("startup ref reconciliation")?;
    }

    // ADR-0014: load the approval key (or fail-closed None) BEFORE binding
    // the socket, so a malformed key aborts startup before accepting work.
    let approval_key = load_approval_key(args.approval_key_file.as_deref())?;

    let peer_auth = Arc::new(peer_auth);
    let state = Arc::new(
        DaemonState::open(
            args.repo.clone(),
            agentic_dir.clone(),
            store,
            args.postgres.as_deref(),
            tables,
            mcp_servers,
            Arc::clone(&peer_auth),
        )
        .await?
        .with_approval_key(approval_key)
        .with_limits(limits.clone()),
    );

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
    let conn_gate = agenticd::limits::ConnGate::new(&limits);
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
                                tracing::warn!(target: "agenticd::accept", error = %e, "accept failed");
                                continue;
                            }
                        };
                        // ADR-0012: read peer credentials before any I/O.
                        let cred = match sock.peer_cred() {
                            Ok(c) => c,
                            Err(e) => {
                                tracing::warn!(target: "agenticd::accept", error = %e, "peer_cred() failed; closing connection");
                                continue;
                            }
                        };
                        let peer_uid: u32 = cred.uid();
                        let peer_pid: Option<i32> = cred.pid();

                        if !state.peer_auth.is_allowed(peer_uid) {
                            tracing::warn!(
                                target: "agenticd::accept",
                                peer_uid,
                                peer_pid = ?peer_pid,
                                "connection rejected: UID not in allowlist"
                            );
                            drop(sock);
                            continue;
                        }

                        // Issue #118: connection caps, keyed on the
                        // observed UID. No frame has been read, so a
                        // structured reply is impossible — log-and-close,
                        // same shape as the allowlist rejection above.
                        let conn_guard = match conn_gate.try_admit(peer_uid) {
                            Ok(g) => g,
                            Err(rej) => {
                                use agenticd::limits::ConnRejection;
                                match rej {
                                    ConnRejection::GlobalCap { current, cap } => {
                                        tracing::warn!(
                                            target: "agenticd::limits",
                                            peer_uid,
                                            peer_pid = ?peer_pid,
                                            current,
                                            cap,
                                            "connection rejected: global connection cap"
                                        );
                                    }
                                    ConnRejection::PerUidCap { uid, current, cap } => {
                                        tracing::warn!(
                                            target: "agenticd::limits",
                                            peer_uid = uid,
                                            peer_pid = ?peer_pid,
                                            current,
                                            cap,
                                            "connection rejected: per-UID connection cap"
                                        );
                                    }
                                }
                                drop(sock);
                                continue;
                            }
                        };
                        tracing::debug!(
                            target: "agenticd::accept",
                            peer_uid,
                            peer_pid = ?peer_pid,
                            "connection accepted"
                        );

                        // Under --insecure-allow-any-uid we deliberately do
                        // NOT attest commits with the connection's UID; the
                        // UID has no security meaning in that mode.
                        // Centralised on PeerAuthPolicy::attestation_for so
                        // the "insecure mode suppresses attestation"
                        // invariant lives in one place.
                        let carried_uid = state.peer_auth.attestation_for(peer_uid);

                        let state = state.clone();
                        tokio::task::spawn_local(async move {
                            let _conn_guard = conn_guard;
                            if let Err(e) =
                                handle_connection(state, sock, peer_uid, carried_uid).await
                            {
                                tracing::warn!(
                                    target: "agenticd::accept",
                                    error = %format!("{e:#}"),
                                    peer_uid = ?carried_uid,
                                    "connection error",
                                );
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
