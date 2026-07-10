//! agentic-proto
//!
//! Wire types for the `agenticd` daemon. The CLI (`agentic`) and the
//! Python SDK both speak this protocol over a Unix domain socket.
//!
//! MVP transport: length-prefixed JSON. We will migrate to protobuf in
//! v1.1 when stability matters more than iteration speed.
//!
//! ## Protocol versioning
//!
//! Per [ADR-0010](../../docs/adr/0010-wire-protocol-error-model.md) the
//! `Envelope` carries an explicit `protocol_version: u16`. The current
//! shape is v1. The daemon's v1.0.0 release also accepts v0 envelopes
//! (missing `protocol_version`, prompts as `String` rather than
//! base64-encoded `Vec<u8>`) and translates them into the v1 path
//! internally; v1.1.0 drops v0 support. SDKs and CLIs targeting the v1
//! daemon should always emit `protocol_version = 1` and base64-encoded
//! prompts.

pub mod framing;

use serde::{Deserialize, Serialize};
use serde_with::base64::Base64;
use serde_with::serde_as;

/// Current wire-protocol version. Bumped by ADRs that change the
/// `Envelope`, `Request`, `Response`, or any nested type's wire shape.
pub const PROTOCOL_VERSION: u16 = 1;

fn default_protocol_version() -> u16 {
    // Default to 0 (i.e. "not present on wire" → v0) so a missing field
    // routes through the v0 coexistence shim in the daemon. Per
    // ADR-0010 Decision 6, the daemon's v1.0.0 release accepts both.
    0
}

/// Every daemon request carries an opaque correlation id chosen by the
/// caller. Responses echo it back. This lets the SDK demultiplex
/// concurrent in-flight requests over a single socket.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub correlation_id: String,
    /// Wire-protocol version. Daemons set this to [`PROTOCOL_VERSION`] on
    /// every reply. Missing on inbound (deserialised as 0) means v0 — the
    /// daemon's coexistence shim translates such envelopes to v1
    /// internally. Per ADR-0010 Decision 5.
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u16,
    pub payload: T,
}

impl<T> Envelope<T> {
    /// Wrap a payload in a v1 envelope with the given correlation id.
    pub fn new(correlation_id: impl Into<String>, payload: T) -> Self {
        Self {
            correlation_id: correlation_id.into(),
            protocol_version: PROTOCOL_VERSION,
            payload,
        }
    }
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
        /// Operator approval token for `accept_data_loss = true` requests
        /// (ADR-0014). Additive field: older clients omit it and it
        /// deserializes as `None`, which the daemon rejects fail-closed
        /// when `accept_data_loss` is set. Ignored when `accept_data_loss`
        /// is `false`. Wire format `"<unix_ts>:<hex_hmac>"`.
        #[serde(default)]
        approval_token: Option<String>,
    },

    /// Look up a single ref → commit hash.
    ResolveRef { name: String },

    /// Fetch the canonical content bytes of a typed object by its hash.
    /// Currently supported object kinds are blob, tree, and commit.
    /// Unblocks checkpointer time-travel and direct inspection of those objects.
    ReadObject { hash: String },
}

