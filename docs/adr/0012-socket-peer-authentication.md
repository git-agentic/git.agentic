# ADR-0012: Socket Peer Authentication and Commit Attestation

**Status:** Accepted (2026-07-10 — status line reconciled; the decision was ratified in practice when the implementation landed and ADR-0014 built on it)
**Date:** 2026-05-21
**Deciders:** Toni
**Closes:** [`git.agentic-threat-model.md`](../../git.agentic-threat-model.md) TM-001 (exfiltration via `ReadObject`), TM-003 (integrity forgery via `Commit`)
**Closed in:** `864b5c3` (2026-05-21) — `SO_PEERCRED` accept-loop check + peer-UID threading; hardened by issue #118 per-UID admission control (`crates/agenticd/src/limits.rs`)
**Relates to:** [ADR-0001](./0001-architecture-foundations.md) Decision 1 (tuple-as-version), [ADR-0002](./0002-substrate-and-supercommit.md) Decision 6 (Commit object as platform API contract), [ADR-0003](./0003-claude-agent-sdk-integration.md) Decision 3 (framework-neutral SDK contract), [ADR-0004](./0004-realtime-agenticd-for-executor.md) (sidecar deployment shape).

## Context

The 2026-05-21 threat model (`git.agentic-threat-model.md`) reframes the daemon's trust posture for the v1.0 sidecar deployment shape defined by [ADR-0004](./0004-realtime-agenticd-for-executor.md). In that shape `agenticd` runs as a sidecar container co-located with a Coding worker inside one Cloud Run instance, and the worker speaks to the daemon over a Unix domain socket using the `agentic-proto` wire protocol.

Two design facts about the worker matter:

1. The worker runs LLM-driven code. Tickets it executes may include adversarial prompt injection.
2. Today the worker shares a UID with the daemon and reaches the socket through the same filesystem mount.

The current accept path in `crates/agenticd/src/main.rs` reads:

```rust
let listener = UnixListener::bind(&socket_path)?;
loop {
    let (sock, _) = listener.accept().await?;
    tokio::task::spawn_local(handle_connection(state.clone(), sock));
}
```

There is no peer-identity check. Every accepted connection is treated as equally trusted, and every `Request` variant — `Ping`, `Commit`, `Rollback`, `Log`, `Diff`, `ReadObject`, `ResolveRef` — is dispatched without asking *who* is on the other side.

This means:

- **TM-001:** the worker can enumerate every branch ref via `ResolveRef`, then fetch every object by hash via `ReadObject`. Each response returns up to 10 MiB of raw bytes (per the size guard in `crates/agenticd/src/server.rs:222`). The full tuple state — every prompt, every memory-segment manifest, every Commit blob — is reachable by a process that the threat model now categorises as fully adversarial.
- **TM-003:** the worker can construct any shape of `CommitInput`, set `no_memory: true` to skip the Postgres branch, and ship a commit whose `prompts` and `tools` content is whatever it chose. That commit lands honestly on the branch ref and is indistinguishable in the recorded history from a commit shaped by a legitimate operator.

Both abuse paths share a root cause: the daemon's wire protocol has no concept of *who* its peer is. Closing that gap is the architectural fix; downstream rate limits, audit logs, and Commit-history forensics all depend on having a peer identity to bind them to.

The fix has to fit four constraints:

