//! The daemon's per-connection request dispatcher.
//!
//! Each connected client speaks one or more length-prefixed JSON envelopes
//! (`agentic_proto::framing`). The server reads requests, calls into the
//! handlers below, and writes responses back on the same socket. The
//! daemon owns one global commit lock (per ADR-0001 §"process model");
//! requests touching the object store acquire that lock for the duration
//! of the call.

use std::path::PathBuf;
use std::sync::Arc;

use agentic_core::commit::walk_log;
use agentic_core::diff as diff_mod;
use agentic_core::refs::Refs;
use agentic_core::{Object, ObjectStore};
use agentic_memory::postgres::{PgConfig, PostgresAdapter, TrackedTable};
use agentic_memory::MemoryAdapter;
use agentic_proto::framing::{read_frame, write_frame};
use agentic_proto::{DiffOutput, Envelope, LogEntry, Request, Response};
use anyhow::{anyhow, Context};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::mcp::McpServerSpec;

/// Long-lived state shared by every connection handler.
pub struct DaemonState {
    /// Filesystem root of the repo (parent of `.agentic/`). Rollback uses
    /// this to compute the `prompts/` write-back target.
    pub repo_root: PathBuf,
    /// Object store backing the repo. Concrete type is chosen at startup
    /// from the `--object-store` flag (see `crate::objstore`); call sites
    /// only see the [`ObjectStore`] trait.
    pub store: Arc<dyn ObjectStore + Send + Sync>,
    /// Ref manager rooted at `<repo>/.agentic/`.
    pub refs: Refs,
    /// Serialises every write-path request. Per ADR-0001 the daemon does
    /// one commit at a time.
    pub commit_lock: Arc<Mutex<()>>,
    /// Shutdown signal shared with [`crate::lifecycle::Lifecycle`]. Set
    /// when SIGTERM/SIGINT fires. Write-path handlers check this BEFORE
    /// and AFTER acquiring `commit_lock` so a commit that's queued at
    /// shutdown bails out instead of starting a 2PC sequence the
    /// LocalSet is about to abort. Audit §A2 / PR #50 follow-up review.
    pub shutdown: tokio_util::sync::CancellationToken,
    /// Optional memory backend. When present, every commit takes a memory
    /// snapshot under the commit lock and threads its manifest hash into
    /// the Commit's `memory_snapshot` dimension.
    pub memory: Option<Arc<Mutex<PostgresAdapter>>>,
    /// MCP servers to fingerprint on each commit.
    pub mcp_servers: Vec<McpServerSpec>,
    /// Shared HTTP client for MCP calls. Reusing one client lets the
    /// connection pool stay warm across many commits.
    pub http: reqwest::Client,
}

impl DaemonState {
    pub async fn open(
        repo_root: PathBuf,
        agentic_dir: PathBuf,
        store: Arc<dyn ObjectStore + Send + Sync>,
        postgres_url: Option<&str>,
        tables: Vec<TrackedTable>,
        mcp_servers: Vec<McpServerSpec>,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&agentic_dir).context("creating .agentic directory")?;
        let refs = Refs::open(&agentic_dir).context("opening refs")?;

        let memory = match postgres_url {
            None => None,
            Some(url) => {
                if tables.is_empty() {
                    return Err(anyhow::anyhow!(
                        "--postgres requires at least one --tables entry"
                    ));
                }
                let cfg = PgConfig::new(url, tables);
                let mut adapter = PostgresAdapter::connect(cfg, store.clone()).await?;
                adapter.init().await?;
                tracing::info!(
                    logical_decoding = adapter.logical_decoding_available(),
                    "memory backend attached"
                );
                Some(Arc::new(Mutex::new(adapter)))
            }
        };