/// Top-level classification for `Response::Error`. Closed enum at the
/// protocol layer per [ADR-0010] Decision 2: adding a class is a
/// wire-protocol change; adding a `code` within a class is additive.
///
/// Clients (especially `AgenticSessionStore` per ADR-0005) discriminate
/// on this enum to decide whether to retry, surface to the user, or
/// fail the calling agent run.
///
/// [ADR-0010]: ../../docs/adr/0010-wire-protocol-error-model.md
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Wire-level: framing, version, malformed envelope, oversize frame.
    Protocol,
    /// Input validation: bad ref name, malformed Commit input, unknown branch
    /// in `--branch`, invalid migration name.
    Validation,
    /// Semantic absence: ref not found, commit hash not found, schema migration
    /// not registered. Not retryable; caller should query a different identity.
    NotFound,
    /// Object store, refs, filesystem: GCS 5xx, disk full, permission denied.
    /// Retryable unless the error chain indicates persistent corruption.
    Storage,
    /// Postgres memory backend: connection failure, advisory-lock timeout,
    /// schema mismatch, partial-migration orphan. The `retryable` field
    /// discriminates per occurrence.
    Memory,
    /// Daemon-internal serialisation: commit_lock contention timeout, snapshot
    /// in progress, another rollback running. Always retryable.
    Concurrency,
    /// Last-resort. Bugs, panics, anything the daemon can't classify. Treat as
    /// non-retryable until a more specific class is added in a future ADR.
    Internal,
    /// Forward-compat catchall. A future ADR may add a class (e.g. `Auth`
    /// when remote `agenticd` lands per ADR-0004 Decision 2's footnote);
    /// older clients deserialise the unknown tag as this variant rather
    /// than failing the whole envelope. Treat as non-retryable until the
    /// client upgrades to the proto version that names the class
    /// concretely.
    ///
    /// **Caveat for proxy authors:** `#[serde(other)]` only affects the
    /// deserialiser. If you receive `class = "auth"` from a future
    /// daemon, decode it through `ErrorClass::Unknown`, and re-serialise
    /// the same value back onto the wire, the output will be
    /// `class = "unknown"` — the original tag is lost. Proxies that
    /// need to preserve unknown tags should keep the original JSON
    /// alongside the typed enum.
    #[serde(other)]
    Unknown,
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
    /// Structured error reply per [ADR-0010] Decision 1. Clients
    /// discriminate on `class` first, then on the free-form `code`
    /// within that class (e.g. `class = NotFound`, `code = "ref_not_found"`).
    /// The `retryable` field is the load-bearing addition: clients use it
    /// to decide back-off-and-retry vs surface-to-user.
    ///
    /// [ADR-0010]: ../../docs/adr/0010-wire-protocol-error-model.md
    Error {
        class: ErrorClass,
        /// Stable string within `class`. SDKs treat as opaque tokens.
        code: String,
        message: String,
        /// True iff retrying the same request later may succeed without
        /// operator intervention.
        retryable: bool,
    },
}

impl Response {
    /// Build a structured `Response::Error`. Use the static-method
    /// constructors below (e.g. `Response::not_found(...)`) when the
    /// class and retryability are pinned at the call site.
    pub fn error(
        class: ErrorClass,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self::Error {
            class,
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }

    pub fn protocol(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::error(ErrorClass::Protocol, code, message, false)
    }

    pub fn validation(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::error(ErrorClass::Validation, code, message, false)
    }

    pub fn not_found(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::error(ErrorClass::NotFound, code, message, false)
    }

    pub fn storage(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self::error(ErrorClass::Storage, code, message, retryable)
    }

    pub fn memory(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self::error(ErrorClass::Memory, code, message, retryable)
    }

    pub fn concurrency(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::error(ErrorClass::Concurrency, code, message, true)
    }

    pub fn internal(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::error(ErrorClass::Internal, code, message, false)
    }
}

#[serde_as]
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
    /// Path → contents for prompt files in this commit. Bytes on the
    /// wire; serialised as base64-in-JSON per [ADR-0010] Decision 3. The
    /// previous wire shape (`BTreeMap<String, String>`) is accepted by the
    /// daemon's v0 coexistence path and translated by `into_bytes()`.
    ///
    /// [ADR-0010]: ../../docs/adr/0010-wire-protocol-error-model.md
    #[serde_as(as = "std::collections::BTreeMap<_, Base64>")]
    pub prompts: std::collections::BTreeMap<String, Vec<u8>>,
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

/// V0-compatible `CommitInput` shape: `prompts` is `BTreeMap<String,
/// String>`, not base64-encoded bytes. Deserialise an inbound envelope
/// into this shape when `Envelope.protocol_version == 0`, then call
/// `.into()` to translate into the v1 [`CommitInput`]. Per ADR-0010
/// Decision 6, the daemon's v1.0.0 release supports both. v1.1.0
/// removes this type and the coexistence shim.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitInputV0 {
    pub message: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub code_sha: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    pub prompts: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub no_memory: bool,
}

