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

use agentic_core::commit::{stage_and_commit, walk_log, CommitInputs};
use agentic_core::diff as diff_mod;
use agentic_core::refs::{HeadRef, Refs};
use agentic_core::{Hash, ObjectKind, ObjectStore};
use agentic_memory::postgres::{PgConfig, PostgresAdapter, TrackedTable};
use agentic_memory::MemoryAdapter;
use agentic_proto::framing::{read_frame, write_frame};
use agentic_proto::{CommitInput, CommitOutput, DiffOutput, Envelope, LogEntry, Request, Response};
use anyhow::{anyhow, Context};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::mcp::{fingerprint_all, McpServerSpec};

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
            memory,
            mcp_servers,
            http,
        })
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
            let _guard = Arc::clone(&state.commit_lock).lock_owned().await;
            let out = handle_commit(state.as_ref(), input).await?;
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

        Request::Rollback {
            target,
            dry_run,
            accept_data_loss,
        } => {
            let _guard = Arc::clone(&state.commit_lock).lock_owned().await;
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

async fn handle_commit(state: &DaemonState, input: CommitInput) -> anyhow::Result<CommitOutput> {
    let head = state.refs.read_head()?;
    let branch = input.branch.clone().unwrap_or_else(|| match &head {
        Some(HeadRef::Branch(b)) => b.clone(),
        _ => "main".to_string(),
    });

    if head.is_none() {
        state.refs.write_head_symbolic(&branch)?;
    }

    let parent: Option<Hash> = state.refs.read_branch(&branch)?;

    // ADR-0002 Decision 3, Step 1: stage memory before any other blob.
    // We capture a snapshot, persist its manifest as a raw object, and
    // thread the manifest hash + schema version into the Commit.
    let (memory_snapshot, schema_version) = if input.no_memory {
        (None, None)
    } else if let Some(memory) = state.memory.as_ref().map(Arc::clone) {
        let adapter = memory.lock_owned().await;
        let handle = adapter.snapshot().await.context("taking memory snapshot")?;
        let manifest_bytes = handle.manifest.to_canonical_bytes();
        let manifest_hash = state
            .store
            .put_raw(ObjectKind::Tree, &manifest_bytes)
            .context("persisting segment manifest")?;
        (Some(manifest_hash), Some(handle.schema_version))
    } else {
        (None, None)
    };

    // ADR-0002 Decision 3, Step 1 (continued): stage tools. Fingerprint
    // each configured MCP server in turn, collect the canonical manifest
    // bytes keyed by server name, and let `stage_and_commit` build the
    // tools Tree downstream. A per-server failure is surfaced as an
    // error so the operator sees it; if the call returned partial
    // success and we silently dropped a server, the resulting tools-tree
    // hash would be wrong relative to the supposed commit state.
    let tools = if state.mcp_servers.is_empty() {
        Default::default()
    } else {
        let fingerprints = fingerprint_all(&state.http, &state.mcp_servers).await;
        let mut tools_map = std::collections::BTreeMap::new();
        for (spec, result) in state.mcp_servers.iter().zip(fingerprints) {
            let fp = result.with_context(|| format!("fingerprinting MCP server {}", spec.name))?;
            tools_map.insert(fp.name, fp.canonical_manifest);
        }
        tools_map
    };

    let prompts = input
        .prompts
        .into_iter()
        .map(|(name, body)| (name, body.into_bytes()))
        .collect();

    let inputs = CommitInputs {
        author: input.author.unwrap_or_else(|| "unknown".to_string()),
        message: input.message,
        parent,
        code_sha: input.code_sha,
        prompts,
        tools,
        model: input.model,
        memory_snapshot,
        schema_version,
        intent: None,
        plan: None,
        transcript: None,
        evals: None,
        cost_cents: 0,
    };

    let out = stage_and_commit(state.store.as_ref(), &state.refs, &branch, inputs)?;
    Ok(CommitOutput {
        commit_hash: out.commit_hash.to_hex(),
        branch: out.branch,
    })
}
