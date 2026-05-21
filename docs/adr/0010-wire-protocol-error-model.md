# ADR-0010: Wire-Protocol Error Model and Binary Payload Carriage

**Status:** Proposed
**Date:** 2026-05-21
**Deciders:** Toni
**Extends:** [ADR-0002](./0002-substrate-and-supercommit.md) Decision 2 (Commit object as platform API contract)
**Relates to:** [`docs/ops/2026-05-21-agenticd-architectural-analysis.md`](../ops/2026-05-21-agenticd-architectural-analysis.md) §A6 / §B6 / §B13 / §B14 / §R7 (the recommendations and risk this ADR unblocks)

## Context

The 2026-05-21 architectural analysis of `crates/agenticd/` surfaced three wire-protocol limitations that compose into one design problem:

1. **B6** — `Response::Error { message: String }` is the only error variant. The dispatch loop maps every `Err` return into this one shape. A semantic "ref not found" (`server.rs:139–142`), a transient Postgres connection failure, a 16 MiB-exceeding object, and a corrupt-object integrity error all reach the client as `format!("{e:#}")`. Clients (CLI, SDK) cannot programmatically distinguish retryable from terminal failure.
2. **B13** — `CommitInput.prompts: BTreeMap<String, String>` (`agentic-proto/src/lib.rs:94`) requires UTF-8. Binary prompt payloads (compiled templates, instruction blobs with embedded nul bytes, non-UTF-8 character sets) fail at JSON deserialisation in the framing layer, before reaching `dispatch`. The object store stores raw bytes; the wire cannot carry them.
3. **B14** — Frame-level errors from `read_frame` / `write_frame` (`server.rs:111–125`) close the connection without sending a `Response::Error`. The client sees a half-closed socket; it cannot distinguish a crashed daemon from a frame-size violation or a serialisation failure.

These are all properties of `agentic-proto/src/lib.rs`, the wire contract every SDK, CLI, and daemon version must agree on. Touching them is not a daemon-internal change. Per [CLAUDE.md](../../CLAUDE.md) "Don't break wire compatibility without a new ADR" — this is that ADR.