- **Backwards compatibility on the Commit object.** Per [ADR-0002](./0002-substrate-and-supercommit.md) Decision 6, the Commit object is the platform API contract. We can extend it but not break wire compatibility.
- **The local-Docker-compose demo path keeps working.** Per [`CLAUDE.md`](../../CLAUDE.md), the demo is the discipline. A flag-gated escape hatch must exist for developer machines where UID matching is meaningless.
- **No new daemon dependencies beyond what's already in the workspace.** The Linux peer-credentials syscall is available via `libc` / `socket2`, both already transitively in the tree.
- **Platform-partner integrations follow the same contract.** Per [ADR-0003](./0003-claude-agent-sdk-integration.md) Decision 3, the SDK contract must stay framework-neutral. `peer_uid` is part of the contract, not a sidecar-specific extension.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **`agenticd` reads peer credentials on every socket accept via `SO_PEERCRED` (Linux).** A repeatable `--allowed-uid <UID>` CLI flag defines the allowlist; connections from any other UID are closed immediately. | Treat every connection's identity as known and decidable at accept time, before any `Request` dispatch. UID is the strongest identity primitive available on a same-host Unix-socket boundary without adding mTLS or a token exchange. |
| 2 | **The `Commit` object gains `peer_uid: Option<u32>`** (and optionally `peer_pid: Option<u32>` for forensics). The dispatcher reads it from the accepted connection's peer credentials and threads it through `commit::execute` into the `CommitInputs` that go to `agentic-core::stage_and_commit_with_now`. | The Commit object is the platform API contract (ADR-0002 D6). Attesting *who* shaped each commit is the audit primitive that closes TM-003 and underpins every downstream forensics tool. |
| 3 | **Daemon refuses to start in production without an allowlist** unless `--insecure-allow-any-uid` is explicitly passed. The flag's name is deliberately self-documenting. | Fail-secure default. Demo and developer paths get an escape hatch but cannot fall into it silently. |
| 4 | **Wire-protocol additions are additive `Option` fields.** Older readers ignore the new field; older writers produce commits with `peer_uid: None`, which validators MUST accept as "pre-attestation history". | Backwards compatibility per ADR-0002 D6. The contract extends; nothing breaks. |
| 5 | **Linux-first. macOS-native development uses `--insecure-allow-any-uid`.** A future ADR may add `LOCAL_PEERCRED` (macOS) and `getpeereid()` (BSD) support; v1.0 does not. | The sidecar deployment shape is Linux Cloud Run. macOS appears only on developer machines where the operator is trusted by definition. Building a cross-platform peercred shim today would buy nothing for the threat model. |
| 6 | **Every rejected connection is logged with the offending UID and PID at `tracing::warn!` level.** Every accepted connection logs at `tracing::debug!`. | The observability primitive a sidecar operator needs to detect a misconfigured worker, and the audit-log row a forensics consumer reads after the fact. |

## Decisions

### Decision 1 — `agenticd` reads peer credentials on every accept

Immediately after `UnixListener::accept()` returns a `(UnixStream, _)`, the daemon issues the `SO_PEERCRED` socket option call on the underlying file descriptor (via `socket2::SockRef::peer_cred()` or a direct `libc::getsockopt` against `SOL_SOCKET / SO_PEERCRED`) and reads the peer's UID. The peer credentials are captured at connect time by the kernel and are not spoofable by the connecting process.

If the UID is not in the `--allowed-uid` allowlist, the daemon closes the connection without sending any envelope. The rejection is logged with the rejected UID and PID at `tracing::warn!`.

If the UID is in the allowlist, the daemon proceeds with the normal `handle_connection` path, but carries the UID into the connection's `DaemonState`-derived handler context so dispatch can attest with it (see Decision 2).

The `--allowed-uid <UID>` CLI flag is repeatable, so deployments with multiple legitimate UIDs (operator + worker, for example) can list them all. Omitting the flag entirely is an error in production (see Decision 3).

### Decision 2 — Commit object attests peer UID

The `Commit` struct in `crates/agentic-core/src/object.rs` and the JSON wire schema in `crates/agentic-proto/src/lib.rs` gain a new field:

```rust
pub peer_uid: Option<u32>,
```

The dispatcher reads the peer UID from the accepted connection (captured per Decision 1) and writes it into the `CommitInputs` going to `agentic-core::stage_and_commit_with_now`. The peer UID becomes part of the canonical Commit blob bytes that the BLAKE3 hash commits to.

An operator running `agentic log` or `agentic diff` sees the `peer_uid` field on every commit. Forensics tools can group commits by peer UID, audit which UID shaped which commit, and detect anomalies (e.g., a Commit attributed to a UID that was never authorised).

