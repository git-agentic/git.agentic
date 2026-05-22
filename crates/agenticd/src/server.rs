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
use agentic_proto::framing::{read_frame_bytes, write_frame, FrameError};
use agentic_proto::{
    DiffOutput, Envelope, LogEntry, Request, RequestV0, Response, PROTOCOL_VERSION,
};
use anyhow::{anyhow, Context};

use crate::wire_error::map_anyhow_to_response_error;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::mcp::McpServerSpec;
use crate::peer_auth::PeerAuthPolicy;

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
    /// Peer-UID policy applied at socket-accept time. Constructed at
    /// startup from CLI flags; carried here so `DaemonState::open`
    /// callers in integration tests can construct one explicitly.
    pub peer_auth: Arc<PeerAuthPolicy>,
}

impl DaemonState {
    pub async fn open(
        repo_root: PathBuf,
        agentic_dir: PathBuf,
        store: Arc<dyn ObjectStore + Send + Sync>,
        postgres_url: Option<&str>,
        tables: Vec<TrackedTable>,
        mcp_servers: Vec<McpServerSpec>,
        peer_auth: Arc<PeerAuthPolicy>,
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
            // Reject duplicate MCP server names at startup. The commit-time
            // tools tree is keyed by `spec.name`; a duplicate would silently
            // overwrite on `BTreeMap::insert`, producing a tools-tree hash
            // that doesn't match the configured server set. Loud refusal at
            // startup beats silent corruption per commit.
            let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
            for spec in &mcp_servers {
                if !seen.insert(spec.name.as_str()) {
                    return Err(anyhow::anyhow!(
                        "duplicate MCP server name {:?} in --mcp spec; \
                         each configured server must have a unique name \
                         (the tools tree is keyed by it)",
                        spec.name
                    ));
                }
            }
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
            peer_auth,
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
pub async fn handle_connection(
    state: Arc<DaemonState>,
    sock: UnixStream,
    peer_uid: Option<u32>,
) -> anyhow::Result<()> {
    let (read_half, write_half) = sock.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let mut writer = tokio::io::BufWriter::new(write_half);

    loop {
        let bytes = match read_frame_bytes(&mut reader).await {
            Ok(Some(b)) => b,
            Ok(None) => return Ok(()),
            Err(FrameError::TooLarge(n)) => {
                // Unattributable: we don't have a correlation_id, so we
                // can't send a structured Response::Error back. Per
                // ADR-0010 Decision 4, log-and-close is the honest
                // failure mode here.
                tracing::warn!(
                    target: "agenticd::framing",
                    frame_size = n,
                    peer_uid = ?peer_uid,
                    "inbound frame exceeds MAX_FRAME_BYTES; closing connection"
                );
                return Err(anyhow!("inbound frame exceeds MAX_FRAME_BYTES ({n} bytes)"));
            }
            Err(e) => return Err(e.into()),
        };

        // Decode envelope + version-route the payload per ADR-0010
        // Decision 6 coexistence shim. Either branch may fail with a
        // structured Protocol-class error reply that we send before
        // dropping the connection.
        let (correlation_id, request) = match parse_envelope_with_v0_shim(&bytes, peer_uid).await {
            Ok(pair) => pair,
            Err(EnvelopeParseError::Attributable {
                correlation_id,
                response,
            }) => {
                // Log the parse failure here, before the write attempt,
                // so a write_frame failure can't swallow the reason this
                // connection is being closed. The Attributable case still
                // gives the client a structured reply when the socket
                // survives long enough.
                if let Response::Error {
                    class,
                    code,
                    message,
                    ..
                } = &response
                {
                    tracing::warn!(
                        target: "agenticd::framing",
                        peer_uid = ?peer_uid,
                        correlation_id = %correlation_id,
                        class = ?class,
                        code = %code,
                        error = %message,
                        "envelope parse failed; sending attributable reply"
                    );
                }
                let reply = Envelope::new(correlation_id.clone(), response);
                if let Err(e) = write_frame(&mut writer, &reply).await {
                    tracing::warn!(
                        target: "agenticd::framing",
                        peer_uid = ?peer_uid,
                        correlation_id = %correlation_id,
                        write_error = %e,
                        "failed to deliver attributable parse-error reply; closing connection"
                    );
                    return Err(e.into());
                }
                continue;
            }
            Err(EnvelopeParseError::Unattributable(msg)) => {
                tracing::warn!(
                    target: "agenticd::framing",
                    peer_uid = ?peer_uid,
                    error = %msg,
                    "envelope unparseable; closing connection"
                );
                return Err(anyhow!("malformed envelope: {msg}"));
            }
        };

        let response = match dispatch(Arc::clone(&state), request, peer_uid).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "agenticd::dispatch",
                    error = %format!("{e:#}"),
                    peer_uid = ?peer_uid,
                    correlation_id = %correlation_id,
                    "dispatch returned error"
                );
                map_anyhow_to_response_error(e)
            }
        };
        // Hold a clone of the correlation_id so the error log on a
        // write failure can include it. Without this, a successful
        // dispatch followed by a socket reset during write would close
        // the connection with no record of which request the client
        // never got a reply for.
        let correlation_id_for_log = correlation_id.clone();
        let reply = Envelope::new(correlation_id, response);
        if let Err(e) = write_frame(&mut writer, &reply).await {
            tracing::warn!(
                target: "agenticd::dispatch",
                peer_uid = ?peer_uid,
                correlation_id = %correlation_id_for_log,
                write_error = %e,
                "failed to deliver dispatch reply; closing connection"
            );
            return Err(e.into());
        }
    }
}

