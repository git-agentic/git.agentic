//! agentic-proto
//!
//! Wire types for the `agenticd` daemon. The CLI (`agentic`) and the
//! Python SDK both speak this protocol over a Unix domain socket.
//!
//! MVP transport: length-prefixed JSON. We will migrate to protobuf in
//! v1.1 when stability matters more than iteration speed.

pub mod framing;

use serde::{Deserialize, Serialize};
use serde_with::base64::Base64;
use serde_with::serde_as;

/// Every daemon request carries an opaque correlation id chosen by the
/// caller. Responses echo it back. This lets the SDK demultiplex
/// concurrent in-flight requests over a single socket.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub correlation_id: String,
    pub payload: T,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Liveness probe.
    Ping,

    /// Create a new commit. Body carries the tuple inputs.
    Commit(CommitInput),

    /// List recent commits.
    Log { limit: usize },

    /// Compute a diff between two refs.
    Diff { from: String, to: String },

    /// Roll back to a target ref.
    Rollback {
        target: String,
        dry_run: bool,
        accept_data_loss: bool,
    },

    /// Look up a single ref → commit hash.
    ResolveRef { name: String },

    /// Fetch the canonical content bytes of a typed object by its hash.
    /// Currently supported object kinds are blob, tree, and commit.
    /// Unblocks checkpointer time-travel and direct inspection of those objects.
    ReadObject { hash: String },
}

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Commit(CommitOutput),
    Log {
        entries: Vec<LogEntry>,
    },
    Diff(DiffOutput),
    Rollback(RollbackOutput),
    ResolveRef {
        hash: String,
    },
    ObjectData {
        hash: String,
        object_kind: String,
        #[serde_as(as = "Base64")]
        data: Vec<u8>,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitInput {
    pub message: String,
    /// Author identifier (e.g. unix user + hostname). Optional on the wire;
    /// daemon falls back to `"unknown"` when absent.
    #[serde(default)]
    pub author: Option<String>,
    /// Git SHA of the code tree at commit time.
    #[serde(default)]
    pub code_sha: Option<String>,
    /// Branch to commit on. Defaults to the daemon's current `HEAD` target.
    #[serde(default)]
    pub branch: Option<String>,
    /// Path → contents for prompt files in this commit.
    pub prompts: std::collections::BTreeMap<String, String>,
    /// MCP server URLs to fingerprint at commit time.
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    /// Model version string (e.g. "anthropic:claude-opus:2026-05-01").
    #[serde(default)]
    pub model: Option<String>,
    /// If true, do not include a memory snapshot in this commit.
    #[serde(default)]
    pub no_memory: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitOutput {
    pub commit_hash: String,
    pub branch: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub hash: String,
    pub message: String,
    pub author: String,
    pub timestamp: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffOutput {
    pub from: String,
    pub to: String,
    pub prompts: Vec<String>,
    pub tools: Vec<String>,
    pub model_changed: bool,
    pub memory_summary: String,
    pub schema_summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RollbackOutput {
    pub planned_steps: Vec<String>,
    pub executed: bool,
    pub new_head_hash: Option<String>,
}