`peer_pid` may be included alongside as a debugging aid (the PID at accept time). It is informational only — PIDs recycle and carry no enforcement value. Decision 6 makes it optional; the v1.0 implementation may omit it to keep the schema minimal.

### Decision 3 — Production startup refuses to run without an allowlist

If `--allowed-uid` is not passed at startup AND `--insecure-allow-any-uid` is not explicitly passed, `agenticd` exits with a non-zero status and a fail-secure error message. The check happens in `main.rs` before the socket is bound, so a misconfigured deployment fails immediately rather than serving every connection.

`--insecure-allow-any-uid` exists to support:

1. The local-Docker-compose demo (`examples/langgraph-rollback/`), where the daemon and the LangGraph agent run as the same UID on a single developer machine and the operator is trusted by definition.
2. macOS-native development (where Linux's `SO_PEERCRED` is not available; see Decision 5).
3. CI runs that exercise the daemon against synthetic clients.

The flag is deliberately verbose. Any deployment script that passes it must do so explicitly — there is no short alias, no default-on path, no environment variable, and no implicit fallback when `--allowed-uid` parsing fails. A failure to enable peer auth in production should require deliberate action.

### Decision 4 — Wire additions are additive and backwards-compatible

Per [ADR-0002](./0002-substrate-and-supercommit.md) Decision 6, the Commit object is the platform API contract. Adding `peer_uid: Option<u32>` is an additive extension:

- A reader from a pre-ADR-0012 build deserialising a new commit will see an unknown field and (depending on the serde config) either ignore it or accept it as `None`. The current `Commit` definition uses serde defaults, which means unknown fields are ignored. Newer readers reading older commits will see `peer_uid: None` ("pre-attestation history").
- A writer from a pre-ADR-0012 build cannot produce `peer_uid`. Newer validators MUST treat `peer_uid: None` as legitimate pre-attestation data, not as a missing-attestation error.

The Commit's BLAKE3 hash changes the moment `peer_uid` becomes `Some(u32)`. That is the intended behaviour: every Commit shaped under the new daemon is distinguishable in its content-addressing from any Commit shaped before. There is no "best-effort attestation" middle ground; either the field is set and the hash reflects it, or the field is `None` and the commit is honestly marked as pre-attestation.

### Decision 5 — Linux-only in v1.0; macOS uses the escape hatch

The v1.0 sidecar deployment shape ([ADR-0004](./0004-realtime-agenticd-for-executor.md)) runs in Cloud Run on Linux. `SO_PEERCRED` is the Linux peercred mechanism, returning a `struct ucred { pid; uid; gid; }` via `getsockopt`.

macOS uses `LOCAL_PEERCRED` (returning `struct xucred`) and `LOCAL_PEERPID`/`getpeereid()` for the PID and UID respectively. The shape is different from Linux's `ucred` and requires a platform-specific code path.

For v1.0 we accept that macOS-native development requires `--insecure-allow-any-uid`. The rationale:

- Production deployments are Linux Cloud Run; macOS does not appear in the threat model's in-scope deployments.
- macOS development is by definition operator-trusted; the demo discipline runs on Linux Docker-compose where the same is true.
- Building a cross-platform peercred abstraction today would absorb engineering time from the v1.0 hardening sprint without closing any threat-model row.

A v1.1 follow-up may add `LOCAL_PEERCRED` and `getpeereid()` support behind a `PeerCred` trait abstraction. That's tracked as a v1.1 item, not in this ADR.

### Decision 6 — Observability of peer-auth decisions

Every accept-path peer-auth decision emits a structured log line:

- **Accepted:** `tracing::debug!(target: "agenticd::accept", peer_uid, peer_pid, "connection accepted")`.
- **Rejected:** `tracing::warn!(target: "agenticd::accept", peer_uid, peer_pid, allowed_uids = ?state.allowed_uids, "connection rejected: UID not in allowlist")`.
- **Insecure mode:** at daemon startup, if `--insecure-allow-any-uid` is in effect, `tracing::warn!(target: "agenticd::accept", "running with --insecure-allow-any-uid; every connection is accepted regardless of peer UID")` fires once during initialisation.

These three lines give a sidecar operator the audit trail they need to:

- Confirm peer-auth is enforced at deployment time (no surprise insecure mode).
- Detect a misconfigured worker that's connecting under the wrong UID (warn-level rejection lines accumulate visibly).
- Reconstruct after the fact which UID shaped which commit (debug-level accept lines join with Commit `peer_uid` records).

## Consequences

**Positive:**

- TM-001 closes: a worker whose UID is not in the allowlist cannot open a connection in the first place, so no `Request::ReadObject` enumeration is possible.
- TM-003 closes against external/cross-tenant adversaries: any `Commit` shaped from outside the allowlist is rejected before it touches the dispatcher. Commits shaped from inside the allowlist now carry attribution.
- The Commit object becomes audit-grade. `agentic log` and `agentic diff` surface `peer_uid` to operators; downstream tooling can build acceptance criteria like "every Commit on `main` must have `peer_uid` set" without further protocol changes.
- The fail-secure default matches the project's stance throughout [`CLAUDE.md`](../../CLAUDE.md): refuse to start rather than serve insecurely without explicit operator consent.
- The platform-partner integration story tightens. [ADR-0003](./0003-claude-agent-sdk-integration.md) Decision 3 says the SDK contract is framework-neutral; `peer_uid` is a clean, framework-neutral attestation primitive that any integrator can satisfy by running their worker under a declared UID.

**Negative:**

- The Commit object schema changes. Every consumer of the JSON wire format (the Python SDK, the CLI, future platform integrators) must accept the new optional field. The schema change is additive, so no consumer breaks, but the wire-compat note in [`crates/agentic-proto`](../../crates/agentic-proto) needs to call it out explicitly.
- macOS-native development requires `--insecure-allow-any-uid`. Demo and contributor paths are unaffected, but a contributor who runs the daemon directly on macOS without the flag will see a startup-refusal error. The error message must point to the flag.
- The daemon binds `agentic-proto` more tightly to Unix-socket semantics. Any future transport (TCP, named pipes on Windows, a hypothetical Cloud Run inter-container shared socket) needs its own peer-identity story. That's a v1.1+ concern; v1.0 is Unix-socket only.
- Sidecar deployments that share a UID between the worker and the operator (the v1.0 baseline per [ADR-0004](./0004-realtime-agenticd-for-executor.md)) get less defense from this ADR than deployments that run the worker under a distinct UID. The mitigation graph still tightens — the worker is no longer accepted by anyone, even at the kernel layer, unless its UID is allowlisted — but the same-UID baseline means TM-001/TM-003 close against external adversaries more than against the worker itself.

**Risks to revisit:**

- If the platform partner cannot run the worker under a UID distinct from the operator's UID in their Cloud Run deployment, this ADR's defense against the worker reduces to "the daemon and worker are in the same trust zone, but no other UID can connect." That is still a meaningful reduction in attack surface (no cross-tenant contamination, no other-container connections), but it is not the full TM-001/TM-003 close. Verify with the platform partner before v1.0 ships.
- The Commit object's hash changes once `peer_uid` is populated. Any pinned-hash references in test fixtures, integration tests, or external documentation must be regenerated. Audit the test suite at PR-2 time.
- The `--insecure-allow-any-uid` flag is the kind of escape hatch that gets quietly turned on and forgotten. Decision 6's startup warning ("running with --insecure-allow-any-uid") is the recurring observability primitive that catches this in production logs.

See also: [`docs/superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md`](../superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md) §"ADR-0012" (the sprint design that frames this ADR alongside ADR-0013/0014/0015 as the pre-public-release hardening pass), and `git.agentic-threat-model.md` TM-001 and TM-003 (the rows this ADR closes).