impl From<CommitInputV0> for CommitInput {
    fn from(v0: CommitInputV0) -> Self {
        Self {
            message: v0.message,
            author: v0.author,
            code_sha: v0.code_sha,
            branch: v0.branch,
            prompts: v0
                .prompts
                .into_iter()
                .map(|(k, v)| (k, v.into_bytes()))
                .collect(),
            mcp_servers: v0.mcp_servers,
            model: v0.model,
            no_memory: v0.no_memory,
        }
    }
}

/// V0-compatible [`Request`] shape. Mirrors v1 [`Request`] except
/// [`Request::Commit`] carries a [`CommitInputV0`] rather than a
/// [`CommitInput`]. Used by the daemon's coexistence shim — see
/// [`Request::from_v0`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RequestV0 {
    Ping,
    Commit(CommitInputV0),
    Log {
        limit: usize,
    },
    Diff {
        from: String,
        to: String,
    },
    Rollback {
        target: String,
        dry_run: bool,
        accept_data_loss: bool,
    },
    ResolveRef {
        name: String,
    },
    ReadObject {
        hash: String,
    },
}

impl From<RequestV0> for Request {
    fn from(v0: RequestV0) -> Self {
        match v0 {
            RequestV0::Ping => Request::Ping,
            RequestV0::Commit(input) => Request::Commit(input.into()),
            RequestV0::Log { limit } => Request::Log { limit },
            RequestV0::Diff { from, to } => Request::Diff { from, to },
            RequestV0::Rollback {
                target,
                dry_run,
                accept_data_loss,
            } => Request::Rollback {
                target,
                dry_run,
                accept_data_loss,
                // v0 predates the approval gate; a v0 destructive rollback
                // therefore carries no token and is rejected fail-closed.
                approval_token: None,
            },
            RequestV0::ResolveRef { name } => Request::ResolveRef { name },
            RequestV0::ReadObject { hash } => Request::ReadObject { hash },
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_serialises_with_protocol_version() {
        let env = Envelope::new("abc", Request::Ping);
        let s = serde_json::to_string(&env).unwrap();
        assert!(
            s.contains("\"protocol_version\":1"),
            "envelope must carry protocol_version=1; got: {s}"
        );
    }

    #[test]
    fn envelope_missing_protocol_version_deserialises_as_v0() {
        // Wire shape from a pre-ADR-0010 client: no protocol_version field.
        let s = r#"{"correlation_id":"x","payload":{"op":"ping"}}"#;
        let env: Envelope<Request> = serde_json::from_str(s).unwrap();
        assert_eq!(
            env.protocol_version, 0,
            "missing protocol_version must deserialise to 0 so the daemon's \
             coexistence shim routes through the v0 path"
        );
    }

    #[test]
    fn commit_input_prompts_round_trip_as_base64() {
        let mut prompts = std::collections::BTreeMap::new();
        prompts.insert("system.md".to_string(), b"you are helpful".to_vec());
        prompts.insert("binary.bin".to_string(), vec![0x00, 0x01, 0xff, 0xfe]);
        let input = CommitInput {
            message: "hello".to_string(),
            author: Some("tester".to_string()),
            code_sha: None,
            branch: None,
            prompts: prompts.clone(),
            mcp_servers: Vec::new(),
            model: None,
            no_memory: true,
        };
        let s = serde_json::to_string(&input).unwrap();
        // Base64("you are helpful") = "eW91IGFyZSBoZWxwZnVs"
        assert!(
            s.contains("\"system.md\":\"eW91IGFyZSBoZWxwZnVs\""),
            "prompts must round-trip as base64; got: {s}"
        );
        let back: CommitInput = serde_json::from_str(&s).unwrap();
        assert_eq!(back.prompts, prompts);
    }

    #[test]
    fn commit_input_v0_translates_into_v1_prompts() {
        let mut prompts = std::collections::BTreeMap::new();
        prompts.insert("system.md".to_string(), "you are helpful".to_string());
        let v0 = CommitInputV0 {
            message: "hi".to_string(),
            author: None,
            code_sha: None,
            branch: None,
            prompts,
            mcp_servers: Vec::new(),
            model: None,
            no_memory: false,
        };
        let v1: CommitInput = v0.into();
        assert_eq!(
            v1.prompts.get("system.md").map(Vec::as_slice),
            Some(b"you are helpful".as_slice()),
            "v0 String prompts must translate to v1 Vec<u8> via into_bytes()"
        );
    }

    #[test]
    fn error_response_serialises_structured_shape() {
        let r = Response::not_found("ref_not_found", "ref 'feature-x' not found");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"class\":\"not_found\""), "got: {s}");
        assert!(s.contains("\"code\":\"ref_not_found\""), "got: {s}");
        assert!(s.contains("\"retryable\":false"), "got: {s}");
    }

