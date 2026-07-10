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
use agentic_memory::postgres::TrackedTable;
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
use crate::membackend::MemoryBackendSpec;
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
    /// the Commit's `memory_snapshot` dimension. No adapter-level mutex:
    /// write-path exclusivity comes from `commit_lock` (one commit at a
    /// time, ADR-0001), and adapters are internally `&self`-safe
    /// (audit §C9 / §A9).
    pub memory: Option<Arc<dyn MemoryAdapter>>,
    /// MCP servers to fingerprint on each commit.
    pub mcp_servers: Vec<McpServerSpec>,
    /// Shared HTTP client for MCP calls. Reusing one client lets the
    /// connection pool stay warm across many commits.
    pub http: reqwest::Client,
    /// Peer-UID policy applied at socket-accept time. Constructed at
    /// startup from CLI flags; carried here so `DaemonState::open`
    /// callers in integration tests can construct one explicitly.
    pub peer_auth: Arc<PeerAuthPolicy>,
    /// Operator approval key for destructive-rollback tokens (ADR-0014).
    /// `None` — the default — means no key is configured, so every
    /// `accept_data_loss = true` request is rejected fail-closed
    /// (Decision 4). Set at startup from `--approval-key-file` via
    /// [`DaemonState::with_approval_key`].
    pub approval_key: Option<agentic_core::approval::ApprovalKey>,
    /// Static limits in force (issue #118). Set at startup via
    /// [`DaemonState::with_limits`]; defaults are spec values.
    pub limits: crate::limits::LimitsConfig,
    /// Per-UID request rate budget, keyed on the observed peer UID.
    pub rate: crate::limits::RateLimiter,
    /// Bound on requests queued-or-executing on `commit_lock`. A
    /// dispatch arm that would take the lock first takes a slot here;
    /// `try_acquire` failure is an immediate structured rejection
    /// instead of a silent unbounded queue.
    pub commit_slots: Arc<tokio::sync::Semaphore>,
    /// Observable commit-queue depth (queued + executing). Mirrors the
    /// semaphore purely for logging — the semaphore enforces, this
    /// reports.
    pub commit_queue_depth: Arc<std::sync::atomic::AtomicUsize>,
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

        let memory = MemoryBackendSpec::from_flags(postgres_url, tables)?
            .open(store.clone())
            .await?;

        let http = reqwest::Client::builder()
            .user_agent(concat!("agenticd/", env!("CARGO_PKG_VERSION")))
            // ADR-0016: never follow redirects. This client fingerprints
            // operator-configured MCP servers; a redirect from one of them
            // could bounce the request to an unconfigured (internal) host —
            // e.g. the Cloud Run metadata server — whose response would then
            // be committed. Disabling redirects makes the reachable-host set
            // exactly the configured `--mcp` list. INVARIANT: this client is
            // shared (`DaemonState.http`); any future non-MCP use that
            // legitimately needs redirects MUST build its own client rather
            // than relaxing this policy.
            .redirect(reqwest::redirect::Policy::none())
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
            approval_key: None,
            limits: crate::limits::LimitsConfig::default(),
            rate: crate::limits::RateLimiter::new(
                crate::limits::LimitsConfig::default().rate_per_uid,
            ),
            commit_slots: Arc::new(tokio::sync::Semaphore::new(
                crate::limits::LimitsConfig::default().commit_queue_depth,
            )),
            commit_queue_depth: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        })
    }

    /// Attach an operator approval key (ADR-0014). Builder-style so the
    /// startup path can chain it after `open`; tests and the no-key
    /// deployment leave it `None`.
    pub fn with_approval_key(mut self, key: Option<agentic_core::approval::ApprovalKey>) -> Self {
        self.approval_key = key;
        self
    }

    /// Attach the limits configuration (issue #118). Builder-style like
    /// `with_approval_key`; rebuilds the rate limiter and the commit
    /// queue semaphore to match. Call before serving traffic.
    pub fn with_limits(mut self, cfg: crate::limits::LimitsConfig) -> Self {
        self.rate = crate::limits::RateLimiter::new(cfg.rate_per_uid);
        self.commit_slots = Arc::new(tokio::sync::Semaphore::new(cfg.commit_queue_depth));
        self.commit_queue_depth = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        self.limits = cfg;
        self
    }

    /// Take a commit-queue slot, or say the queue is full. Callers turn
    /// `None` into a `Response::concurrency("commit_queue_full", ..)`
    /// reply. The slot is held for the queued + lock-held duration, so
    /// the bound covers everything that can queue on `commit_lock`.
    pub fn try_commit_slot(&self, peer_uid: Option<u32>) -> Option<CommitSlot> {
        let permit = Arc::clone(&self.commit_slots).try_acquire_owned().ok()?;
        let depth = self
            .commit_queue_depth
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        tracing::debug!(
            target: "agenticd::limits",
            depth,
            peer_uid = ?peer_uid,
            "commit queue slot acquired"
        );
        Some(CommitSlot {
            _permit: permit,
            depth: Arc::clone(&self.commit_queue_depth),
            peer_uid,
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

/// RAII commit-queue slot (issue #118). Dropping it releases the
/// semaphore permit and decrements the observable depth gauge.
pub struct CommitSlot {
    _permit: tokio::sync::OwnedSemaphorePermit,
    depth: Arc<std::sync::atomic::AtomicUsize>,
    peer_uid: Option<u32>,
}

impl Drop for CommitSlot {
    fn drop(&mut self) {
        let depth = self
            .depth
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_sub(1);
        tracing::debug!(
            target: "agenticd::limits",
            depth,
            peer_uid = ?self.peer_uid,
            "commit queue slot released"
        );
    }
}

/// Write one reply frame under the write-idle deadline (issue #118). A
/// peer that stops reading fills the socket buffer; without a deadline
/// the pended `write_all` would pin this task forever. On elapse we
/// log-and-close — mid-write there is no way to send anything else.
async fn write_reply<W>(
    writer: &mut W,
    reply: &Envelope<Response>,
    write_idle: std::time::Duration,
    peer_uid: Option<u32>,
    // Raw SO_PEERCRED UID (issue #118 final review). Under
    // --insecure-allow-any-uid `peer_uid` logs as `None`, so this is what
    // makes the write-idle-close event attributable to a sender.
    observed_uid: u32,
    correlation_id: &str,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    match tokio::time::timeout(write_idle, write_frame(writer, reply)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.into()),
        Err(_elapsed) => {
            tracing::warn!(
                target: "agenticd::limits",
                peer_uid = ?peer_uid,
                observed_uid,
                correlation_id = %correlation_id,
                write_idle_secs = write_idle.as_secs(),
                "response write stalled beyond write-idle deadline; closing"
            );
            Err(anyhow!(
                "response write stalled beyond {}s write-idle deadline",
                write_idle.as_secs()
            ))
        }
    }
}

/// Handle a single accepted connection. Runs the read/dispatch/write
/// loop until the peer closes, misses the read-idle deadline, stalls a
/// response write past the write-idle deadline, or exhausts budgets in
/// a way that closes the connection.
///
/// `observed_uid` is the raw SO_PEERCRED UID — the key for limits
/// accounting in BOTH auth modes. `peer_uid` is the ADR-0012
/// attestation identity (None under --insecure-allow-any-uid) and is
/// what dispatch stamps into commits.
pub async fn handle_connection(
    state: Arc<DaemonState>,
    sock: UnixStream,
    observed_uid: u32,
    peer_uid: Option<u32>,
) -> anyhow::Result<()> {
    let read_idle = state.limits.read_idle;
    let write_idle = state.limits.write_idle;
    handle_connection_with_deadlines(state, sock, observed_uid, peer_uid, read_idle, write_idle)
        .await
}

/// [`handle_connection`] with both I/O deadlines injected, so tests can
/// exercise them without waiting the production windows.
pub async fn handle_connection_with_deadlines(
    state: Arc<DaemonState>,
    sock: UnixStream,
    observed_uid: u32,
    peer_uid: Option<u32>,
    read_idle: std::time::Duration,
    write_idle: std::time::Duration,
) -> anyhow::Result<()> {
    let (read_half, write_half) = sock.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let mut writer = tokio::io::BufWriter::new(write_half);

    loop {
        // Bound the wait for the next complete frame. On elapse we can't
        // send a structured reply (no correlation_id mid-frame), so we
        // log-and-close — the same honest failure mode as the oversize
        // arm below (ADR-0010 Decision 4).
        let read = match tokio::time::timeout(read_idle, read_frame_bytes(&mut reader)).await {
            Ok(inner) => inner,
            Err(_elapsed) => {
                tracing::warn!(
                    target: "agenticd::framing",
                    peer_uid = ?peer_uid,
                    idle_timeout_secs = read_idle.as_secs(),
                    "connection idle beyond read deadline; closing"
                );
                return Err(anyhow!(
                    "connection idle beyond {}s read deadline; closing",
                    read_idle.as_secs()
                ));
            }
        };
        let bytes = match read {
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
                // Issue #118 final review (2026-07-10): an attributable
                // parse-error reply costs the daemon the same read +
                // dispatch-adjacent work as a real request, so malformed
                // envelopes must debit the same per-UID rate budget —
                // otherwise a peer can spam garbage bytes for free and
                // never be rate-limited. Debit before replying; if the
                // budget is already exhausted, close instead of replying
                // (the peer is over budget, so closing is the honest
                // failure mode, same shape as the oversize-frame arm
                // above).
                if !state
                    .rate
                    .try_consume(observed_uid, std::time::Instant::now())
                {
                    tracing::warn!(
                        target: "agenticd::limits",
                        peer_uid = ?peer_uid,
                        observed_uid,
                        correlation_id = %correlation_id,
                        "rate budget exhausted by malformed envelopes; closing"
                    );
                    return Err(anyhow!(
                        "rate budget exhausted by malformed envelopes; closing"
                    ));
                }
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
                if let Err(e) = write_reply(
                    &mut writer,
                    &reply,
                    write_idle,
                    peer_uid,
                    observed_uid,
                    &correlation_id,
                )
                .await
                {
                    tracing::warn!(
                        target: "agenticd::framing",
                        peer_uid = ?peer_uid,
                        correlation_id = %correlation_id,
                        write_error = %e,
                        "failed to deliver attributable parse-error reply; closing connection"
                    );
                    return Err(e);
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

        // Issue #118: per-UID request rate budget, keyed on the
        // observed UID. Checked after envelope parse so the rejection
        // carries the correlation_id. The connection survives — a
        // rate-limited client backs off and retries on the same socket.
        if !state
            .rate
            .try_consume(observed_uid, std::time::Instant::now())
        {
            tracing::warn!(
                target: "agenticd::limits",
                peer_uid = ?peer_uid,
                observed_uid,
                correlation_id = %correlation_id,
                "per-UID rate budget exhausted; rejecting request"
            );
            let reply = Envelope::new(
                correlation_id.clone(),
                Response::concurrency(
                    "rate_budget_exhausted",
                    "per-UID request rate budget exhausted; retry shortly",
                ),
            );
            write_reply(
                &mut writer,
                &reply,
                write_idle,
                peer_uid,
                observed_uid,
                &correlation_id,
            )
            .await?;
            continue;
        }

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
        if let Err(e) = write_reply(
            &mut writer,
            &reply,
            write_idle,
            peer_uid,
            observed_uid,
            &correlation_id_for_log,
        )
        .await
        {
            tracing::warn!(
                target: "agenticd::dispatch",
                peer_uid = ?peer_uid,
                correlation_id = %correlation_id_for_log,
                write_error = %e,
                "failed to deliver dispatch reply; closing connection"
            );
            return Err(e);
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

pub(crate) async fn dispatch(
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
            let Some(_slot) = state.try_commit_slot(peer_uid) else {
                tracing::warn!(
                    target: "agenticd::limits",
                    peer_uid = ?peer_uid,
                    depth = state
                        .commit_queue_depth
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "commit queue full; rejecting Commit"
                );
                return Ok(Response::concurrency(
                    "commit_queue_full",
                    "commit queue is full; retry shortly",
                ));
            };
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
            // `commit_lock` so a concurrent ref-writing operation —
            // commit or rollback — can't advance one side of the diff
            // between the two ref resolves. Once we have the snapshot
            // the lock can drop; the rest of diff operates on
            // content-addressed object reads, which are immutable by
            // construction.
            let Some(_slot) = state.try_commit_slot(peer_uid) else {
                tracing::warn!(
                    target: "agenticd::limits",
                    peer_uid = ?peer_uid,
                    depth = state
                        .commit_queue_depth
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "commit queue full; rejecting Diff"
                );
                return Ok(Response::concurrency(
                    "commit_queue_full",
                    "commit queue is full; retry shortly",
                ));
            };
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
            approval_token,
        } => {
            // Same shutdown discipline as Commit — bail out before queuing
            // and re-check after acquiring the lock.
            state.check_shutdown()?;
            let Some(_slot) = state.try_commit_slot(peer_uid) else {
                tracing::warn!(
                    target: "agenticd::limits",
                    peer_uid = ?peer_uid,
                    depth = state
                        .commit_queue_depth
                        .load(std::sync::atomic::Ordering::Relaxed),
                    "commit queue full; rejecting Rollback"
                );
                return Ok(Response::concurrency(
                    "commit_queue_full",
                    "commit queue is full; retry shortly",
                ));
            };
            let _guard = Arc::clone(&state.commit_lock).lock_owned().await;
            state.check_shutdown()?;
            let repo_root = state.repo_root.clone();
            let out = crate::rollback::execute(
                Arc::clone(&state),
                crate::rollback::RollbackArgs {
                    target,
                    dry_run,
                    accept_data_loss,
                    approval_token,
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
    snapshot: &agentic_core::RefsSnapshot,
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

    /// Issue #45: prove the Diff dispatch arm acquires `commit_lock`
    /// before reading refs. Deleting the lock acquisition would let
    /// `dispatch(Diff)` proceed past a held lock; this test would
    /// fail because the inner task completes immediately instead of
    /// blocking on the lock release.
    ///
    /// Mechanic: hold `commit_lock` from the test, drive
    /// `dispatch(Diff)` as a pinned future on the same runtime, and
    /// race it against a short sleep using `tokio::select!`. With the
    /// lock acquisition in the Diff arm in place, the future parks on
    /// `lock_owned().await` and the sleep wins. Without it (if
    /// someone deletes the dispatch's lock block) the future completes
    /// here and the test panics. After releasing the lock we await
    /// the same pinned future for the actual response.
    #[tokio::test]
    async fn dispatch_diff_blocks_on_commit_lock() {
        use agentic_core::{FsObjectStore, ObjectStore};
        use std::sync::Arc;
        use std::time::Duration;

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

        // Two commits so there's actually something for diff to
        // resolve. Without this, dispatch(Diff) would fail at the
        // `ref not found` step BEFORE proving the lock interaction.
        let first = make_commit(&state, "main", "first", b"v1".to_vec()).await;
        let second = make_commit(&state, "main", "second", b"v2".to_vec()).await;
        assert_ne!(first, second);

        // Grab the lock from the test side, holding it.
        let guard = Arc::clone(&state.commit_lock).lock_owned().await;

        let dispatch_fut = dispatch(
            Arc::clone(&state),
            Request::Diff {
                from: first.to_hex(),
                to: "main".to_string(),
            },
            None,
        );
        tokio::pin!(dispatch_fut);

        tokio::select! {
            res = &mut dispatch_fut => {
                panic!(
                    "dispatch(Diff) completed while commit_lock was held; \
                     the lock acquisition in the Diff arm is missing or \
                     was bypassed. got: {res:?}"
                );
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                // expected: dispatch is parked on commit_lock.
            }
        }

        // Release the lock — dispatch now proceeds.
        drop(guard);
        let response = tokio::time::timeout(Duration::from_secs(2), &mut dispatch_fut)
            .await
            .expect("dispatch must complete promptly once lock is dropped")
            .expect("dispatch should succeed once unblocked");

        match response {
            Response::Diff(out) => {
                assert_eq!(out.from, first.to_hex());
                assert_eq!(out.to, second.to_hex());
            }
            other => panic!("expected Response::Diff, got {other:?}"),
        }
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

    // -----------------------------------------------------------------
    // Audit finding #5 carve-out — read-idle timeout on the connection
    // loop. A short injected deadline keeps the tests fast.
    // -----------------------------------------------------------------

    /// Build a minimal daemon state with explicit limits. Returns the
    /// `TempDir` too — the caller keeps it alive for the test.
    async fn minimal_state_with_limits(
        cfg: crate::limits::LimitsConfig,
    ) -> (std::sync::Arc<DaemonState>, tempfile::TempDir) {
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
            .unwrap()
            .with_limits(cfg),
        );
        (state, dir)
    }

    /// Build a minimal daemon state. Returns the `TempDir` too — the caller
    /// keeps it alive for the test, and it cleans up on drop.
    async fn minimal_state() -> (std::sync::Arc<DaemonState>, tempfile::TempDir) {
        minimal_state_with_limits(crate::limits::LimitsConfig::default()).await
    }

    /// Issue #118: with the commit queue bounded at 1, a second
    /// lock-taking request is rejected with a structured retryable
    /// Concurrency error instead of parking unboundedly, and the depth
    /// gauge tracks the queued occupant.
    #[tokio::test]
    async fn commit_queue_full_rejects_instead_of_parking() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let cfg = crate::limits::LimitsConfig {
            commit_queue_depth: 1,
            ..Default::default()
        };
        let (state, _dir) = minimal_state_with_limits(cfg).await;

        // Two commits so Diff has refs to resolve.
        let first = make_commit(&state, "main", "first", b"v1".to_vec()).await;
        let second = make_commit(&state, "main", "second", b"v2".to_vec()).await;
        assert_ne!(first, second);

        // Hold commit_lock from the test side so the occupier parks.
        let guard = Arc::clone(&state.commit_lock).lock_owned().await;

        // Occupier: takes the single queue slot, parks on the lock.
        let occupier = dispatch(
            Arc::clone(&state),
            Request::Diff {
                from: first.to_hex(),
                to: "main".to_string(),
            },
            None,
        );
        tokio::pin!(occupier);
        tokio::select! {
            res = &mut occupier => panic!("occupier must park on commit_lock, got {res:?}"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        assert_eq!(
            state.commit_queue_depth.load(Ordering::Relaxed),
            1,
            "gauge must count the parked occupant"
        );

        // Queue is full: the next lock-taking request is rejected NOW,
        // not parked.
        let mut prompts = std::collections::BTreeMap::new();
        prompts.insert("system.md".to_string(), b"v3".to_vec());
        let rejected = tokio::time::timeout(
            Duration::from_secs(1),
            dispatch(
                Arc::clone(&state),
                Request::Commit(agentic_proto::CommitInput {
                    message: "rejected".to_string(),
                    author: Some("tester".to_string()),
                    code_sha: None,
                    branch: Some("main".to_string()),
                    prompts,
                    mcp_servers: Vec::new(),
                    model: None,
                    no_memory: true,
                }),
                None,
            ),
        )
        .await
        .expect("rejection must be immediate, not queued")
        .expect("dispatch returns Ok(Response::Error), not Err");
        match rejected {
            Response::Error {
                class,
                code,
                retryable,
                ..
            } => {
                assert_eq!(class, agentic_proto::ErrorClass::Concurrency);
                assert_eq!(code, "commit_queue_full");
                assert!(retryable, "queue-full must be retryable");
            }
            other => panic!("expected Concurrency error, got {other:?}"),
        }

        // Release the lock: the occupier completes and the gauge drains.
        drop(guard);
        let response = tokio::time::timeout(Duration::from_secs(2), &mut occupier)
            .await
            .expect("occupier completes once lock is free")
            .expect("occupier diff succeeds");
        assert!(matches!(response, Response::Diff(_)));
        assert_eq!(state.commit_queue_depth.load(Ordering::Relaxed), 0);
    }

    /// A peer that connects and sends nothing is dropped once the read-idle
    /// deadline elapses, instead of pinning the handler task forever. The
    /// handler future isn't `Send` (the daemon drives it via `spawn_local`),
    /// so we await it directly under an outer guard rather than `spawn`.
    #[tokio::test]
    async fn idle_connection_is_dropped_after_deadline() {
        use std::time::Duration;
        let (state, _dir) = minimal_state().await;
        let (client, server) = tokio::net::UnixStream::pair().unwrap();

        let fut = handle_connection_with_deadlines(
            state,
            server,
            0,
            None,
            Duration::from_millis(150),
            Duration::from_secs(30),
        );
        // The client holds the socket open but sends nothing.
        let outcome = tokio::time::timeout(Duration::from_secs(2), fut)
            .await
            .expect("handler must return well before the outer 2s guard");
        let err = outcome.expect_err("an idle connection must be closed with an error");
        assert!(
            format!("{err:#}").contains("idle"),
            "error should name the idle deadline; got: {err:#}"
        );
        drop(client);
    }

    /// A peer sending complete frames spaced *within* the deadline is not
    /// dropped: the clock resets per frame. A clean close returns Ok. Driven
    /// on a `LocalSet` because the handler future isn't `Send`.
    #[tokio::test]
    async fn well_spaced_frames_are_not_dropped() {
        use agentic_proto::framing::write_frame;
        use agentic_proto::{Envelope, Request};
        use std::time::Duration;

        let (state, _dir) = minimal_state().await;
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
        let idle = Duration::from_millis(200);

        let local = tokio::task::LocalSet::new();
        let handle = local.spawn_local(handle_connection_with_deadlines(
            state,
            server,
            0,
            None,
            idle,
            Duration::from_secs(30),
        ));
        local
            .run_until(async move {
                // Two Pings, spaced 120ms apart — each gap is under the 200ms
                // deadline, so the connection must survive both.
                for i in 0..2 {
                    let env = Envelope::new(format!("corr-{i}"), Request::Ping);
                    write_frame(&mut client, &env).await.unwrap();
                    // Drain the Pong so the server's write doesn't back up.
                    let _reply = read_frame_bytes(&mut client).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(120)).await;
                }
                // Clean close: dropping the client ends the loop with Ok.
                drop(client);
                let outcome = handle.await.expect("handler task should not panic");
                assert!(
                    outcome.is_ok(),
                    "well-spaced frames + clean close must not error; got: {outcome:?}"
                );
            })
            .await;
    }

    /// Issue #118: a request over the per-UID rate budget gets a
    /// structured retryable Concurrency reply and the connection
    /// SURVIVES — after refill the next request succeeds.
    #[tokio::test]
    async fn rate_exhausted_request_is_rejected_and_connection_survives() {
        use agentic_proto::framing::{read_frame, write_frame};
        use std::time::Duration;

        let cfg = crate::limits::LimitsConfig {
            rate_per_uid: 1, // burst 2
            ..Default::default()
        };
        let (state, _dir) = minimal_state_with_limits(cfg).await;
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();

        let local = tokio::task::LocalSet::new();
        let handle = local.spawn_local(handle_connection_with_deadlines(
            state,
            server,
            1000, // observed uid — any value; keys the bucket
            None,
            Duration::from_secs(30),
            Duration::from_secs(30),
        ));
        local
            .run_until(async move {
                // The burst of 2 passes.
                for i in 0..2 {
                    write_frame(
                        &mut client,
                        &Envelope::new(format!("ok-{i}"), Request::Ping),
                    )
                    .await
                    .unwrap();
                    let reply: Envelope<Response> =
                        read_frame(&mut client).await.unwrap().expect("reply");
                    assert!(matches!(reply.payload, Response::Pong));
                }
                // The third rapid request trips the budget.
                write_frame(&mut client, &Envelope::new("limited", Request::Ping))
                    .await
                    .unwrap();
                let reply: Envelope<Response> =
                    read_frame(&mut client).await.unwrap().expect("reply");
                match reply.payload {
                    Response::Error {
                        class,
                        code,
                        retryable,
                        ..
                    } => {
                        assert_eq!(class, agentic_proto::ErrorClass::Concurrency);
                        assert_eq!(code, "rate_budget_exhausted");
                        assert!(retryable);
                    }
                    other => panic!("expected rate rejection, got {other:?}"),
                }
                // Connection survived: after >1s the bucket refills.
                tokio::time::sleep(Duration::from_millis(1200)).await;
                write_frame(&mut client, &Envelope::new("after-refill", Request::Ping))
                    .await
                    .unwrap();
                let reply: Envelope<Response> =
                    read_frame(&mut client).await.unwrap().expect("reply");
                assert!(matches!(reply.payload, Response::Pong));
                drop(client);
                let outcome = handle.await.expect("handler must not panic");
                assert!(outcome.is_ok(), "clean close expected, got {outcome:?}");
            })
            .await;
    }

    /// Issue #118 final review (2026-07-10): malformed-but-attributable
    /// envelopes (e.g. an unsupported `protocol_version`) must debit the
    /// same per-UID rate budget as well-formed requests. Otherwise a peer
    /// can spam garbage bytes forever without ever tripping the limiter,
    /// since the parse-error reply costs the daemon real work. Once the
    /// budget is exhausted, the daemon must close instead of replying.
    #[tokio::test]
    async fn malformed_envelopes_debit_rate_budget_and_close_when_exhausted() {
        use agentic_proto::framing::{read_frame, write_frame};
        use std::time::Duration;

        let cfg = crate::limits::LimitsConfig {
            rate_per_uid: 1, // burst 2
            ..Default::default()
        };
        let (state, _dir) = minimal_state_with_limits(cfg).await;
        let (mut client, server) = tokio::net::UnixStream::pair().unwrap();

        let local = tokio::task::LocalSet::new();
        let handle = local.spawn_local(handle_connection_with_deadlines(
            state,
            server,
            2000, // observed uid — any value; keys the bucket
            None,
            Duration::from_secs(30),
            Duration::from_secs(30),
        ));
        local
            .run_until(async move {
                // A malformed-but-attributable envelope: a correlation_id
                // we can recover, paired with an unsupported
                // protocol_version, so parse fails with the Attributable
                // version_mismatch variant.
                let malformed = serde_json::json!({
                    "correlation_id": "m1",
                    "protocol_version": 9999,
                    "payload": {"op": "ping"}
                });

                // The burst of 2 malformed frames each get a structured
                // Protocol-class reply, and each debits a token.
                for i in 0..2 {
                    let mut frame = malformed.clone();
                    frame["correlation_id"] = serde_json::json!(format!("m{i}"));
                    write_frame(&mut client, &frame).await.unwrap();
                    let reply: Envelope<Response> =
                        read_frame(&mut client).await.unwrap().expect("reply");
                    match reply.payload {
                        Response::Error { class, code, .. } => {
                            assert_eq!(class, agentic_proto::ErrorClass::Protocol);
                            assert_eq!(code, "version_mismatch");
                        }
                        other => panic!("expected protocol error, got {other:?}"),
                    }
                }

                // The 3rd malformed frame trips the exhausted budget: the
                // daemon closes the connection instead of replying.
                write_frame(&mut client, &malformed).await.unwrap();
                let read_result = read_frame::<_, Envelope<Response>>(&mut client).await;
                match read_result {
                    Ok(None) => {} // clean EOF — connection closed
                    Ok(Some(reply)) => {
                        panic!("expected no reply once rate budget is exhausted; got {reply:?}")
                    }
                    Err(_) => {} // reset/broken pipe — also an acceptable close
                }

                let outcome = tokio::time::timeout(Duration::from_secs(2), handle)
                    .await
                    .expect("handler must not hang")
                    .expect("handler task should not panic");
                let err = outcome.expect_err("handler must close with an error");
                assert!(
                    format!("{err:#}").contains("rate budget"),
                    "error should name the rate budget; got: {err:#}"
                );
            })
            .await;
    }

    /// Issue #118: a response write that stalls (peer stops reading)
    /// hits the write-idle deadline instead of pinning the task. Unit
    /// test of the write helper via a tiny duplex buffer — a real
    /// UnixStream's kernel buffer is too large to fill with a Pong.
    #[tokio::test]
    async fn stalled_response_write_hits_write_idle_deadline() {
        use std::time::Duration;
        // 16-byte pipe: the serialized envelope exceeds it, so write_all
        // pends until someone reads. Nobody reads.
        let (client, mut server_side) = tokio::io::duplex(16);
        let reply = Envelope::new("w1".to_string(), Response::Pong);
        let err = write_reply(
            &mut server_side,
            &reply,
            Duration::from_millis(100),
            None,
            0,
            "w1",
        )
        .await
        .expect_err("stalled write must error out at the deadline");
        assert!(
            format!("{err:#}").contains("write-idle"),
            "error should name the write-idle deadline; got: {err:#}"
        );
        drop(client);
    }
}
