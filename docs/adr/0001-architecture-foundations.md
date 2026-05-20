# ADR-0001: Architectural Foundations

**Status:** Accepted
**Date:** 2026-05-19
**Deciders:** Toni
**Supersedes:** —
**Amendments:** Decision 7 amended by [ADR-0003](0003-codento-executor-integration.md) — MVP framework support extended to LangGraph + Claude Agent SDK (Codento Executor), via a layered/offline session-manifest path on the framework-neutral SDK contract.

## Context

We are building `git.agentic`, a tool whose core primitive is the atomic snapshot and rollback of an AI agent's behavioral state. This ADR establishes the load-bearing architectural decisions for the MVP. Every subsequent ADR will assume these.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **The version unit is a tuple, not a SHA.** `AgentVersion = (code_sha, prompts, tools, model, memory_snapshot, schema_version)`. | Code-only versioning cannot reproduce behavior. |
| 2 | **Content-addressed object store, modeled on Git's object database.** | Cheap snapshots, structural sharing, well-understood semantics. |
| 3 | **Rust for the core engine and CLI; Python for the SDK.** | Rust for systems work and binary distribution; Python because that's where agent devs live. |
| 4 | **Postgres + pgvector as the only first-class memory backend in MVP.** | One DB = atomic snapshots are tractable. Adapters for Mem0/Zep/Letta come later. |
| 5 | **We do not replace Git for code.** We delegate code versioning to Git and reference Git SHAs inside our snapshots. | Don't fight a battle we'd lose; Git is excellent at what it does. |
| 6 | **MCP is the tool-versioning standard.** Tool fingerprints are hashes of MCP manifests pinned to a server version. | MCP has clear momentum and a real schema; rolling our own would be unforced error. |
| 7 | **LangGraph is the first and only integration in MVP.** Other frameworks via the same SDK contract in v1.1. | Best enterprise adoption; explicit state-machine model maps cleanly onto our snapshot model. |
| 8 | **Apache 2.0 license.** | Matches OSS norms (Mem0, Letta, LangGraph). Permits commercial offering without forking. |
| 9 | **CLI-first; no web UI in MVP.** | Our users live in terminals and Python notebooks. UI is a v1.1 lever for distribution, not for product. |
| 10 | **Self-hosted Docker compose; no SaaS in MVP.** | Lower trust threshold for design partners. SaaS comes after seed. |

---

## Decision 1 — The tuple, not the SHA

The fundamental observation from the blueprint is correct: agent behavior is determined by more than code. Reproducing or rolling back behavior requires reproducing or rolling back **all** of the inputs that determine it. We define:

```
AgentVersion = {
  code_sha:         <git-sha>            # delegated to Git
  prompts:          <content-hash>       # all prompt templates + system instructions
  tools:            <content-hash>       # MCP manifest hashes, pinned versions
  model:            <provider:model:rev> # e.g. "anthropic:claude-opus:2026-05-01"
  memory_snapshot:  <snapshot-id>        # point-in-time memory state
  schema_version:   <semver>             # memory schema version
}
```

This tuple is the single unit of identity. Everything in the system — commits, branches, rollbacks, diffs, reproducibility guarantees — operates over the tuple, not over any individual dimension. Tooling that treats prompts or memory as "configuration" rather than as first-class versioned artifacts is the anti-pattern we're correcting.

**Alternatives considered:**
- *Version each dimension separately and reconcile at runtime.* Rejected: this is what teams do today and it's exactly what doesn't work. The whole point is atomicity.
- *Version only prompts + tools + memory; ignore model.* Rejected: silent provider-side model updates have caused real production regressions. We must at least capture the version string.

## Decision 2 — Content-addressed object store

We mimic Git's object database: blobs, trees, and commits, all identified by the SHA-256 of their canonical serialization. A commit object references the tuple's six dimensions; large memory snapshots are stored as Merkle DAGs so that incremental snapshots share structure with their parents.

**Why content addressing:**
- Deduplication. A 10M-row vector table changed in 100 rows produces a snapshot that's 100 rows + a delta tree, not 10M rows.
- Integrity. Tampering with a stored object changes its hash. Useful later for signing/SLSA.
- Mental model. Engineers already understand Git's object model; reusing it lowers cognitive load.

**Alternatives considered:**
- *Use Postgres as the entire backing store.* Rejected: ties the system to one DB, and Postgres is not optimized for content-addressed blob storage.
- *Use OSTree or Git itself as the store.* Tempting; Git's object format is mature. Rejected for MVP because we need first-class support for very large binary blobs (vector indexes) and for streaming snapshots, which Git is not optimized for. We may revisit and adopt Git's wire protocol for replication later.

## Decision 3 — Rust core, Python SDK

The core engine — object store, snapshot/restore, schema-compat checks, content-addressing, the CLI — is in Rust. The user-facing SDK is Python.

**Why Rust for the core:**
- Snapshot/restore performance matters. We're aiming for <2s snapshot on 1M-row tables; this is achievable in Rust, fragile in Python.
- Single static binary distribution. `curl | sh` works.
- Memory safety in a tool that handles user data is non-negotiable.
- The ecosystem is real: `tokio`, `sqlx`, `rkyv`, `blake3`, `clap`, `pgvector-rs`, `pyo3` for bindings.

**Why Python for the SDK:**
- Every agent framework worth integrating with is Python-first. LangGraph, CrewAI, AutoGen, LlamaIndex, Letta, Mem0 — all Python.
- The SDK is small surface: a few classes and decorators. We don't lose much by not having it in Rust.