    #[test]
    fn error_response_retryable_classes_have_expected_defaults() {
        // Storage / Memory are per-occurrence (the caller picks); Concurrency
        // is always retryable; NotFound / Validation / Internal / Protocol
        // are never retryable by the helper constructors.
        match Response::concurrency("commit_lock_busy", "another commit in progress") {
            Response::Error { retryable, .. } => assert!(retryable),
            _ => unreachable!(),
        }
        match Response::protocol("version_mismatch", "client too new") {
            Response::Error { retryable, .. } => assert!(!retryable),
            _ => unreachable!(),
        }
    }

    #[test]
    fn v0_client_can_deserialise_v1_error_response() {
        // A pre-ADR-0010 Rust client built against the old proto crate
        // had this Response shape (Error variant carried only `message`).
        // Simulate it locally and confirm a v1 wire-shape Error reply
        // still deserialises successfully (extra fields are ignored by
        // serde's default tagged-enum behaviour). This pins the
        // backward-compat guarantee for the v1.0.0 release window.
        #[derive(serde::Deserialize, Debug)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum V0Response {
            Pong,
            Error { message: String },
        }

        let v1_wire = r#"{
            "kind": "error",
            "class": "not_found",
            "code": "ref_not_found",
            "message": "ref not found: feature-x",
            "retryable": false
        }"#;
        let parsed: V0Response = serde_json::from_str(v1_wire).unwrap();
        match parsed {
            V0Response::Error { message } => {
                assert!(message.contains("ref not found: feature-x"));
            }
            _ => panic!("expected Error variant"),
        }
    }

    #[test]
    fn v0_envelope_deserialises_commit_with_string_prompts() {
        // Exact wire shape a pre-ADR-0010 client would send: no
        // protocol_version, no base64 on prompts. This is what the
        // daemon's coexistence shim must round-trip cleanly.
        let s = r#"{
            "correlation_id": "c1",
            "payload": {
                "op": "commit",
                "message": "hello",
                "prompts": {"system.md": "you are helpful"}
            }
        }"#;
        let env: Envelope<RequestV0> = serde_json::from_str(s).unwrap();
        assert_eq!(env.protocol_version, 0);
        let v1: Request = env.payload.into();
        let Request::Commit(input) = v1 else {
            panic!("expected Commit");
        };
        assert_eq!(
            input.prompts.get("system.md").map(Vec::as_slice),
            Some(b"you are helpful".as_slice())
        );
    }

    #[test]
    fn request_v0_pings_convert_cleanly() {
        let v0 = RequestV0::Ping;
        let v1: Request = v0.into();
        assert!(matches!(v1, Request::Ping));
    }
}
