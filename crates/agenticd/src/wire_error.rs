//! Map an `anyhow::Error` chain into a structured `Response::Error`.
//!
//! Per [ADR-0010] Decision 1, every error reply carries
//! `(class, code, message, retryable)`. The dispatch loop turns every
//! `Err(...)` it returns into a [`Response::Error`] via
//! [`map_anyhow_to_response_error`]; the in-line `Response::Error`
//! constructors in [`server::dispatch`] use the
//! `Response::not_found(...)` / `Response::validation(...)` family of
//! helpers when the class is pinned at the call site (e.g. ref not
//! found, oversize object).
//!
//! Classification is best-effort: the helper walks the error chain
//! looking for the typed error sources we know about (today: the
//! `agentic-core` `Error` enum for storage/integrity/secret-detected,
//! the `sqlx` Postgres failure modes, our own `anyhow` chains with
//! known contexts). Anything we can't classify falls into
//! [`ErrorClass::Internal`] with a generic `code = "unclassified"`.
//!
//! [ADR-0010]: ../../../docs/adr/0010-wire-protocol-error-model.md

use agentic_core::Error as CoreError;
use agentic_memory::Error as MemoryError;
use agentic_proto::{ErrorClass, Response};

/// Map an `anyhow::Error` into a structured [`Response::Error`].
///
/// The full error chain is rendered via `{:#}` into the `message`
/// field. The `(class, code, retryable)` triple is derived by
/// downcasting the chain.
pub fn map_anyhow_to_response_error(err: anyhow::Error) -> Response {
    let message = format!("{err:#}");

    // Walk the error chain looking for typed errors we know how to
    // classify. `agentic_memory::Error` is checked first because it
    // wraps `agentic_core::Error` and `sqlx::Error` — so we look at
    // the most-specific layer before falling through. Raw sqlx errors
    // are classified inside `classify_memory_error`'s `Sqlx` arm.
    if let Some(mem) = err.chain().find_map(|e| e.downcast_ref::<MemoryError>()) {
        let (class, code, retryable) = classify_memory_error(mem);
        return Response::error(class, code, message, retryable);
    }
    if let Some(core) = err.chain().find_map(|e| e.downcast_ref::<CoreError>()) {
        let (class, code, retryable) = classify_core_error(core);
        return Response::error(class, code, message, retryable);
    }

    // Anyhow chains: we look for known root contexts. These are the
    // strings the codebase uses verbatim in `.with_context(...)`
    // calls — keep this list in sync with the call sites that
    // matter for retryability.
    let lower = message.to_ascii_lowercase();
    if lower.contains("daemon is shutting down") {
        // Concurrency: another commit/rollback is in flight or shutdown
        // is draining. Caller can wait and retry.
        return Response::concurrency("daemon_shutting_down", message);
    }
    if lower.contains("invalid hash") {
        return Response::validation("invalid_hash", message);
    }
    if lower.contains("ref not found") || lower.contains("commit not found") {
        return Response::not_found("ref_not_found", message);
    }
    if lower.contains("scanner") && lower.contains("rejected") {
        // Caller would have to mutate the input; not retryable.
        return Response::validation("secret_detected", message);
    }

    // Fallback: an error we couldn't classify. Treat as Internal,
    // non-retryable. The `code = "unclassified"` token is the signal
    // that we should add a typed case for this error site.
    Response::error(ErrorClass::Internal, "unclassified", message, false)
}

fn classify_core_error(err: &CoreError) -> (ErrorClass, &'static str, bool) {
    match err {
        CoreError::NotFound(_) => (ErrorClass::NotFound, "object_not_found", false),
        CoreError::IntegrityError { .. } => {
            (ErrorClass::Storage, "object_integrity_failure", false)
        }
        CoreError::SecretDetected { .. } => (ErrorClass::Validation, "secret_detected", false),
        CoreError::Io(_) => (ErrorClass::Storage, "io_failure", true),
        CoreError::Serialize(_) => (ErrorClass::Storage, "serialization_failure", false),
        CoreError::KindMismatch { .. } => (ErrorClass::Storage, "object_kind_mismatch", false),
        // Fallback for any future `Other(...)` wrapper. Treat as
        // Internal so we surface "we forgot to classify this" loudly.
        CoreError::Other(_) => (ErrorClass::Internal, "core_other", false),
    }
}