The Python SDK calls into the Rust core via `pyo3` bindings for performance-sensitive paths, and otherwise speaks to the local daemon (`agenticd`) over a Unix socket / named pipe. A `agentic.commit()` call from Python materializes the tuple in-process, ships it to the daemon, and the daemon writes it to the object store.

**Alternatives considered:**
- *Go.* Faster to write, real ecosystem (Dolt, LakeFS exist in Go). Rejected because (a) Rust's perf and FFI story are stronger for this specific workload, (b) the Python-bindings story (pyo3) is more mature than Go's, (c) it's a positioning signal: Rust says "we take infra seriously."
- *All-Python.* Rejected on performance grounds and because pip-only distribution makes ops nervous.
- *TypeScript/Node.* Rejected; wrong audience and wrong tool for snapshots.

## Decision 4 — Postgres + pgvector as the only first-class memory backend in MVP

We support exactly one memory backend at MVP: Postgres with the pgvector extension. The integration is deep, the snapshot algorithm uses Postgres-specific features (logical replication, MVCC visibility, `pg_dump --section`), and we recommend it to design partners.

**Why one backend:**
- Snapshots that span multiple stores (e.g., "snapshot my Mem0 and my pgvector together") are a distributed-systems problem we don't need to solve in MVP. One store = one transaction boundary = atomicity is tractable.
- Postgres + pgvector is the most common starting point for teams building stateful agents.
- It's also the easiest to demo on a laptop.

Adapters for Mem0, Zep/Graphiti, and Letta come in v1.1, with the explicit caveat that snapshots become *eventually consistent* rather than strictly atomic when crossing store boundaries. We document this honestly.

**Alternatives considered:**
- *Backend-agnostic from day one.* Rejected: the leakiest abstraction in the system would be the snapshot model, and pretending all backends are equivalent is the trap we want to avoid.
- *Lead with Mem0/Zep because they're the buzzy ones.* Rejected: their internal storage is harder to snapshot and their teams move fast; we'd be chasing.

## Decision 5 — Don't replace Git for code

Code is versioned by Git. Our commits carry a `code_sha` field pointing at a Git commit. Our CLI shells out to `git` for the code dimension and never tries to be its replacement.

This is a positioning decision as much as a technical one. The "GitHub for the Agentic Era" framing in the original blueprint is a distraction. We don't compete with GitHub at hosting Git. We complement Git by versioning everything Git doesn't.

## Decision 6 — MCP for tool versioning

We adopt the Model Context Protocol as the canonical representation of tools. A tool's fingerprint in our tuple is the BLAKE3 hash of the MCP server's manifest (the JSON-RPC `tools/list` response, canonicalized), plus the MCP server's own version string. We require design partners to pin MCP servers to immutable versions (commit SHA or container digest).

This means: if you change a tool's behavior, we notice. If your MCP server upgrades silently, we notice. If you can't pin your tools, the platform won't be able to fully reproduce — and we tell you so loudly.

## Decision 7 — LangGraph first

LangGraph is our beachhead framework integration. Its node/edge state-machine model maps onto our snapshot model cleanly: a snapshot can be triggered on every graph compile and on every `invoke` boundary, and the existing `Checkpointer` interface is the right shape to extend.

We ship `agentic.langgraph.AgenticCheckpointer` as a drop-in replacement for the default Postgres checkpointer. From the user's perspective, they swap one line of code and now every graph execution is fully snapshotted.

Other frameworks land in v1.1 via the same SDK contract.

## Decision 8 — Apache 2.0

License is Apache 2.0. The agentic-infrastructure norm is permissive OSS (Mem0, Letta, Graphiti, LangChain, LangGraph). MIT is also defensible but Apache's explicit patent grant is friendlier to enterprise adoption.

We may dual-license a future SaaS / enterprise edition (AGPL + commercial) but the core engine stays Apache.

## Decision 9 — CLI-first

The MVP user surface is the `agentic` CLI plus the Python SDK. No web UI. No dashboard. No "platform."

This is deliberate. Design partners trust a CLI. A half-built web UI in a pre-seed pitch makes the product look smaller, not bigger. We add a minimal dashboard in v1.1 when we have actual usage to display.

## Decision 10 — Self-hosted Docker compose

For MVP we ship a `docker-compose.yml` that brings up the daemon, an embedded object store, and a Postgres instance (or connects to the user's). No SaaS, no auth, no multi-tenant. Design partners run it on their own infra. We help them.

SaaS comes after seed funding and after we know what design partners actually want from a hosted offering.

---

## Consequences

**Positive:**
- Tight scope. Twelve-week MVP is plausible.
- Clear positioning vs. adjacent categories (eval, memory, MCP registry).
- Defensible technical wedge: nobody else snapshots the tuple atomically.

**Negative:**
- Postgres-only is a real limitation for design partners on Mem0/Zep. We accept this and lean on a strong roadmap.
- Rust + Python is a polyglot codebase from day one; raises the bar for contributors.
- No UI means we're invisible to anyone evaluating us via a web demo.

**Risks to revisit:**
- If pgvector copy-on-write proves too expensive at scale, we may need a different snapshot mechanism (LSM-style segment files, or a sidecar object store).
- If LangGraph's `Checkpointer` interface changes upstream, our integration breaks. Pin against a known LangGraph version.
- If MCP adoption stalls, our tool-versioning story weakens. Mitigation: also support direct OpenAPI manifest hashes as a fallback.

See [snapshot-model.md](../architecture/snapshot-model.md) for the technical model and [overview.md](../architecture/overview.md) for the system view.