The audit's recommendation [A6](../ops/2026-05-21-agenticd-architectural-analysis.md#a6) is the daemon-side patch (a `respond_error` helper that maps framing failures into structured envelopes). It assumes a wire shape the daemon can serialise, which is what this ADR pins down.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **Replace `Response::Error { message: String }` with a structured `Response::Error { class, code, message, retryable }`.** | Clients need to programmatically distinguish semantic absence from operational failure without substring-matching. |
| 2 | **`ErrorClass` is a closed enum** at the protocol layer: `Protocol`, `Validation`, `NotFound`, `Storage`, `Memory`, `Concurrency`, `Internal`. Adding a class is a wire-protocol change; adding a `code` within a class is additive. | A closed taxonomy gives clients a stable surface. The free-form `code` slot inside each class is the additive growth path. |
| 3 | **Prompts (and any tree-typed dimension on the wire) become `BTreeMap<String, Bytes>`** with `Bytes` serialised as base64 in JSON, matching the existing `Response::ObjectData.data` shape. | Object store already carries bytes; the wire restriction was incidental. Use the existing base64 convention rather than introducing a second binary-carriage idiom. |
| 4 | **Framing errors that the daemon can attribute to the current envelope produce a best-effort `Response::Error { class: Protocol, … }` reply before connection close.** Framing errors that cannot be attributed (corrupt length prefix on read, oversize on write) still close the connection — there is no envelope to reply to. | Loud-close on unrecoverable framing failure is honest. Best-effort reply on recoverable failure gives clients a correlation_id to log. |
| 5 | **Protocol version bumps via `Envelope.protocol_version: u16` (currently absent).** The current implicit `1` becomes explicit. Clients receiving a higher version than they understand receive `Response::Error { class: Protocol, code: "version_mismatch" }` and close. | Versioning the envelope itself, not just the variants, lets us add fields later (e.g., trace IDs, capabilities) without another wire-break ADR. |
| 6 | **Backward-compat strategy is "bump once, hold forever."** v0 (current shape) and v1 (this ADR's shape) coexist for one release. After v1.0 ships, the daemon drops v0 support; SDK/CLI mismatches surface as version_mismatch. | Maintaining v0 forever is unbounded cost; one release of overlap is enough for SDK and CLI to catch up. |

## Decision details

### Decision 1 — Structured `Response::Error` variant

```rust
// crates/agentic-proto/src/lib.rs (replaces current Error variant)
pub enum Response {
    Ok(Payload),
    Error {
        class: ErrorClass,
        /// Stable string within `class`. Free-form on the wire; SDKs treat as
        /// opaque tokens for matching ("ref_not_found", "schema_mismatch", …).
        code: String,
        message: String,
        /// True iff retrying the same request later may succeed without operator
        /// intervention. Storage and Concurrency classes are retryable by default;
        /// Validation and NotFound are not.
        retryable: bool,
    },
}
```

The `retryable` field is the load-bearing addition: clients (especially `AgenticSessionStore` per [ADR-0005](./0005-sessionstore-amendment-to-adr-0004.md)) need to know whether to back off and retry or fail the calling agent run. The current single-variant shape forces every client to substring-match the message field, which both daemon and clients have to evolve in lockstep.

### Decision 2 — Closed `ErrorClass` taxonomy

```rust
pub enum ErrorClass {
    /// Wire-level: framing, version, malformed envelope, oversize frame.
    Protocol,
    /// Input validation: bad ref name, malformed Commit input, unknown branch
    /// in `--branch`, invalid migration name.
    Validation,
    /// Semantic absence: ref not found, commit hash not found, schema migration
    /// not registered. NOT retryable (caller should query a different identity).
    NotFound,
    /// Object store, refs, filesystem: GCS 5xx, disk full, permission denied.
    /// Retryable unless the error chain indicates persistent corruption.
    Storage,
    /// Postgres memory backend: connection failure, advisory-lock timeout,
    /// schema mismatch, partial-migration orphan. Some retryable, some not —
    /// `retryable` field discriminates per occurrence.
    Memory,
    /// Daemon-internal serialisation: commit_lock contention timeout, snapshot
    /// in progress, another rollback running. Always retryable.
    Concurrency,
    /// Last-resort. Bugs, panics, anything the daemon can't classify. Treat as
    /// non-retryable until a more specific class is added in a future ADR.
    Internal,
}
```

Closed enum at the protocol layer is the discipline that keeps the API surface honest. Adding `ErrorClass::Auth` (when remote `agenticd` lands, per [ADR-0004](./0004-realtime-agenticd-for-executor.md) Decision 2's footnote) requires a new ADR. Adding `code: "advisory_lock_timeout"` to the existing `Memory` class does not.

### Decision 3 — Bytes-typed payloads

Prompts and any future tree-typed dimension on the wire become `Bytes` rather than `String`:

```rust
use serde_with::{base64::Base64, serde_as};

#[serde_as]
pub struct CommitInput {
    #[serde_as(as = "BTreeMap<_, Base64>")]
    pub prompts: BTreeMap<String, Vec<u8>>,
    // tools: same shape if/when tools cross the wire as bytes
    ...
}
```

`serde_with::Base64` is already in the workspace (used by `Response::ObjectData.data`). The CLI and SDK encode/decode at the boundary; consumers continue to receive `String` from text prompts and `Vec<u8>` from binary payloads at their typed-language API surface.

### Decision 4 — Best-effort framing-error envelope

```rust
// crates/agenticd/src/server.rs (sketch)
async fn handle_connection(state: Arc<DaemonState>, sock: UnixStream) -> Result<()> {
    let (reader, writer) = sock.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    loop {
        let envelope = match read_frame::<_, Envelope<Request>>(&mut reader).await {
            Ok(Some(env)) => env,
            Ok(None) => return Ok(()),                          // clean EOF
            Err(FrameError::TooLarge(n)) => {
                // We have no correlation_id; close after a best-effort scream.
                // This is the unattributable case.
                tracing::warn!(frame_size = n, "frame exceeds MAX_FRAME_BYTES");
                return Err(FrameError::TooLarge(n).into());
            }
            Err(e) => return Err(e.into()),
        };
        let correlation_id = envelope.correlation_id.clone();
        let response = match dispatch(Arc::clone(&state), envelope.payload).await {
            Ok(r) => r,
            Err(e) => map_anyhow_to_response_error(e),          // classifies into ErrorClass + code
        };
        let reply = Envelope { correlation_id, payload: response, protocol_version: 1 };
        if let Err(e) = write_frame(&mut writer, &reply).await {
            // Reply itself oversize is rare but possible (ReadObject of a 10 MiB blob
            // base64-expands past 16 MiB). Log + close; client sees correlation_id
            // never returns and triggers its own timeout.
            tracing::warn!(error = %e, "write_frame failed; closing");
            return Err(e.into());
        }
    }
}
```

Two regimes: **attributable** framing errors (well-formed read with a payload the daemon can't handle, write that fails after a successful read) get a typed `Response::Error { class: Protocol, … }` reply where possible. **Unattributable** failures (corrupt length prefix, mid-read truncation before envelope deserialisation) close the connection — there is no envelope to reply to, and the client must use its own timeout.

### Decision 5 — Explicit `protocol_version` on `Envelope`

```rust
pub struct Envelope<P> {
    pub correlation_id: String,
    /// Wire-protocol version. Currently 1. Daemon refuses higher and replies
    /// with `Response::Error { class: Protocol, code: "version_mismatch" }`.
    pub protocol_version: u16,
    pub payload: P,
}
```

The current shape omits `protocol_version` entirely. Adding it now (as part of the v0 → v1 bump) is cheap; adding it later would itself be a wire-break.

### Decision 6 — One-release overlap, then drop v0

The daemon's v1.0.0 release supports both v0 and v1 envelopes — v0 envelopes deserialise (no `protocol_version` field) and get translated into the v1 path internally; the daemon's v1 responses include the new fields. SDK/CLI shipped before v1.0.0 continue to work against v1.0.0.

The daemon's v1.1.0 release drops v0 support. SDK and CLI versions older than v1.0.0 receive `Response::Error { class: Protocol, code: "version_mismatch" }` and close. This is loud-fail, matching the failure semantics in [ADR-0004](./0004-realtime-agenticd-for-executor.md) Decision 4 / [ADR-0005](./0005-sessionstore-amendment-to-adr-0004.md) Decision 2.

Maintaining v0 forever is unbounded cost; one release of overlap gives SDK and CLI consumers a deterministic upgrade window with a clear deadline.

## What does not change

- The `Envelope` framing itself (length-prefixed JSON, MAX_FRAME_BYTES = 16 MiB) is unchanged — `agentic-proto/src/framing.rs` stays as-is. This ADR is about the *payload schema*, not the *framing*.
- The Unix-domain-socket transport is unchanged.
- The `Commit` object schema (per [ADR-0002](./0002-substrate-and-supercommit.md) Decision 2) is unchanged.
- The `AgenticSessionStore` adapter from [ADR-0005](./0005-sessionstore-amendment-to-adr-0004.md) Decision 1 keeps the same `AgenticClient` surface; the `Response::Error` shape change is absorbed inside the client's error translation.

## Consequences

**Positive:**

- Clients can programmatically distinguish retryable from terminal failure without substring-matching. The synchronising `PreToolUse` hook from [ADR-0005](./0005-sessionstore-amendment-to-adr-0004.md) Decision 2 has a clean retry-vs-fail signal.
- Binary prompt payloads stop being a wire-imposed restriction. Compiled-template workflows that today have to base64-encode at the application layer can stop.
- Framing errors that the daemon can attribute now surface with a correlation_id instead of presenting as a dropped socket.
- Protocol version bumps become tractable; adding fields to the envelope later (trace IDs, capabilities, deadline propagation) does not require another wire-break ADR.

**Negative:**

- Wire-break for any SDK/CLI built against v0. v1.0.0 has a one-release coexistence window; v1.1.0 drops v0.
- `ErrorClass` is closed; adding a class requires an ADR. This is the design intent (a stable surface) but it has a friction cost for new error categories.
- Base64-encoded prompts grow the wire size for text payloads by ~4/3. For the demo's 5-prompt example this is negligible; for thousand-prompt deployments it's measurable.

**Risks to revisit:**

- If a v1.1 ADR introduces remote `agenticd` (currently rejected by [ADR-0004](./0004-realtime-agenticd-for-executor.md) Decision 1), `ErrorClass::Auth` lands then. Don't add it preemptively.
- If a future client SDK in a non-JSON language joins (e.g., a Go SDK), the base64-in-JSON convention may want a binary framing variant. Out of scope here; the existing `Response::ObjectData.data` already commits us to base64 for JSON.

## Implementation plan

1. **`agentic-proto` shape update.** Add `ErrorClass` enum, restructure `Response::Error`, change `CommitInput.prompts` to `Vec<u8>` with `serde_with::Base64`, add `Envelope.protocol_version`. Bump `agentic-proto` to a `0.2.0a1` pre-release.
2. **Daemon side (the [A6](../ops/2026-05-21-agenticd-architectural-analysis.md#a6) work).** Implement `map_anyhow_to_response_error` in `agenticd/src/server.rs` — converts the `anyhow` chain into a structured `Response::Error`. Add `respond_error` helper for the attributable framing-error path. Wire `protocol_version` into the dispatch loop.
3. **SDK side.** `agentic-sdk` Python client decodes the new shape; raises class-specific exceptions (`AgenticNotFoundError`, `AgenticStorageError`, etc.) with `retryable` as an attribute. `AgenticSessionStore.append` retry loop reads `retryable` directly.
4. **CLI side.** `agentic` prints `[CLASS:code] message` for errors, with a `--retryable-only-retry` flag for scripted use.
5. **Coexistence shim.** Daemon's v1.0.0 deserialises envelopes without `protocol_version` as v0; translates v0 `CommitInput.prompts: BTreeMap<String, String>` into v1 `BTreeMap<String, Vec<u8>>` by `into_bytes()`. Daemon's v1.1.0 release removes the shim.
6. **Update the [`agenticd` architectural-analysis audit](../ops/2026-05-21-agenticd-architectural-analysis.md)** §A6 to reference this ADR as "blocked by → unblocked by". Close the corresponding follow-up issue.

Owner: TBD.
