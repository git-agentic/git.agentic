# ADR-0003: Codento Executor as First Non-LangGraph Integration Target

**Status:** Accepted
**Date:** 2026-05-20
**Deciders:** Toni
**Amends:** ADR-0001 Decision 7

## Context

ADR-0001 Decision 7 names LangGraph as the only framework integration in MVP.
Other frameworks were explicitly deferred to v1.1, "via the same SDK contract."

Three things have changed since that decision was taken.

First, Codento's internal **Executor** (sketched in `codento-core/docs/EXECUTOR.md`) is the most concrete agent-runtime use case we have line-of-sight to.
It is two Cloud Run services — a dispatcher that polls Flux's MCP for ready tickets, and a Coding worker that runs one ticket per scale-to-zero instance against a sandboxed repo checkout, opens a PR, and posts the result back to Flux through Flux's MCP.

Second, the Executor's primary harness is the **Claude Agent SDK**, not LangGraph.
The reasoning in `EXECUTOR.md` §4 is explicit and well-argued: only the Claude Agent SDK gives both benchmark-quality harness behavior and the programmatic surface (per-ticket MCP wiring, streamed tool/result events, checkpoint/resume, per-run model swap) the orchestration needs.
Forge has no SDK; Codex/Aider vary.
Building a harness from scratch (the EXECUTOR.md option C) loses to Anthropic's full-time harness team on the prompt-and-tool engineering that SWE-bench-class benchmarks measure.

Third, the Executor's stated goal is a **harness × model matrix** for evals: the same Prism-authored ticket replayed across (Claude Code, Forge, Codex) × (Opus 4.7, Sonnet 4.6, Gemini) and scored on the same eval set.
This is a tuple-snapshot problem by construction.
Without pinning prompts, tools, model, and session state, the eval results are not honest.

This makes the Executor the first concrete external use case for git.agentic where the value prop is undisputed and the technical fit is tight — in some respects tighter than the LangGraph mapping, because session-based harnesses have cleaner snapshot boundaries than graph-based frameworks.
Leaving it as a v1.1 "and other frameworks too" footnote would be a strategic miss.

It also surfaces a tension already flagged in `CLAUDE.md`: the in-repo MVP ICP (LangGraph teams) and the platform-led strategic direction (integrate with the agent platforms themselves) do not currently agree.
The Executor is the first piece of evidence that stateful coding-agent teams are picking SDK-based harnesses over graph frameworks, and the first opportunity to validate the platform-led direction against a real integrator we control.

## Decision

### Decision 1 — Codento Executor is the first non-LangGraph integration target, in v1.0

ADR-0001 Decision 7 is amended.
MVP framework support is now **LangGraph plus the Claude Agent SDK (via the Codento Executor)**.
All other frameworks remain deferred to v1.1 via the same SDK contract.

This is a deliberate scope add to the 12-week MVP. The cost is justified because:

- The Executor is the first design partner we have full visibility into and influence over. If we cannot integrate with our own sibling project, the "framework-neutral SDK contract" claim in ADR-0001 Decision 7 is unverified.
- The Claude Agent SDK use case stress-tests the Commit object schema (per ADR-0002, the platform API contract) in ways the LangGraph use case does not. Session-message logs, per-tool-call events, ephemeral compute environments — none of these are exercised by a LangGraph Postgres checkpointer. Better to discover schema gaps now than after v1.0 ships and we are locked into wire compatibility.
- The harness × model matrix is the cleanest first demonstration of git.agentic's eval-mode value: "compare two runs honestly" maps directly onto a tuple diff.

### Decision 2 — Integration is real-time and atomic, via a co-located sidecar daemon

The Executor depends on `agenticd` to ship in v1.0.
The daemon runs as a sidecar container in the same Cloud Run instance as the Coding worker; the worker speaks to it over a Unix domain socket using the same `agentic-proto` wire protocol the LangGraph integration uses.
[ADR-0004](0004-realtime-agenticd-for-executor.md) specifies the topology in detail — deployment shape, snapshot triggers, failure semantics, and the GCS-backed object store the sidecar requires.

What this commits to in v1.0:

- **Real-time per-checkpoint snapshots.** The sidecar records a Commit at session start, at every Claude Agent SDK checkpoint (typically per tool-call boundary), and at session end. Writes are durable through to a GCS-backed object store on each checkpoint, so they survive the Cloud Run instance scaling to zero.
- **Atomic rollback of in-flight sessions.** The daemon can signal pause/restore/resume to the worker, which honors them via the Agent SDK's checkpoint primitives. The rollback contract matches the LangGraph integration's — no "rollback-by-replay" caveat, no degraded subset of the product.
- **Hard runtime dependency.** Unlike the originally-considered manifest-export shape, the Coding worker cannot run without a reachable sidecar. If the sidecar is unavailable mid-session, the worker fails the ticket loudly (ADR-0004 Decision 4). There is no silent degraded mode.

This is the largest scope add to the 12-week MVP since the foundational ADRs.
It is justified because the alternative — manifest-export with rollback-by-replay — would ship the platform-led flagship integration with a degraded version of the product's core differentiator.
Atomic rollback *is* the differentiator; the Executor integration is its proof.
Shipping it with the differentiator removed muddies the message of every conversation we have with a subsequent platform integrator.

**Escape hatch.** If the sidecar and GCS-store work threaten the broken-prompt demo at any point, this Decision reverts to its originally-drafted manifest-export shape and atomic Executor defers to v1.1.
The hard decision point in the roadmap is **end of week 8**: if the GCS-backed `ObjectStore` does not pass integration tests by then, revert.
The demo discipline outranks the Executor integration; this trade is preserved even at the cost of the atomic story.