fn classify_memory_error(err: &MemoryError) -> (ErrorClass, &'static str, bool) {
    match err {
        // TODO: `MemoryError::Backend(String)` mixes transient failures
        // (connection dropped mid-snapshot, lock waiter timeout) with
        // permanent configuration bugs ("empty identifier", "snapshot
        // called before init"). The right fix is upstream — split the
        // variant in `agentic-memory::Error`. Until then we sniff the
        // string for the known-permanent shapes and downgrade those to
        // non-retryable; the default remains retryable so transient
        // outages still get the retry loop they need. Tracked separately
        // as a v1.1 cleanup.
        MemoryError::Backend(msg) => classify_memory_backend_message(msg),
        MemoryError::SchemaMismatch { .. } => (ErrorClass::Memory, "schema_mismatch", false),
        MemoryError::MissingReverseMigration(_) => {
            (ErrorClass::Memory, "missing_reverse_migration", false)
        }
        // Recurse into the wrapped types so we never lose information
        // for an error that the upstream surface knows how to classify.
        MemoryError::Core(core) => classify_core_error(core),
        MemoryError::Sqlx(sqlx) => classify_sqlx_like(sqlx),
        MemoryError::Other(_) => (ErrorClass::Internal, "memory_other", false),
        // Permanent: streamer is gone and a retry can't bring it
        // back. Surface as non-retryable Memory so the SDK doesn't
        // burn a retry budget on something that needs a daemon
        // restart.
        MemoryError::StreamerShutdown => (ErrorClass::Memory, "streamer_shutdown", false),
    }
}

/// Classify a `MemoryError::Backend(String)` payload. The list is
/// derived from every `Error::Backend(...)` callsite in
/// `crates/agentic-memory` as of the audit on 2026-05-22 — keep it in
/// sync when new callsites land. Anything not matched here is treated
/// as transient (retryable=true) because the safer default for the
/// memory backend is the same as for sqlx: an unrecognised failure
/// usually means a hiccup, and retry is cheaper than surfacing a
/// permanent failure to the agent.
///
/// TODO(v1.1): split `agentic_memory::Error::Backend(String)` into
/// typed variants upstream so this string-sniffing can be deleted.
fn classify_memory_backend_message(msg: &str) -> (ErrorClass, &'static str, bool) {
    let lower = msg.to_ascii_lowercase();

    // pgvector missing — fires on the very first `init()` against a
    // Postgres without the extension. Permanent until an operator
    // runs `CREATE EXTENSION vector;`.
    if lower.contains("pgvector extension is not installed") {
        return (ErrorClass::Memory, "pgvector_not_installed", false);
    }
    // Identifier / configuration validation — caller passed a bad
    // table name or column. Permanent until input changes.
    if lower.contains("empty identifier") || lower.contains("invalid character in identifier") {
        return (ErrorClass::Validation, "invalid_identifier", false);
    }
    // Init-ordering bugs — adapter used before init() ran. Permanent
    // until the daemon's startup path is fixed.
    if lower.contains("called before init")
        || lower.contains("not initialised")
        || lower.contains("not initialized")
    {
        return (ErrorClass::Memory, "backend_not_initialised", false);
    }
    // Static configuration errors.
    if lower.contains("misconfigured") || lower.contains("invalid configuration") {
        return (ErrorClass::Memory, "backend_misconfigured", false);
    }
    // Streamer task died — does not self-restart in v1.0; the daemon
    // must be restarted. Permanent until then; retrying makes no
    // progress.
    if lower.contains("streamer task has shut down")
        || lower.contains("streamer dropped reply channel")
    {
        return (ErrorClass::Memory, "streamer_dead", false);
    }
    // Stored segment row is malformed JSON. Retrying can't change
    // bytes already at rest.
    if lower.contains("segment row is not a json object") {
        return (ErrorClass::Storage, "segment_row_corrupt", false);
    }

    // Fallback: assume transient. See doc comment above for the
    // rationale (same safe-default reasoning as `classify_sqlx_like`).
    (ErrorClass::Memory, "backend_failure", true)
}

