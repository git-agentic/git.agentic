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
use agentic_core::refs::{HeadRef, Refs};
use agentic_core::{FsObjectStore, Hash};
use agentic_proto::framing::{read_frame, write_frame};
use agentic_proto::{
    CommitInput, CommitOutput, DiffOutput, Envelope, LogEntry, Request, Response, RollbackOutput,
};
use anyhow::Context;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// Long-lived state shared by every connection handler.
pub struct DaemonState {
    /// Object store rooted at `<repo>/.agentic/objects/`.
    pub store: FsObjectStore,
    /// Ref manager rooted at `<repo>/.agentic/`.
    pub refs: Refs,
    /// Serialises every write-path request. Per ADR-0001 the daemon does
    /// one commit at a time.
    pub commit_lock: Mutex<()>,
}

impl DaemonState {
    pub fn open(agentic_dir: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&agentic_dir).context("creating .agentic directory")?;
        let store =
            FsObjectStore::open(agentic_dir.join("objects")).context("opening object store")?;
        let refs = Refs::open(&agentic_dir).context("opening refs")?;
        Ok(Self {
            store,
            refs,
            commit_lock: Mutex::new(()),
        })
    }
}

/// Handle a single accepted connection. Runs the read/dispatch/write loop
/// until the peer closes the socket.
pub async fn handle_connection(
    state: Arc<DaemonState>,
    mut sock: UnixStream,
) -> anyhow::Result<()> {
    let (read_half, write_half) = sock.split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let mut writer = tokio::io::BufWriter::new(write_half);

    while let Some(envelope) = read_frame::<_, Envelope<Request>>(&mut reader).await? {
        let correlation_id = envelope.correlation_id.clone();
        let response = match dispatch(state.as_ref(), envelope.payload).await {
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

async fn dispatch(state: &DaemonState, request: Request) -> anyhow::Result<Response> {
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
            let _guard = state.commit_lock.lock().await;
            let out = handle_commit(state, input).await?;
            Ok(Response::Commit(out))
        }

        Request::Log { limit } => {
            let head = state.refs.resolve("HEAD")?;
            let entries = match head {
                None => Vec::new(),
                Some(h) => walk_log(&state.store, h, limit)?
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

        Request::Diff { from, to } => Ok(Response::Diff(DiffOutput {
            from,
            to,
            prompts: vec!["(not yet implemented — Chunk C)".into()],
            tools: Vec::new(),
            model_changed: false,
            memory_summary: String::new(),
            schema_summary: String::new(),
        })),

        Request::Rollback {
            target, dry_run: _, ..
        } => Ok(Response::Rollback(RollbackOutput {
            planned_steps: vec![format!("would rollback to {target} (Chunk C)")],
            executed: false,
            new_head_hash: None,
        })),
    }
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
        tools: Default::default(),
        model: input.model,
        memory_snapshot: None,
        schema_version: None,
        intent: None,
        plan: None,
        transcript: None,
        evals: None,
        cost_cents: 0,
    };

    let out = stage_and_commit(&state.store, &state.refs, &branch, inputs)?;
    Ok(CommitOutput {
        commit_hash: out.commit_hash.to_hex(),
        branch: out.branch,
    })
}