### Decision 3 — Integration surface is the framework-neutral SDK contract, not an Agent-SDK-specific adapter

The Executor consumes git.agentic exclusively through the SDK contract that v1.1 framework adapters will share.
There is no "Claude Agent SDK adapter" crate in v1.0 — only the manifest writer (which lives in the Executor's codebase, not ours) and the import CLI (which lives in `agentic-cli`).

This forces the SDK contract to be genuinely framework-neutral before v1.0 ships.
If the contract leaks LangGraph-specific assumptions, the Executor integration will surface those leaks before they are baked in.
That is the discipline this decision is supposed to enforce.

If the integration cannot be done through the framework-neutral SDK contract — i.e., if the Agent-SDK use case requires Commit-schema extensions or new SDK methods — those extensions must be made framework-neutral and apply to LangGraph too.
We do not ship framework-specific Commit fields.
Per ADR-0002 the Commit object is the platform API contract; framework-specific fields preclude the platform direction.

### Decision 4 — Memory dimension for Agent-SDK sessions is the message log, not pgvector

ADR-0001 Decision 4 names Postgres + pgvector as the only first-class memory backend in MVP.
The Executor's sessions do not use pgvector — their "memory" is the harness's conversation and tool-call history, which lives in process and is exported through the SDK's streamed events.

We do not amend Decision 4.
Instead, the `MemoryAdapter` trait in `crates/agentic-memory/src/adapter.rs` covers both cases: a content-addressed snapshot of *whatever the agent remembers between steps*, regardless of underlying store.
The Executor's manifest writer treats the Claude Agent SDK message log as the memory source; the existing Postgres adapter still serves LangGraph's pgvector.
Same trait, two implementations, one consistent Commit schema.

The pgvector-specific snapshot algorithm (`pg_dump --section`, logical decoding, segment manifests) remains the MVP's deep integration.
The Agent-SDK manifest path is comparatively shallow — it is essentially a structured log dump — and that asymmetry is honest about where each integration sits in terms of engineering depth.

## Consequences

**Positive:**

- The "framework-neutral SDK contract" claim from ADR-0001 Decision 7 gets tested against a second framework before v1.0 ships, not after. If the contract is wrong, we discover it on a friendly first integrator.
- The Executor integration reaches **parity with the LangGraph integration on atomic rollback**. The platform-led story is the full product, not a degraded subset.
- Codento has a concrete first-party integration to point at when talking to the next platform partner — one that demonstrates atomic, not one with a caveat.
- The Executor's harness × model matrix becomes a real-world stress test of `agentic diff` and `agentic rollback` against runs we did not architect for.
- The GCS-backed `ObjectStore` implementation (ADR-0004 Decision 5), originally anticipated as v2+ under ADR-0002 Decision 6, lands in v1.0 as a side-effect. Subsequent platform integrations inherit it without re-litigation.
- Resolves the in-repo strategic-tension flag (LangGraph-team MVP vs. platform-led GTM) constructively: the demo discipline runs on LangGraph; the platform-style integration runs on the Claude Agent SDK with the same product surface; both through the same SDK contract.

**Negative:**

- 12-week MVP scope grows substantially — the **largest scope add since the foundational ADRs**. The roadmap (`docs/product/roadmap.md`) must absorb a GCS-backed `ObjectStore`, sidecar packaging, integration tests against a real GCS bucket, and ongoing coordination with the Codento Executor team. The plan now has negative slack on the Executor track.
- The Executor is not yet built. We are committing to a real-time integration with a sidecar that does not exist on a development schedule we do not control. Coordination risk is structural and ongoing.
- We trade the "Executor's hot path stays free of any agentic-side runtime dependency" property of the original Decision 2 for the atomic contract. If the sidecar is broken, the Executor is broken. This is the right trade for the product story, but it raises the operational bar and the failure surface.

**Risks to revisit:**

- If the Claude Agent SDK's checkpoint primitives are not as ADR-0004 Decision 3 assumes (granularity, pause/restore support, timing), this Decision 2 may not be implementable as designed. Verify in **week 6**. Fallback is to revert to the originally-drafted manifest-export shape for v1.0 and reopen ADR-0004 for the next attempt.
- If the GCS-backed `ObjectStore` cannot pass integration tests by **end of week 8**, revert to manifest-export and defer atomic to v1.1. This decision point is hard, called out in the roadmap.
- If the SDK contract cannot accommodate the Agent-SDK use case without contortion, this ADR is wrong about "no framework-specific Commit fields" and a follow-up ADR (ADR-0005 or later) will be needed to amend the Commit schema. Better to find this in week 6 than week 11.
- If the Codento Executor team's harness work slips past the demo, the integration cannot be demonstrated even if our daemon side is ready. Fallback: a hand-rolled stub Cloud Run worker that exercises the wire protocol with synthetic Agent-SDK events, sufficient for a demo.

See also:
[ADR-0001](0001-architecture-foundations.md) (amended Decision 7),
[ADR-0002](0002-substrate-and-supercommit.md) (the Commit object as platform API contract, and Decision 6's swappable storage layer exercised by ADR-0004 Decision 5),
[ADR-0004](0004-realtime-agenticd-for-executor.md) (the sidecar topology that makes Decision 2 implementable),
and `codento-core/docs/EXECUTOR.md` (the Codento Executor design sketch this ADR responds to).