        let http = reqwest::Client::builder()
            .user_agent(concat!("agenticd/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building HTTP client")?;

        if !mcp_servers.is_empty() {
            tracing::info!(count = mcp_servers.len(), "MCP fingerprinting attached");
        }

        Ok(Self {
            repo_root,
            store,
            refs,
            commit_lock: Arc::new(Mutex::new(())),
            shutdown: tokio_util::sync::CancellationToken::new(),
            memory,
            mcp_servers,
            http,
        })
    }

    /// Returns `Err` if shutdown has been signalled. Write-path handlers
    /// call this on entry (before queuing on `commit_lock`) and again
    /// after acquiring the lock — the second check catches the race
    /// where shutdown fired while the handler waited in the lock's queue.
    pub fn check_shutdown(&self) -> anyhow::Result<()> {
        if self.shutdown.is_cancelled() {
            return Err(anyhow::anyhow!(
                "daemon is shutting down; refusing new write-path work"
            ));
        }
        Ok(())
    }
}

/// Handle a single accepted connection. Runs the read/dispatch/write loop
/// until the peer closes the socket.
pub async fn handle_connection(state: Arc<DaemonState>, sock: UnixStream) -> anyhow::Result<()> {
    let (read_half, write_half) = sock.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let mut writer = tokio::io::BufWriter::new(write_half);

    while let Some(envelope) = read_frame::<_, Envelope<Request>>(&mut reader).await? {
        let correlation_id = envelope.correlation_id.clone();
        let response = match dispatch(Arc::clone(&state), envelope.payload).await {
            Ok(r) => r,
            Err(e) => Response::Error {
                message: format!("{e:#}"),
            },
        };
        let reply = Envelope {
            correlation_id,
            payload: response,
        };
        write_frame(&mut writer, &reply).await?;
    }
    Ok(())
}

async fn dispatch(state: Arc<DaemonState>, request: Request) -> anyhow::Result<Response> {
    match request {
        Request::Ping => Ok(Response::Pong),

        Request::ResolveRef { name } => {
            let resolved = state
                .refs
                .resolve(&name)
                .with_context(|| format!("resolving ref {name}"))?;
            match resolved {
                Some(h) => Ok(Response::ResolveRef { hash: h.to_hex() }),
                None => Ok(Response::Error {
                    message: format!("ref not found: {name}"),
                }),
            }
        }

        Request::Commit(input) => {
            // Early bail-out: skip the queue entirely if shutdown already
            // fired. Otherwise this request would queue on commit_lock and
            // then race with `Lifecycle::drain` for the in-flight 2PC.
            state.check_shutdown()?;
            let _guard = Arc::clone(&state.commit_lock).lock_owned().await;
            // Re-check after acquire: shutdown may have fired while we
            // waited in the queue. Without this, drain releases the lock
            // and the next queued waiter wakes up and starts 2PC inside a
            // LocalSet that is about to abort it.
            state.check_shutdown()?;
            let out = crate::commit::execute(Arc::clone(&state), input).await?;
            Ok(Response::Commit(out))
        }

        Request::Log { limit } => {
            let head = state.refs.resolve("HEAD")?;
            let entries = match head {
                None => Vec::new(),
                Some(h) => walk_log(state.store.as_ref(), h, limit)?
                    .into_iter()
                    .map(|(hash, c)| LogEntry {
                        hash: hash.to_hex(),
                        message: c.message,
                        author: c.author,
                        timestamp: c.timestamp.to_rfc3339(),
                    })
                    .collect(),
            };
            Ok(Response::Log { entries })
        }

        Request::Diff { from, to } => Ok(Response::Diff(handle_diff(state.as_ref(), &from, &to)?)),

        Request::ReadObject { hash } => {
            let h: agentic_core::Hash = hash
                .parse()
                .with_context(|| format!("invalid hash: {hash}"))?;
            let object = state
                .store
                .get(&h)
                .with_context(|| format!("reading object {hash}"))?;
            // Extract canonical bytes — the same bytes the hash commits to —
            // so callers can verify Hash::of(&data) == hash.
            let (object_kind, data) = match object {
                Object::Blob(b) => ("blob", b.bytes),
                Object::Tree(t) => ("tree", serde_json::to_vec(&t).context("serializing tree")?),
                Object::Commit(c) => (
                    "commit",
                    serde_json::to_vec(&c).context("serializing commit")?,
                ),
            };
            // Base64 expands by ~33%; guard against blowing the 16 MiB frame
            // limit so the client receives a structured error rather than a
            // dropped connection.
            const MAX_OBJECT_BYTES: usize = 10 * 1024 * 1024;
            if data.len() > MAX_OBJECT_BYTES {
                return Ok(Response::Error {
                    message: format!(
                        "object {} is too large to fetch inline ({} bytes > {} byte limit)",
                        h.to_hex(),
                        data.len(),
                        MAX_OBJECT_BYTES,
                    ),
                });
            }
            Ok(Response::ObjectData {
                hash: h.to_hex(),
                object_kind: object_kind.to_string(),
                data,
            })
        }

        Request::Rollback {
            target,
            dry_run,
            accept_data_loss,
        } => {
            // Same shutdown discipline as Commit — bail out before queuing
            // and re-check after acquiring the lock.
            state.check_shutdown()?;
            let _guard = Arc::clone(&state.commit_lock).lock_owned().await;
            state.check_shutdown()?;
            let repo_root = state.repo_root.clone();
            let out = crate::rollback::execute(
                Arc::clone(&state),
                crate::rollback::RollbackArgs {
                    target,
                    dry_run,
                    accept_data_loss,
                    repo: repo_root,
                },
            )
            .await?;
            Ok(Response::Rollback(out))
        }
    }
}

