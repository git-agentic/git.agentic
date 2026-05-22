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
    // are classified inside `classify_memory_error`'s `Sqlx` arm; we
    // don't carry a direct `sqlx::Error` runtime dependency here.
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
        MemoryError::Backend(_) => (ErrorClass::Memory, "backend_failure", true),
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
/// Defined as a free function on a `&dyn std::error::Error` so this
/// module does not carry a direct sqlx runtime dependency — the error
/// shape is determined from the rendered string. The discriminators
/// here are not exhaustive: anything we don't recognise falls into
/// `(Memory, "postgres_other", false)`.
fn classify_sqlx_like(err: &(dyn std::error::Error + 'static)) -> (ErrorClass, &'static str, bool) {
    let s = err.to_string().to_ascii_lowercase();
    if s.contains("row not found") {
        return (ErrorClass::NotFound, "row_not_found", false);
    }
    if s.contains("pool timed out") || s.contains("pool closed") {
        return (ErrorClass::Memory, "postgres_pool_unavailable", true);
    }
    if s.contains("io error") || s.contains("connection reset") || s.contains("broken pipe") {
        return (ErrorClass::Memory, "postgres_io_failure", true);
    }
    (ErrorClass::Memory, "postgres_other", false)
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