/// Outcome of [`parse_envelope_with_v0_shim`] when the envelope can't
/// be deserialised. `Attributable` carries a `correlation_id` extracted
/// from the bytes — the daemon can send a structured `Protocol`-class
/// reply. `Unattributable` is for failures so deep we never recovered a
/// correlation_id (truncated JSON, missing top-level fields).
#[derive(Debug)]
enum EnvelopeParseError {
    Attributable {
        correlation_id: String,
        response: Response,
    },
    Unattributable(String),
}

/// Decode an inbound envelope, applying the ADR-0010 Decision 6
/// coexistence shim:
///
/// * If `protocol_version` is absent or 0, deserialise as
///   `Envelope<RequestV0>` and translate the payload via
///   `RequestV0 -> Request` (which calls `into_bytes()` on each v0
///   prompt String).
/// * If `protocol_version` equals [`PROTOCOL_VERSION`], deserialise as
///   `Envelope<Request>` directly.
/// * If `protocol_version` exceeds [`PROTOCOL_VERSION`], return an
///   attributable `Protocol`-class `version_mismatch` reply.
async fn parse_envelope_with_v0_shim(
    bytes: &[u8],
    peer_uid: Option<u32>,
) -> Result<(String, Request), EnvelopeParseError> {
    // First pass: pull correlation_id + protocol_version out of a
    // version-agnostic Value. If that fails, this envelope is too
    // malformed to attribute.
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
        EnvelopeParseError::Unattributable(format!("envelope JSON parse failed: {e}"))
    })?;
    let correlation_id = value
        .get("correlation_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            EnvelopeParseError::Unattributable("envelope missing correlation_id".to_string())
        })?
        .to_string();
    // Decode `protocol_version` explicitly so we can refuse two
    // silent-failure shapes the previous `as_u64().unwrap_or(0) as u16`
    // path enabled: (a) values outside u16 range silently wrapping
    // modulo 65536 (e.g. 65537 → 1), and (b) a JSON string `"1"`
    // silently downgrading to v0 because `as_u64()` returns `None`
    // for non-integer JSON. Both are now attributable Protocol-class
    // replies rather than corrupt deserialisation paths.
    let protocol_version: u16 = match value.get("protocol_version") {
        None => 0, // ADR-0010 Decision 6: no field → v0.
        Some(serde_json::Value::Number(n)) => {
            let n_u64 = n.as_u64().ok_or_else(|| EnvelopeParseError::Attributable {
                correlation_id: correlation_id.clone(),
                response: Response::protocol(
                    "malformed_protocol_version",
                    format!("protocol_version is not a non-negative integer: {n}"),
                ),
            })?;
            u16::try_from(n_u64).map_err(|_| EnvelopeParseError::Attributable {
                correlation_id: correlation_id.clone(),
                response: Response::protocol(
                    "malformed_protocol_version",
                    format!(
                        "protocol_version {n_u64} exceeds the u16 wire range; \
                         max is {}",
                        u16::MAX
                    ),
                ),
            })?
        }
        Some(other) => {
            return Err(EnvelopeParseError::Attributable {
                correlation_id,
                response: Response::protocol(
                    "malformed_protocol_version",
                    format!(
                        "protocol_version must be a JSON integer; got {}",
                        match other {
                            serde_json::Value::String(_) => "string",
                            serde_json::Value::Bool(_) => "boolean",
                            serde_json::Value::Array(_) => "array",
                            serde_json::Value::Object(_) => "object",
                            serde_json::Value::Null => "null",
                            serde_json::Value::Number(_) => unreachable!(),
                        }
                    ),
                ),
            });
        }
    };

    if protocol_version > PROTOCOL_VERSION {
        tracing::info!(
            target: "agenticd::framing",
            peer_uid = ?peer_uid,
            correlation_id = %correlation_id,
            requested_version = protocol_version,
            supported_version = PROTOCOL_VERSION,
            "rejecting envelope with unknown protocol_version"
        );
        return Err(EnvelopeParseError::Attributable {
            correlation_id,
            response: Response::protocol(
                "version_mismatch",
                format!(
                    "client protocol_version {protocol_version} > daemon-supported \
                     {PROTOCOL_VERSION}; upgrade the daemon or downgrade the client"
                ),
            ),
        });
    }

    // Second pass: deserialise the envelope as the typed shape we now
    // know matches the client's wire version.
    if protocol_version == 0 {
        // v0 client: prompts are String, no protocol_version field.
        let env_v0: Envelope<RequestV0> =
            serde_json::from_slice(bytes).map_err(|e| EnvelopeParseError::Attributable {
                correlation_id: correlation_id.clone(),
                response: Response::protocol(
                    "malformed_v0_envelope",
                    format!("v0 envelope failed to deserialise: {e}"),
                ),
            })?;
        tracing::debug!(
            target: "agenticd::framing",
            correlation_id = %correlation_id,
            "translating v0 envelope to v1"
        );
        Ok((correlation_id, env_v0.payload.into()))
    } else {
        // v1 (or future-equivalent) client.
        let env: Envelope<Request> =
            serde_json::from_slice(bytes).map_err(|e| EnvelopeParseError::Attributable {
                correlation_id: correlation_id.clone(),
                response: Response::protocol(
                    "malformed_envelope",
                    format!("v{protocol_version} envelope failed to deserialise: {e}"),
                ),
            })?;
        Ok((correlation_id, env.payload))
    }
}

