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
        MemoryError::Backend(msg) => {
            let lower = msg.to_ascii_lowercase();
            let is_permanent_config_bug = lower.contains("empty identifier")
                || lower.contains("called before init")
                || lower.contains("not initialised")
                || lower.contains("not initialized")
                || lower.contains("misconfigured")
                || lower.contains("invalid configuration");
            if is_permanent_config_bug {
                (ErrorClass::Memory, "backend_misconfigured", false)
            } else {
                (ErrorClass::Memory, "backend_failure", true)
            }
        }
        MemoryError::SchemaMismatch { .. } => (ErrorClass::Memory, "schema_mismatch", false),
        MemoryError::MissingReverseMigration(_) => {
            (ErrorClass::Memory, "missing_reverse_migration", false)
        }
        // Recurse into the wrapped types so we never lose information
        // for an error that the upstream surface knows how to classify.
        MemoryError::Core(core) => classify_core_error(core),
        MemoryError::Sqlx(sqlx) => classify_sqlx_like(sqlx),
        MemoryError::Other(_) => (ErrorClass::Internal, "memory_other", false),
    }
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
    fn memory_backend_permanent_config_bug_is_not_retryable() {
        // The known-permanent shapes (TODO upstream: split the
        // Backend(String) variant) must not flip AgenticSessionStore
        // into retry-forever.
        for msg in [
            "empty identifier on memory snapshot",
            "snapshot called before init",
            "Postgres adapter not initialised",
            "memory adapter misconfigured",
            "invalid configuration: missing url",
        ] {
            let err: anyhow::Error = anyhow::Error::new(MemoryError::Backend(msg.to_string()));
            match map_anyhow_to_response_error(err) {
                Response::Error {
                    class,
                    code,
                    retryable,
                    ..
                } => {
                    assert_eq!(class, ErrorClass::Memory);
                    assert_eq!(code, "backend_misconfigured");
                    assert!(!retryable, "permanent config bug must not retry; msg={msg}");
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
