# git.agentic — MVP Product Spec

**Status:** Draft v0.1
**Last updated:** 2026-05-21
**Owner:** Toni
**Ship target:** 2026-05-26 with the repo going public (pulled forward from the originally-planned 2026-08-11). Design-partner onboarding moves post-launch; full v1.0 scope preserved.

---

## One-line pitch

**`git.agentic` is Git for agent behavior — atomic, reversible snapshots of the full system state that determines how an AI agent acts.**

## The problem we solve

When an AI agent system degrades in production, you cannot roll it back. Engineers reach for `git revert` and discover that the regression isn't in the code:

- A prompt was tweaked yesterday and is now hallucinating.
- A memory schema migration changed how embeddings are stored and old entries no longer match.
- An MCP tool server upgraded its API contract.
- The underlying foundation model silently changed (provider-side update).
- Episodic memory has accumulated since the bad change — even if you revert code, the agent's "experience" is now contaminated.

A real production incident has six versioning surfaces — code, prompts, tools, model, memory state, schema — and `git` only knows about one of them. The on-call engineer's options today are:

1. Revert code and pray. *(Usually doesn't fix it.)*
2. Hand-write a memory migration backward. *(Hours to days; risky.)*
3. Wipe memory and rehydrate from logs. *(Loses user context; angry customers.)*
4. Spin up a parallel agent on the prior config and cut traffic over. *(Doubles cost; doesn't fix the contaminated memory state.)*

There is no `git revert` for behavior. We build it.

## Who this is for (ICP)

**Primary ICP (MVP design partners):** Teams of 2–15 engineers running a stateful agent system in production that has burned them at least once. They are likely using LangGraph or an equivalent and have a memory layer (Postgres + pgvector, Mem0, Zep, Letta, or a hand-rolled graph). They feel the rollback pain weekly and have an internal Notion doc titled something like "Things to do before re-deploying the agent."

**Disqualifiers for MVP:**
- Pure chatbot teams with no persistent memory — they don't have the problem yet.
- Enterprise procurement-first buyers — too long a cycle for pre-seed.
- "Coding agent" companies (Cursor, Cognition class) — they have their own infra and aren't going to adopt ours.

## The wedge

We do one thing better than anyone: **commit, branch, and roll back the full agent state tuple atomically.**

The "agent state tuple" is:

```
AgentVersion = (code_sha, prompts, tools, model, memory_snapshot, schema_version)
```

A `git.agentic` commit content-addresses all six dimensions into one immutable object. A `git.agentic rollback` restores all six coherently. Everything else in the agentic-infrastructure space — evaluation, observability, MCP registries, sandboxes — is downstream of this primitive and we will not build it in the MVP.

## MVP scope (what ships in 12 weeks)

### In scope

1. **CLI tool `agentic`** — `init`, `commit`, `log`, `checkout`, `rollback`, `diff`, `branch`.
2. **Rust core engine** — content-addressed object store, snapshot algorithm, rollback algorithm, schema-compat checks.
3. **Python SDK** — `agentic.commit(...)`, `agentic.rollback(version)`, context-manager hooks for capturing prompts/tools/model.
4. **One memory backend adapter: Postgres + pgvector.** Copy-on-write snapshots via Postgres logical replication slots and content-addressed embedding tables. Single backend, deep integration.
5. **One framework integration: LangGraph.** A `git.agentic` checkpointer that captures the full tuple on every graph compile and every `invoke()`.
6. **One demo: "the broken prompt".** End-to-end scripted scenario: working agent → engineer ships a bad prompt + a memory schema change → production degrades → one CLI command restores the entire system, memory included, and the agent recovers.
7. **Docs:** quickstart, conceptual model, one design partner runbook.

### Explicitly out of scope (v2+)

- Web UI / dashboard. CLI + Python only for MVP.
- Multi-tenant SaaS. Self-hosted Docker compose; we deploy it for design partners.
- Eval / CI/AE pipelines. Out — that's LangSmith/Braintrust territory and not the wedge.
- MCP registry hosting. We *consume* MCP for tool versioning; we don't host servers.
- Sandbox execution. Use E2B / Modal / customer's own infra.
- Code versioning replacement. We *use* Git underneath for code; we don't reinvent it.
- More than one memory backend. Mem0/Zep adapters in v1.1.
- More than one framework. CrewAI/AutoGen in v1.1.
- A2A protocol routing. Defer.
- IAM, RBAC, audit logs. Defer to enterprise (post-seed).

If a feature isn't in the demo path, it doesn't ship in MVP.

## What success looks like

**At ship (2026-05-26):**

- **Technical:** The demo runs reliably on a fresh laptop in <5 minutes from `git clone`. Snapshot < 2s on a 1M-row pgvector table. Rollback < 5s end-to-end on the same.
- **Narrative:** A blog post / video showing the broken-prompt demo that explains the wedge clearly enough that a hostile-but-fair YC partner gets it in under 90 seconds.

**At eight weeks post-launch (2026-07-21):**

- **Product:** Three design partners have run `agentic rollback` against their own staging environments and reported at least one "would have saved hours" moment.
- **Kill criteria:** If after eight weeks zero design partners are using it weekly, we abandon the wedge and reassess. If two or more are, we raise.

*The 12-week MVP scope above is preserved in full; design-partner onboarding (originally Week 12) is sequenced after the public release rather than before it.*

## Non-goals for the seed round

This product does not need to be:
- A complete agentic-development platform.
- Better than GitHub at hosting code.
- A multi-cloud control plane.
- A "memory database."

It needs to be the smallest thing that makes a real engineering team say: *"I cannot run an agent in production without this."*

## Open questions

These need resolution but are not blockers to scaffolding:

- **Q1:** How aggressively do we copy-on-write embeddings? Cheap snapshots vs. storage cost.
- **Q2:** Do we capture model weights for self-hosted models, or only the version string for hosted models? *(MVP answer: version string only.)*
- **Q3:** Tool schema fingerprinting — JSON-schema hash, or call out to the live MCP server and hash the manifest? *(Lean: manifest hash; pin to a specific MCP commit.)*
- **Q4:** Garbage collection — when does an old snapshot get pruned? *(Lean: never, in MVP. Storage is cheap and design partners want forensic depth.)*

See [ADR-0001](../adr/0001-architecture-foundations.md) for the structural decisions and [snapshot-model.md](../architecture/snapshot-model.md) for the technical model.