async fn dispatch(
    state: Arc<DaemonState>,
    request: Request,
    peer_uid: Option<u32>,
) -> anyhow::Result<Response> {
    match request {
        Request::Ping => Ok(Response::Pong),

        Request::ResolveRef { name } => {
            let resolved = state
                .refs
                .resolve(&name)
                .with_context(|| format!("resolving ref {name}"))?;
            match resolved {
                Some(h) => Ok(Response::ResolveRef { hash: h.to_hex() }),
                None => Ok(Response::not_found(
                    "ref_not_found",
                    format!("ref not found: {name}"),
                )),
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
            let out = crate::commit::execute(Arc::clone(&state), input, peer_uid).await?;
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

        Request::Diff { from, to } => {
            // Take a frozen view of HEAD + every branch ref under
            // `commit_lock` so a concurrent commit can't advance one
            // side of the diff between the two ref resolves. Once we
            // have the snapshot the lock can drop — the rest of diff
            // operates on content-addressed object reads, which are
            // immutable by construction.
            let snapshot = {
                let _guard = Arc::clone(&state.commit_lock).lock_owned().await;
                state.refs.snapshot()?
            };
            Ok(Response::Diff(handle_diff(
                state.as_ref(),
                &snapshot,
                &from,
                &to,
            )?))
        }

        Request::ReadObject { hash } => {
            let h: agentic_core::Hash = hash
                .parse()
                .with_context(|| format!("invalid hash: {hash}"))?;
            // Wrap the (potentially slow under GCS) read in spawn_blocking
            // so the LocalSet thread stays free to poll other connections'
            // pings, logs, and diffs. Audit §A5 / B2 / C1 / R3.
            let object = crate::store_async::get(Arc::clone(&state.store), h)
                .await
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
                return Ok(Response::validation(
                    "object_too_large",
                    format!(
                        "object {} is too large to fetch inline ({} bytes > {} byte limit)",
                        h.to_hex(),
                        data.len(),
                        MAX_OBJECT_BYTES,
                    ),
                ));
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
                peer_uid,
            )
            .await?;
            Ok(Response::Rollback(out))
        }
    }
}

fn handle_diff(
    state: &DaemonState,
    snapshot: &agentic_core::refs::RefsSnapshot,
    from: &str,
    to: &str,
) -> anyhow::Result<DiffOutput> {
    let from_hash = snapshot
        .resolve(from)?
        .ok_or_else(|| anyhow!("ref not found: {from}"))?;
    let to_hash = snapshot
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

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0010 Decision 6: an envelope missing `protocol_version` (the
    /// v0 wire shape) deserialises through the coexistence shim and
    /// surfaces as a v1 `Request`. Pre-ADR-0010 SDK clients keep working.
    #[tokio::test]
    async fn parse_envelope_translates_v0_commit_into_v1() {
        let v0 = br#"{
            "correlation_id": "c1",
            "payload": {
                "op": "commit",
                "message": "hi",
                "prompts": {"system.md": "you are helpful"}
            }
        }"#;
        let (cid, req) = parse_envelope_with_v0_shim(v0, None).await.unwrap();
        assert_eq!(cid, "c1");
        let Request::Commit(input) = req else {
            panic!("expected Commit");
        };
        assert_eq!(
            input.prompts.get("system.md").map(Vec::as_slice),
            Some(b"you are helpful".as_slice()),
            "v0 String prompt must translate to v1 Vec<u8> via into_bytes()"
        );
    }

    /// ADR-0010 Decision 5: a higher-than-supported `protocol_version`
    /// is met with an attributable Protocol-class `version_mismatch`
    /// reply, not a dropped connection.
    #[tokio::test]
    async fn parse_envelope_rejects_unknown_future_protocol_version() {
        let future = br#"{
            "correlation_id": "c2",
            "protocol_version": 9999,
            "payload": {"op": "ping"}
        }"#;
        match parse_envelope_with_v0_shim(future, None).await {
            Err(EnvelopeParseError::Attributable {
                correlation_id,
                response: Response::Error { class, code, .. },
            }) => {
                assert_eq!(correlation_id, "c2");
                assert_eq!(class, agentic_proto::ErrorClass::Protocol);
                assert_eq!(code, "version_mismatch");
            }
            other => panic!("expected attributable Protocol error, got {other:?}"),
        }
    }

    /// An envelope without `correlation_id` is unattributable — the
    /// daemon has no envelope to reply to, so it closes the connection.
    #[tokio::test]
    async fn parse_envelope_unattributable_when_correlation_id_missing() {
        let bad = br#"{
            "protocol_version": 1,
            "payload": {"op": "ping"}
        }"#;
        match parse_envelope_with_v0_shim(bad, None).await {
            Err(EnvelopeParseError::Unattributable(_)) => {}
            other => panic!("expected Unattributable, got {other:?}"),
        }
    }

    /// Helper: assert that the given envelope JSON triggers an
    /// attributable `malformed_protocol_version` reply. Used by the
    /// four cases below that exercise the strict decoding path.
    async fn assert_malformed_protocol_version(envelope: &[u8], expected_cid: &str) {
        match parse_envelope_with_v0_shim(envelope, None).await {
            Err(EnvelopeParseError::Attributable {
                correlation_id,
                response: Response::Error { class, code, .. },
            }) => {
                assert_eq!(correlation_id, expected_cid);
                assert_eq!(class, agentic_proto::ErrorClass::Protocol);
                assert_eq!(code, "malformed_protocol_version");
            }
            other => {
                panic!("expected attributable malformed_protocol_version reply, got {other:?}")
            }
        }
    }

    /// ADR-0010 Decision 5 (strict): a `protocol_version` outside the
    /// u16 wire range must surface as `malformed_protocol_version`,
    /// not silently wrap modulo 65536 (65537 → 1 was the silent path
    /// the round-1 review caught).
    #[tokio::test]
    async fn parse_envelope_rejects_oversize_protocol_version() {
        let bytes = br#"{
            "correlation_id": "c-oversize",
            "protocol_version": 65537,
            "payload": {"op": "ping"}
        }"#;
        assert_malformed_protocol_version(bytes, "c-oversize").await;
    }

    /// A JSON *string* `"1"` must not silently downgrade to v0 (which
    /// `as_u64().unwrap_or(0)` would have done). Treated as a
    /// Protocol-class malformed envelope instead.
    #[tokio::test]
    async fn parse_envelope_rejects_string_protocol_version() {
        let bytes = br#"{
            "correlation_id": "c-string",
            "protocol_version": "1",
            "payload": {"op": "ping"}
        }"#;
        assert_malformed_protocol_version(bytes, "c-string").await;
    }

    /// A JSON boolean `protocol_version: true` is invalid.
    #[tokio::test]
    async fn parse_envelope_rejects_boolean_protocol_version() {
        let bytes = br#"{
            "correlation_id": "c-bool",
            "protocol_version": true,
            "payload": {"op": "ping"}
        }"#;
        assert_malformed_protocol_version(bytes, "c-bool").await;
    }

    /// A negative integer is rejected on the `as_u64()` step.
    #[tokio::test]
    async fn parse_envelope_rejects_negative_protocol_version() {
        let bytes = br#"{
            "correlation_id": "c-neg",
            "protocol_version": -1,
            "payload": {"op": "ping"}
        }"#;
        assert_malformed_protocol_version(bytes, "c-neg").await;
    }

    /// A v1 envelope with a base64-encoded prompt deserialises into
    /// raw bytes the same way the proto-crate test asserts at the
    /// wire-type layer. Lower-level integration than the proto crate's
    /// own test; this one exercises the daemon's shim entry point.
    #[tokio::test]
    async fn parse_envelope_v1_decodes_base64_prompts() {
        // base64("hello world") == "aGVsbG8gd29ybGQ="
        let v1 = br#"{
            "correlation_id": "c3",
            "protocol_version": 1,
            "payload": {
                "op": "commit",
                "message": "hi",
                "prompts": {"sys.md": "aGVsbG8gd29ybGQ="}
            }
        }"#;
        let (cid, req) = parse_envelope_with_v0_shim(v1, None).await.unwrap();
        assert_eq!(cid, "c3");
        let Request::Commit(input) = req else {
            panic!("expected Commit");
        };
        assert_eq!(
            input.prompts.get("sys.md").map(Vec::as_slice),
            Some(b"hello world".as_slice())
        );
    }

    /// Issue #45 / audit §A11: when a diff is in flight and a
    /// concurrent commit advances one of the refs, the diff must
    /// resolve both endpoints from the snapshot taken before the
    /// commit — not mix the pre-commit `from` with the post-commit
    /// `to`. Drives the full handle_diff path with a real
    /// DaemonState.
    #[tokio::test]
    async fn handle_diff_uses_snapshot_pinned_before_concurrent_commit() {
        use agentic_core::{FsObjectStore, ObjectStore};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let agentic_dir = dir.path().join(".agentic");
        std::fs::create_dir_all(&agentic_dir).unwrap();
        let store: Arc<dyn ObjectStore + Send + Sync> =
            Arc::new(FsObjectStore::open(agentic_dir.join("objects")).unwrap());
        let state = Arc::new(
            DaemonState::open(
                dir.path().to_path_buf(),
                agentic_dir,
                store,
                None,
                Vec::new(),
                Vec::new(),
                Arc::new(crate::peer_auth::PeerAuthPolicy::InsecureAllowAny),
            )
            .await
            .unwrap(),
        );

        // Two committed states on `main`: first carries prompt v1,
        // second advances to v2.
        let first = make_commit(&state, "main", "first", b"prompt v1".to_vec()).await;
        let second = make_commit(&state, "main", "second", b"prompt v2".to_vec()).await;
        assert_ne!(first, second);

        // Take a snapshot *before* a third commit lands. The snapshot
        // must continue to resolve `main` to the second commit even
        // after the underlying ref advances.
        let snapshot = state.refs.snapshot().unwrap();
        assert_eq!(snapshot.resolve("main").unwrap(), Some(second));

        // Simulate the concurrent commit racing past the snapshot.
        let third = make_commit(&state, "main", "third", b"prompt v3".to_vec()).await;
        assert_ne!(third, second);

        // The live Refs sees `main` advanced to `third`...
        assert_eq!(state.refs.resolve("main").unwrap(), Some(third));
        // ...but the frozen snapshot — and any diff that uses it —
        // still sees `second`. That's the invariant #45 lands.
        assert_eq!(snapshot.resolve("main").unwrap(), Some(second));

        // Run handle_diff through the pinned snapshot and confirm the
        // output references the snapshotted hashes, not the live tip.
        let diff = handle_diff(&state, &snapshot, &first.to_hex(), "main").unwrap();
        assert_eq!(diff.from, first.to_hex());
        assert_eq!(
            diff.to,
            second.to_hex(),
            "to must be the snapshotted tip, not the live one"
        );
    }

    /// Helper: drive a commit on `branch` with a single prompt blob.
    /// Returns the new commit's hash.
    async fn make_commit(
        state: &std::sync::Arc<DaemonState>,
        branch: &str,
        message: &str,
        prompt_bytes: Vec<u8>,
    ) -> agentic_core::Hash {
        use agentic_proto::CommitInput;
        let mut prompts = std::collections::BTreeMap::new();
        prompts.insert("system.md".to_string(), prompt_bytes);
        let input = CommitInput {
            message: message.to_string(),
            author: Some("tester".to_string()),
            code_sha: Some("deadbeef".to_string()),
            branch: Some(branch.to_string()),
            prompts,
            mcp_servers: Vec::new(),
            model: Some("anthropic:claude-opus:2026-05-01".to_string()),
            no_memory: true,
        };
        let out = crate::commit::execute(std::sync::Arc::clone(state), input, None)
            .await
            .unwrap();
        out.commit_hash.parse().unwrap()
    }
}