/// Classify a `sqlx::Error` that arrives via `MemoryError::Sqlx`.
///
/// Match on the concrete enum variants rather than the rendered
/// string so a future sqlx-side message tweak can't silently shift a
/// connection failure from `(Memory, retryable=true)` into
/// `(Memory, "postgres_other", retryable=true)`.
///
/// The catchall returns `retryable=true` because the safe default for
/// an unrecognised database error is "transient; retry" — most sqlx
/// failures we don't know about are network or pool churn, not
/// permanent config bugs. A wrongly-retryable error wastes work; a
/// wrongly-non-retryable one surfaces a transient hiccup as a permanent
/// failure to the agent, which is the worse outcome for
/// `AgenticSessionStore.append`'s retry loop.
fn classify_sqlx_like(err: &sqlx::Error) -> (ErrorClass, &'static str, bool) {
    match err {
        sqlx::Error::RowNotFound => (ErrorClass::NotFound, "row_not_found", false),
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed => {
            (ErrorClass::Memory, "postgres_pool_unavailable", true)
        }
        sqlx::Error::Io(_) => (ErrorClass::Memory, "postgres_io_failure", true),
        sqlx::Error::Database(_) => (ErrorClass::Memory, "postgres_database", false),
        sqlx::Error::Configuration(_) => (ErrorClass::Memory, "postgres_configuration", false),
        sqlx::Error::ColumnNotFound(_)
        | sqlx::Error::ColumnIndexOutOfBounds { .. }
        | sqlx::Error::ColumnDecode { .. }
        | sqlx::Error::TypeNotFound { .. }
        | sqlx::Error::Decode(_) => (ErrorClass::Memory, "postgres_schema_drift", false),
        // Catchall — assume transient. See doc comment above.
        _ => (ErrorClass::Memory, "postgres_other", true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_error_falls_into_internal_unclassified() {
        let err = anyhow::anyhow!("something exotic the daemon doesn't know about");
        match map_anyhow_to_response_error(err) {
            Response::Error {
                class,
                code,
                retryable,
                ..
            } => {
                assert_eq!(class, ErrorClass::Internal);
                assert_eq!(code, "unclassified");
                assert!(!retryable);
            }
            _ => panic!("expected Response::Error"),
        }
    }

    #[test]
    fn ref_not_found_classifies_as_not_found() {
        let err = anyhow::anyhow!("ref not found: feature-x");
        match map_anyhow_to_response_error(err) {
            Response::Error { class, code, .. } => {
                assert_eq!(class, ErrorClass::NotFound);
                assert_eq!(code, "ref_not_found");
            }
            _ => panic!("expected Response::Error"),
        }
    }

    #[test]
    fn shutdown_classifies_as_retryable_concurrency() {
        let err = anyhow::anyhow!("daemon is shutting down; refusing new write-path work");
        match map_anyhow_to_response_error(err) {
            Response::Error {
                class,
                code,
                retryable,
                ..
            } => {
                assert_eq!(class, ErrorClass::Concurrency);
                assert_eq!(code, "daemon_shutting_down");
                assert!(retryable, "shutdown is transient — caller should retry");
            }
            _ => panic!("expected Response::Error"),
        }
    }

    #[test]
    fn core_not_found_classifies_correctly_through_context_wrap() {
        // Wrap the typed error in an anyhow context the way real call
        // sites do. The downcast through the chain must still find it.
        let h = agentic_core::Hash::of(b"never-stored");
        let core_err: anyhow::Error =
            anyhow::Error::new(agentic_core::Error::NotFound(h)).context("reading object 0xabc");
        match map_anyhow_to_response_error(core_err) {
            Response::Error { class, code, .. } => {
                assert_eq!(class, ErrorClass::NotFound);
                assert_eq!(code, "object_not_found");
            }
            _ => panic!("expected Response::Error"),
        }
    }

    #[test]
    fn invalid_hash_classifies_as_validation() {
        let err = anyhow::anyhow!("invalid hash: not-a-hex-string");
        match map_anyhow_to_response_error(err) {
            Response::Error { class, .. } => assert_eq!(class, ErrorClass::Validation),
            _ => panic!("expected Response::Error"),
        }
    }

    #[test]
    fn core_io_classifies_as_retryable_storage() {
        let io: std::io::Error =
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "GCS dropped us");
        let core_err: anyhow::Error =
            anyhow::Error::new(agentic_core::Error::Io(io)).context("writing blob");
        match map_anyhow_to_response_error(core_err) {
            Response::Error {
                class,
                code,
                retryable,
                ..
            } => {
                assert_eq!(class, ErrorClass::Storage);
                assert_eq!(code, "io_failure");
                assert!(retryable, "I/O is transient; retry");
            }
            _ => panic!("expected Response::Error"),
        }
    }

    #[test]
    fn core_secret_detected_classifies_as_validation() {
        // ADR-0013: SecretDetected is a structural rejection — the
        // caller would have to mutate the input. Not retryable.
        // (Hit list is empty here; the variant tag alone drives the
        // classification.)
        let core_err: anyhow::Error =
            anyhow::Error::new(agentic_core::Error::SecretDetected { hits: Vec::new() });
        match map_anyhow_to_response_error(core_err) {
            Response::Error {
                class,
                code,
                retryable,
                ..
            } => {
                assert_eq!(class, ErrorClass::Validation);
                assert_eq!(code, "secret_detected");
                assert!(!retryable);
            }
            _ => panic!("expected Response::Error"),
        }
    }

    #[test]
    fn memory_streamer_shutdown_is_non_retryable_with_specific_code() {
        // Permanent failure mode — the streamer task is gone and a
        // retry can't reopen the mpsc channel. The SDK must see
        // retryable=false so it doesn't burn its retry budget on
        // something that needs a daemon restart. The `code` must be
        // distinct from the generic `backend_failure` catchall so
        // operator alerts can wire on the right signal.
        let err: anyhow::Error = anyhow::Error::new(MemoryError::StreamerShutdown);
        match map_anyhow_to_response_error(err) {
            Response::Error {
                class,
                code,
                retryable,
                ..
            } => {
                assert_eq!(class, ErrorClass::Memory);
                assert_eq!(code, "streamer_shutdown");
                assert!(
                    !retryable,
                    "StreamerShutdown must be non-retryable; the channel can't reopen"
                );
            }
            _ => panic!("expected Response::Error"),
        }
    }

    #[test]
    fn memory_backend_transient_message_is_retryable() {
        let err: anyhow::Error = anyhow::Error::new(MemoryError::Backend(
            "connection dropped mid-snapshot".to_string(),
        ));
        match map_anyhow_to_response_error(err) {
            Response::Error {
                class,
                code,
                retryable,
                ..
            } => {
                assert_eq!(class, ErrorClass::Memory);
                assert_eq!(code, "backend_failure");
                assert!(retryable);
            }
            _ => panic!("expected Response::Error"),
        }
    }

    #[test]
    fn memory_backend_permanent_messages_classify_with_specific_codes() {
        // Every known permanent shape from `crates/agentic-memory/src/`
        // routes to a code more specific than the catchall, and is
        // marked retryable=false. The triples are
        // (message, expected_class, expected_code).
        // TODO(v1.1): collapse this when MemoryError::Backend is split
        // into typed variants upstream.
        let cases: &[(&str, ErrorClass, &str)] = &[
            (
                "pgvector extension is not installed; run CREATE EXTENSION vector;",
                ErrorClass::Memory,
                "pgvector_not_installed",
            ),
            (
                "empty identifier",
                ErrorClass::Validation,
                "invalid_identifier",
            ),
            (
                "invalid character in identifier: \"weird name\"",
                ErrorClass::Validation,
                "invalid_identifier",
            ),
            (
                "snapshot called before init",
                ErrorClass::Memory,
                "backend_not_initialised",
            ),
            (
                "begin_restore called before init() — no trigger poller is running",
                ErrorClass::Memory,
                "backend_not_initialised",
            ),
            (
                "Postgres adapter not initialised",
                ErrorClass::Memory,
                "backend_not_initialised",
            ),
            (
                "memory adapter misconfigured",
                ErrorClass::Memory,
                "backend_misconfigured",
            ),
            (
                "invalid configuration: missing url",
                ErrorClass::Memory,
                "backend_misconfigured",
            ),
            (
                "streamer task has shut down",
                ErrorClass::Memory,
                "streamer_dead",
            ),
            (
                "streamer dropped reply channel",
                ErrorClass::Memory,
                "streamer_dead",
            ),
            (
                "segment row is not a JSON object: {\"bad\":1}",
                ErrorClass::Storage,
                "segment_row_corrupt",
            ),
        ];
        for (msg, expected_class, expected_code) in cases {
            let err: anyhow::Error = anyhow::Error::new(MemoryError::Backend(msg.to_string()));
            match map_anyhow_to_response_error(err) {
                Response::Error {
                    class,
                    code,
                    retryable,
                    ..
                } => {
                    assert_eq!(&class, expected_class, "wrong class for: {msg}");
                    assert_eq!(&code, expected_code, "wrong code for: {msg}");
                    assert!(!retryable, "permanent shape must not retry; msg={msg}");
                }
                _ => panic!("expected Response::Error"),
            }
        }
    }

    #[test]
    fn sqlx_row_not_found_classifies_as_not_found() {
        // Uses the concrete enum variant rather than a string match.
        let err: anyhow::Error = anyhow::Error::new(MemoryError::Sqlx(sqlx::Error::RowNotFound));
        match map_anyhow_to_response_error(err) {
            Response::Error { class, code, .. } => {
                assert_eq!(class, ErrorClass::NotFound);
                assert_eq!(code, "row_not_found");
            }
            _ => panic!("expected Response::Error"),
        }
    }

    #[test]
    fn sqlx_pool_closed_classifies_as_retryable_memory() {
        let err: anyhow::Error = anyhow::Error::new(MemoryError::Sqlx(sqlx::Error::PoolClosed));
        match map_anyhow_to_response_error(err) {
            Response::Error {
                class,
                code,
                retryable,
                ..
            } => {
                assert_eq!(class, ErrorClass::Memory);
                assert_eq!(code, "postgres_pool_unavailable");
                assert!(retryable, "pool churn is transient; retry");
            }
            _ => panic!("expected Response::Error"),
        }
    }

    #[test]
    fn error_message_preserves_anyhow_chain() {
        let err = anyhow::anyhow!("inner cause").context("outer context");
        match map_anyhow_to_response_error(err) {
            Response::Error { message, .. } => {
                assert!(
                    message.contains("outer context") && message.contains("inner cause"),
                    "anyhow chain must be preserved in message; got: {message}"
                );
            }
            _ => panic!("expected Response::Error"),
        }
    }
}
