# ADR-0004: Real-Time `agenticd` Integration for Executor Sessions

**Status:** Accepted
**Date:** 2026-05-20
**Deciders:** Toni
**Supersedes:** —
**Relates to:** ADR-0003 Decision 2 (this ADR specifies the topology that makes that decision implementable)
**Amendments:** Decisions 3 and 4 amended by [ADR-0005](0005-sessionstore-amendment-to-adr-0004.md) — snapshot primitive is the Claude Agent SDK's `SessionStore.append` (not an `on_checkpoint` hook); loud-fail is preserved via a synchronising `PreToolUse` hook gated on `agenticd` ack.

## Context

ADR-0003 Decision 1 commits the the first platform-partner integration as the first non-LangGraph integration target in v1.0.
ADR-0003 Decision 2 (as revised) commits to **real-time atomic integration** rather than the originally-considered layered/offline manifest-export path.
This ADR answers the question that revision raises: how does `agenticd` actually attach to a Cloud Run worker that runs one ticket and dies?

The Executor's Coding worker is a scale-to-zero Cloud Run instance, `max-concurrency=1`, lifespan minutes to hours, ephemeral local disk.
Three deployment shapes were considered:

- **In-process Rust library** linked into the worker. Lowest latency, but requires `agenticd` to be embeddable (it isn't) and puts a Rust dependency into a Python worker. Big change to both architectures; defer to v2+ if ever.
- **Sidecar process** in the same Cloud Run instance, with shared durable storage. Matches the MVP's "no SaaS" posture (ADR-0001 Decision 10); no network auth surface; one new storage backend.
- **Remote service** shared by all workers. Maximum flexibility, but requires auth and multi-tenant from day one — both explicitly deferred by ADR-0001 Decision 10. Wrong shape for v1.0.

This ADR picks the sidecar.

## Decision

### Decision 1 — Sidecar deployment in the same Cloud Run instance

`agenticd` runs as a sidecar container in the same Cloud Run instance as the Coding worker.
The worker speaks to it over a Unix domain socket — the same `agentic-proto` wire used by the LangGraph integration.
There is no network surface; communication is intra-instance only.

This matches ADR-0001 Decision 10's "self-hosted, no SaaS, no multi-tenant" posture: the daemon runs inside infrastructure the user already operates (the platform partner's Cloud Run service for the Executor), not as a hosted service we run.

### Decision 2 — No network auth in v1.0

Communication is intra-instance over Unix domain socket, secured by the Cloud Run instance boundary and GCP's per-service runtime identity.
No additional auth layer is added to `agentic-proto`.

If a future ADR moves to remote `agenticd` (per the rejected alternative in Context), it must specify auth at that point.
Until then the protocol stays auth-free, consistent with ADR-0001 Decision 10.

### Decision 3 — Snapshot triggers: per-Agent-SDK-checkpoint plus session boundaries

The sidecar records a Commit at three moments:

- **Session start.** Initial tuple state: model identifier and revision, system prompt hash, MCP manifest hashes, working-copy SHA, empty memory.
- **Every Claude Agent SDK checkpoint** as the harness fires them — typically at tool-call boundaries or per message-cycle. The exact firing pattern is the SDK's responsibility; we snapshot whatever it gives us.
- **Session end.** Final tuple state, with PR SHA recorded if a PR was opened, or with a structured failure record if the session failed.

Higher granularity than this (e.g., mid-tool-call instrumentation) is rejected: the per-write cost (see Decision 5) doesn't justify the marginal rollback fidelity.
Lower granularity (session-end only) is the manifest-export path ADR-0003 just rejected.

### Decision 4 — Failure semantics: if the sidecar is unreachable, the worker fails the ticket

There is no degraded mode in v1.0.
Atomic rollback is the contract; silently degrading to manifest-export breaks the contract by creating a state where the user thinks they have atomic rollback but actually has nothing.

If the sidecar process dies mid-session, the worker:

1. Receives an IPC error on the next checkpoint write.
2. Marks the the ticket dispatcher ticket as failed with a structured error pointing at the agentic-side incident (sidecar exit code, last successful checkpoint hash).
3. Exits non-zero. Cloud Run's restart policy applies as for any other worker failure.

This is loud-fail by design.

### Decision 5 — GCS-backed `ObjectStore`

`crates/agentic-core/src/store.rs` exposes an `ObjectStore` trait; the MVP default implementation is local-disk-backed.
This ADR adds a second implementation: GCS-backed, write-through on every checkpoint.

Write-through is required because the rollback contract demands durability before the rollback request can be served.
A checkpoint recorded only in the sidecar's in-memory cache vanishes when the instance scales to zero — that defeats atomic rollback across sessions.
Per-checkpoint GCS write latency (~50–200ms typical) is accepted as the cost of atomicity.

A read-through local cache stays in front of GCS for diff and replay operations that don't require strict freshness.

The trait swap is consistent with ADR-0002 Decision 6 ("storage layer must stay swappable"): the MVP LangGraph path keeps the local-disk store; the Executor sidecar selects GCS via config at startup.
Subsequent platform integrations inherit the GCS backend without re-litigation.

**Possible follow-up.** If the GCS-backed store develops load-bearing decisions around concurrent writers, ordering across instances, or GC, a follow-up ADR (ADR-0005 or later) may codify them. Not blocked on it for v1.0.

## Consequences

**Positive:**

- Makes ADR-0003 Decision 2's atomic-rollback contract implementable.
- The wire protocol (`agentic-proto` over Unix socket) does not have to grow auth or network attachment surface. The MVP daemon stays operationally simple.
- Sidecar topology is well-understood by Cloud Run operators; no novel ops story to teach.
- The GCS-backed `ObjectStore` lands in v1.0 as part of this work — a substrate capability the platform-led direction would have needed anyway.

**Negative:**

- Two-process Cloud Run packaging (worker + sidecar) is more operational complexity than the Executor would otherwise have. the platform partner's deploy pipeline must absorb that.
- Per-checkpoint GCS write is a real latency cost. If the Claude Agent SDK fires checkpoints aggressively, per-ticket runtime grows by the cumulative write time.
- The Coding worker becomes a hard dependency on a sidecar that didn't exist when ADR-0003 was first drafted. Failure modes (sidecar OOM, sidecar crash on flush, partial-checkpoint recovery on instance restart) need explicit testing.

**Risks to revisit:**

- The Claude Agent SDK's checkpoint primitives may not exactly match Decision 3. Verify in week 6 that `on_checkpoint` (or equivalent) fires at boundaries we can snapshot and that the SDK supports pause/restore on demand. If it doesn't, ADR-0003 Decision 2 reverts to manifest-export for v1.0 and this ADR is reopened.
- GCS region/availability incidents block all Executor sessions in v1.0 (single-region dependency). Document the dependency explicitly; consider multi-region in v1.1.
- Sidecar packaging interacts with how the platform partner builds and deploys the Executor (single container with two processes via supervisor, or two containers in one Cloud Run service). Coordinate with that build pipeline starting week 6.

See also:
[ADR-0001](0001-architecture-foundations.md) Decisions 9 (CLI-first) and 10 (self-hosted, no SaaS in MVP),
[ADR-0002](0002-substrate-and-supercommit.md) Decision 6 (storage layer must stay swappable — this ADR exercises that swap),
[ADR-0003](0003-<partner>-executor-integration.md) Decision 2 (the atomic contract this ADR makes implementable).
