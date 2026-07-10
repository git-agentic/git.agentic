# ADR-0014: Destructive-Rollback Approval Gate

**Status:** Accepted (2026-07-09)
**Date:** 2026-05-21
**Deciders:** Toni
**Closes:** [`git.agentic-threat-model.md`](../../git.agentic-threat-model.md) TM-002 (sibling worker triggers destructive down-migration via `Rollback { accept_data_loss: true }`)
**Relates to:** [ADR-0002](./0002-substrate-and-supercommit.md) Decision 5 (bounded rollback for destructive migrations), [ADR-0004](./0004-realtime-agenticd-for-executor.md) Decision 4 (loud-fail discipline), [ADR-0012](./0012-socket-peer-authentication.md) Decision 2 (`peer_uid` on every accepted connection), [issue #37 / audit §A8](../ops/2026-05-21-agenticd-architectural-analysis.md#a8) (the reverse-migration outer-transaction work that wired `accept_data_loss` end-to-end).

## Context

The reverse-migration work in PR #46 (audit §A8) added an `accept_data_loss: bool` field to `Request::Rollback`. When set to `true`, the daemon bypasses the `IRREVERSIBLE`-marker check in `crates/agenticd/src/migrate.rs::check_irreversible` and proceeds with destructive down-migrations.

That flag is the only knob between a worker and arbitrary data loss in the per-tenant Postgres database. The 2026-05-21 threat model surfaced this as TM-002 high-priority:

> Worker corrupts production schema via Rollback + accept_data_loss=true. Worker constructs a Rollback request targeting an old commit with an IRREVERSIBLE migration, sets accept_data_loss=true, and the daemon executes the destructive down-migration inside the outer transaction. Impact: production data loss; Postgres state diverges from any honest snapshot.

Before [ADR-0012](./0012-socket-peer-authentication.md), the sibling Coding worker could open the socket and send this request with no friction. ADR-0012 closes part of the gap — the worker must come from an allowlisted UID — but does NOT close TM-002 by itself. An allowlisted worker can still set `accept_data_loss=true` and trigger the destructive path. UID-allowlisting controls *who can talk to the daemon*; it does not control *which authority can authorize destructive operations within that conversation*.

The threat model recommends an out-of-band approval mechanism. This ADR specifies it.

The constraints:

- **No long-lived secrets in the worker.** The worker is the adversarial party for TM-002. Any approval credential that lives in the worker's process state, env, or filesystem is bypassable by definition.
- **No persistent server-side approval store.** The daemon already has `commit_lock` as its serialization primitive; adding a separate approval-state table or in-memory store complicates the failure model. Approval should be self-contained per request.
- **Fail-closed default.** A daemon without an approval mechanism configured must reject `accept_data_loss=true` outright — not allow it for ergonomics, not silently ignore the flag, not log a warning and proceed.
- **Compatible with the ADR-0004 sidecar shape.** The approval must be issuable by an out-of-band actor (the operator, a side-channel signer, a future KMS) and verifiable by the daemon without contacting that actor at verification time.
- **Backwards-compatible wire schema.** Same constraint as ADR-0012: `Request::Rollback` extends, does not break.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **`Request::Rollback` grows `approval_token: Option<String>`.** When `accept_data_loss == true`, the request is rejected unless `approval_token` is present AND verifies. | Additive wire change; the gate lives on the request itself so the daemon can decide at handler time without consulting external state. |
| 2 | **Approval tokens are HMAC-SHA256 over `(commit_hash, requesting_peer_uid, timestamp)` with a key held by an out-of-band approver.** The daemon verifies; it does not generate. | Self-contained, stateless verification. Operator holds the key; worker cannot forge. The signed payload is the minimum needed to scope the approval to one specific destructive operation. |
| 3 | **Tokens are short-lived (≤ 5 minutes from `timestamp` to verification).** No replay store needed — the window is the only replay defense. | Stateless replay prevention. The 5-minute window matches the operator's coordination loop (run rollback, see audit event, move on) and is short enough that a leaked token's blast radius is bounded. |
| 4 | **Without an approval-key file configured (`--approval-key-file <path>`), `accept_data_loss = true` is always rejected. Fail-closed.** | The escape hatch from ADR-0012 (`--insecure-allow-any-uid`) is per-deployment ergonomic; there is no equivalent for TM-002. Destructive operations require explicit operator configuration, full stop. |
| 5 | **Every `accept_data_loss = true` attempt emits a structured `RollbackForcedDataLoss` audit event regardless of outcome.** Includes `peer_uid`, `target_commit_hash`, decision (`accepted` / `rejected_no_token` / `rejected_expired` / `rejected_invalid_signature`), and a redacted token prefix for forensics. | The audit primitive a sidecar operator monitors and an after-the-fact forensics consumer reads. Rejection paths emit too so an attacker cannot probe for the gate's existence silently. |
| 6 | **HMAC key distribution is operator-managed in v1.0.** The daemon reads it from `--approval-key-file <path>` at startup; rotation is operator-driven (restart the daemon with a new key file). | KMS-backed signing is the v1.1 conversation. The v1.0 shape needs to ship before 2026-05-26 and a file-on-disk key is the simplest contract that closes TM-002. |

## Decisions

### Decision 1 — `approval_token` field on `Request::Rollback`

The `Request::Rollback` variant in `crates/agentic-proto/src/lib.rs` extends:

```rust
Rollback {
    target: String,
    dry_run: bool,
    accept_data_loss: bool,
    approval_token: Option<String>,  // NEW
}
```

The handler in `crates/agenticd/src/rollback/mod.rs::execute` checks the gate FIRST, before any object-store or Postgres work:

1. If `accept_data_loss == false`, proceed as today; ignore `approval_token` entirely.
2. If `accept_data_loss == true`:
   a. If the daemon has no approval-key configured, reject with `Error::ApprovalKeyNotConfigured` (Decision 4).
   b. If `approval_token` is `None`, reject with `Error::ApprovalRequired`.
   c. Verify the token (Decision 2). If invalid/expired, reject with the appropriate typed error.
   d. If valid, proceed with the destructive rollback path. Emit the audit event regardless (Decision 5).

The gate is evaluated before the existing rollback orchestration (memory validation, prompts write-back, reverse migrations). A rejected request returns before any side effects.

### Decision 2 — Token shape: HMAC-SHA256 over a canonical tuple

Approval tokens are HMAC-SHA256 hex-encoded over the canonical byte form of:

```
"git.agentic/rollback-approval/v1"  +  ":"  +
  commit_hash_hex                   +  ":"  +
  requesting_peer_uid_decimal       +  ":"  +
  timestamp_unix_seconds_decimal
```

The wire format on the token field is `"<unix_ts_seconds>:<hex_hmac>"`. The daemon parses the timestamp, recomputes the HMAC using its loaded key, and compares constant-time.

`commit_hash` is the rollback target's commit hash (the value the worker passes in `Request::Rollback.target`, normalized to lowercase 64-char hex). The signed payload binds the approval to exactly one target — an approval issued for commit A cannot be replayed against commit B.

`requesting_peer_uid` is the UID the daemon reads via ADR-0012's `SO_PEERCRED` machinery on the connection that carries the `Request::Rollback`. The signed payload binds the approval to exactly one peer — an approval issued for the worker UID cannot be replayed by a different connection. The operator generating the token must know the worker's UID at signing time, which in the Cloud Run sidecar deployment is fixed by the service config and is therefore knowable.

The `"git.agentic/rollback-approval/v1"` prefix is the HMAC-domain string. It guarantees that an HMAC computed for one purpose (rollback approval) cannot be reused for any other future signed-message format on the same key.

### Decision 3 — Time-bound: ≤ 5 minutes

At verification time the daemon computes `abs(now_unix - token_timestamp)`. If the absolute delta exceeds 300 seconds (5 minutes), the token is rejected with `Error::ApprovalExpired`.

The window is symmetric (covers both stale tokens and clock-skewed-future tokens) so a daemon with a slightly slow clock does not accept arbitrarily-future tokens.

No persistent anti-replay store is needed. Within the 5-minute window a token can be replayed — but only against its bound `(commit_hash, peer_uid)` tuple, and operators in production should treat the window as the "if you sign it, run it immediately" contract. A token that's been used once and leaked still has a bounded blast radius (the same commit + the same UID, within 5 minutes).

The 5-minute window is a constant in v1.0: `const APPROVAL_TOKEN_TTL_SECONDS: u64 = 300;` in `agentic-core/src/approval.rs`. If real operator coordination loops force a wider window, that's a v1.1 conversation.

### Decision 4 — Fail-closed without configured key

If `--approval-key-file <path>` is not passed at daemon startup, OR the file cannot be read at startup, OR the file is empty, the daemon's `DaemonState` records `approval_key: None`.

When `accept_data_loss == true` arrives at a daemon with `approval_key: None`, the request is rejected with a typed error and the audit event fires:

```
RollbackForcedDataLoss {
    peer_uid,
    target_commit_hash,
    decision: "rejected_no_key_configured",
    token_prefix: None,
}
```

There is no override flag for "I want to accept_data_loss without an approval key." The escape hatch from ADR-0012 (`--insecure-allow-any-uid` for peer auth) exists because demo and macOS development paths legitimately need it. For TM-002, the local-Docker-compose demo does not exercise destructive rollback in the broken-prompt demo path (the demo's bad-change scenario is a forward migration that needs forward-record-over rollback, not an IRREVERSIBLE down-migration). So no legitimate demo path needs `accept_data_loss = true` without an approval key.

If a contributor running the demo wants to exercise the destructive path locally, they pass `--approval-key-file ./demo-approval.key` with any 32-byte file. The friction is intentional: every legitimate use of destructive rollback should require explicit operator setup.

### Decision 5 — Audit events on every attempt

Every `accept_data_loss = true` request emits exactly one structured `RollbackForcedDataLoss` event at `tracing::warn!` (rejected) or `tracing::info!` (accepted) level. The event schema:

```rust
struct RollbackForcedDataLoss {
    peer_uid: u32,           // From the connection's SO_PEERCRED
    target_commit_hash: String,
    decision: &'static str,  // accepted, rejected_no_token, rejected_no_key_configured,
                             // rejected_expired, rejected_invalid_signature, rejected_malformed
    token_prefix: Option<String>,  // First 8 chars of the token, redacted hex for forensics
                                   // None when no token was supplied
    audit_event_version: u32,      // 1, for forward compat
}
```

The event fires REGARDLESS of outcome. Rejected paths emit too so that:

1. An attacker probing for the gate's existence shows up in the audit log.
2. Operators monitoring `decision == "rejected_*"` rates can detect misconfigured workers (e.g., workers sending `accept_data_loss` without approval because some upstream code path is broken).
3. After-the-fact forensics can correlate `RollbackForcedDataLoss` events with peer-UID audit lines from ADR-0012.

The `token_prefix` (first 8 hex chars of the supplied token) is included in rejection events so an operator who issued the token can match the rejection to their own sign event. The remaining 56 chars are not logged — a partial leak is bounded and the operator already has the full token in their issuance log.

### Decision 6 — Operator-managed key in v1.0; KMS in v1.1

The key is read once at daemon startup from `--approval-key-file <path>`. The file contains exactly 32 bytes (256 bits) — any deviation aborts daemon startup. The file is read with `tokio::fs::read` (not memory-mapped) so the bytes can be zeroized in `Drop` on shutdown.

Rotation is operator-driven: stop the daemon, write a new key file, restart. There is no in-process rotation in v1.0 — the operational complexity isn't worth the small reduction in key-lifetime risk for a sidecar that restarts on every Cloud Run instance boot anyway.

v1.1 may introduce KMS-backed signing where the operator holds a KMS key reference instead of a raw byte file. That ADR would specify the signing primitive (Cloud KMS asymmetric vs symmetric, audience scoping, retry semantics) and is out of scope here. v1.0's contract is the file-on-disk key as a minimum-viable shape that closes TM-002 before the public flip.

The operator's CLI for issuing tokens is a separate concern, handled by the `agentic` command:

```
agentic rollback --approval <commit-hash> --uid <worker-uid> --key-file <path>
```

This prints a token to stdout that the operator hands to the worker (or pastes into a CI variable, or whatever the deployment shape requires). The token-issuing path lives in `agentic-cli` and does not require the daemon to be running.

## Consequences

**Positive:**

- TM-002 closes. The worker cannot forge an approval token because it does not hold the key. An operator who has access to the key must explicitly sign each destructive operation, bounded to that specific commit + that specific peer UID within a 5-minute window.
- The audit event gives operators a continuous detection primitive. A spike in `RollbackForcedDataLoss { decision: "rejected_*" }` events is the signal that something is probing the gate or that a worker is misconfigured.
- Stateless verification keeps the daemon's failure model simple. No new persistence layer, no new lock ordering, no new race conditions.
- The contract is durable: the HMAC-domain prefix lets us add other signed-message formats on the same key in the future without ambiguity.
- The ADR composes cleanly with ADR-0012. The `peer_uid` from the connection feeds the signed payload directly; the two ADRs together produce "who is connecting" + "what operations they are authorized for", which is the v1.0 authorization model.

**Negative:**

- Operator coordination overhead. Every destructive rollback now requires the operator to (a) know the worker UID, (b) run a CLI to sign, (c) hand the token to the worker before the 5-minute window expires. This is friction; the friction is the point, but it is real.
- Operators who lose the approval-key file lose the ability to authorize destructive rollback. There is no recovery path in v1.0 except restart-the-daemon-with-a-new-key, which invalidates any in-flight tokens.
- Wire schema extension. Older clients that don't know about `approval_token` will produce JSON without the field, which deserializes as `None`. They cannot exercise destructive rollback against a new daemon. Acceptable: destructive rollback is a new operator capability anyway.
- The escape hatch for the demo (need a key file) is real friction for first-time setup. The friction is mitigated by the demo NOT exercising destructive rollback in the broken-prompt scenario, but a contributor doing exploratory testing will hit it.

**Risks to revisit:**

- The 5-minute window is a guess. The first deployment may surface coordination-loop pain (operator signs, worker takes 7 minutes to pick up). If so, the constant in `agentic-core/src/approval.rs` is one-PR tunable.
- File-on-disk key has a known weakness: anyone with read access to the key file can issue tokens. The mitigation in v1.0 is "the operator's machine is trusted; the key file lives there, not on the daemon." If a deployment shape emerges where the key needs to live on a server, the v1.1 KMS-backed signing ADR addresses it.
- The HMAC-domain string `"git.agentic/rollback-approval/v1"` is versioned. A future signed-message format using the same key MUST use a different domain string. Drift here is silent and dangerous; the convention belongs in CONTRIBUTING.md or a domain-strings registry doc when the second signed format appears.

See also: [`docs/superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md`](../superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md) §"ADR-0014" (the sprint design that frames this ADR), `git.agentic-threat-model.md` TM-002 (the row this ADR closes), [ADR-0012](./0012-socket-peer-authentication.md) (the peer-UID source feeding the signed payload).