fn handle_diff(state: &DaemonState, from: &str, to: &str) -> anyhow::Result<DiffOutput> {
    let from_hash = state
        .refs
        .resolve(from)?
        .ok_or_else(|| anyhow!("ref not found: {from}"))?;
    let to_hash = state
        .refs
        .resolve(to)?
        .ok_or_else(|| anyhow!("ref not found: {to}"))?;
    let d = diff_mod::diff(state.store.as_ref(), from_hash, to_hash)?;

    let render_tree = |td: &Option<diff_mod::TreeDiff>| -> Vec<String> {
        let Some(td) = td else { return Vec::new() };
        let mut lines = Vec::new();
        for e in &td.added {
            lines.push(format!("+ {}", e.name));
        }
        for e in &td.removed {
            lines.push(format!("- {}", e.name));
        }
        for m in &td.modified {
            lines.push(format!("~ {}", m.name));
        }
        lines
    };

    let memory_summary = match d.memory_snapshot {
        None => String::new(),
        Some(ref ch) => match (ch.from, ch.to) {
            (None, Some(t)) => format!("memory snapshot added → {}", t.short()),
            (Some(f), None) => format!("memory snapshot removed (was {})", f.short()),
            (Some(f), Some(t)) => format!("memory snapshot {} → {}", f.short(), t.short()),
            (None, None) => String::new(),
        },
    };
    let schema_summary = match d.schema_version {
        None => String::new(),
        Some(ref sc) => format!(
            "schema_version {} → {}",
            sc.from.as_deref().unwrap_or("(none)"),
            sc.to.as_deref().unwrap_or("(none)")
        ),
    };

    Ok(DiffOutput {
        from: from_hash.to_hex(),
        to: to_hash.to_hex(),
        prompts: render_tree(&d.prompts),
        tools: render_tree(&d.tools),
        model_changed: d.model.is_some(),
        memory_summary,
        schema_summary,
    })
}

// `handle_commit` has been extracted into `crate::commit::execute` —
// the dispatch arm above calls it directly. See `commit.rs` for the
// phased orchestration (snapshot_memory → fingerprint_tools →
// assemble_inputs → stage_and_commit_with_now → publish_head).
// Audit §A3 / §S2.
