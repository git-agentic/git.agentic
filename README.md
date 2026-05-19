# git.agentic

> **Git for agent behavior.** Atomic, reversible snapshots of the full system state that determines how an AI agent acts.

`git revert` knows about code. It doesn't know about prompts, tools, memory state, schema versions, or model versions — and modern AI agents are determined by all of those. When an agent regresses in production, `git revert` can't put the system back together.

`git.agentic` versions the **whole tuple**:

```
AgentVersion = (code_sha, prompts, tools, model, memory_snapshot, schema_version)
```

A commit captures all six dimensions atomically. A rollback restores all six coherently — including reverse schema migrations and memory state. It is the primitive that makes stateful agent systems operable.

## Status

**Pre-MVP, scaffolding phase.** Currently building toward a 12-week MVP targeting 2026-08-11. Nothing here is production-ready yet. See [`docs/product/roadmap.md`](docs/product/roadmap.md) for week-by-week progress and what's safe to depend on.

## Why this exists

Read [`docs/product/mvp-spec.md`](docs/product/mvp-spec.md) for the problem framing. The 30-second version:

A developer ships a "small" prompt tweak plus a memory schema change. The agent starts hallucinating. The on-call engineer reaches for `git revert`. It doesn't work — the schema is still bumped, the memory has accumulated contaminated rows, the tool versions have drifted. There is no `git revert` for the *system* that determines the agent's behavior.

We build it.

## What ships in the MVP

- **`agentic` CLI** — `init`, `commit`, `log`, `diff`, `rollback`, `branch`, `status`.
- **`agenticd` daemon** — Rust binary that owns the content-addressed object store and the snapshot/rollback engine.
- **Python SDK (`agentic-sdk`)** — typed client, plus a drop-in LangGraph checkpointer.
- **One memory backend:** Postgres + pgvector, deeply integrated.
- **One framework integration:** LangGraph.
- **One demo:** ["the broken prompt"](docs/product/demo-scenario.md).

Explicitly **not** in MVP scope: web UI, hosted SaaS, eval/CI/AE pipelines, MCP registry hosting, sandbox execution, A2A routing, more than one memory backend, more than one framework. See [`docs/adr/0001-architecture-foundations.md`](docs/adr/0001-architecture-foundations.md) §9–§10 for why.

## The demo (target week 11)

```bash
# baseline: agent works
./scripts/ask "I'm thinking about cancelling."
> "I understand. Could you tell me a bit more..."

# developer ships a "small" prompt + schema change
./scripts/deploy-bad-change.sh

./scripts/ask "I'm thinking about cancelling."
> "Absolutely! I'll cancel and refund the full amount. Done!"   # hallucinated

# git can't fix this — the schema is bumped, memory is contaminated
git revert HEAD && ./scripts/redeploy.sh
./scripts/ask "I'm thinking about cancelling."
> "Looking at your account, I see your refund processed yesterday..."   # still wrong

# the agentic way
agentic rollback v0.7
# ✓ Schema reverted          in 0.4s
# ✓ Memory restored          in 2.1s
# ✓ Prompts restored         in 0.0s
# ✓ HEAD now at i7j8k9l (rollback of v0.8 → v0.7)

./scripts/ask "I'm thinking about cancelling."
> "I understand. Could you tell me a bit more..."   # baseline restored
```

Full walkthrough in [`docs/product/demo-scenario.md`](docs/product/demo-scenario.md).

## Repository layout

```
git.agentic/
├── Cargo.toml                    Rust workspace
├── rust-toolchain.toml
├── crates/
│   ├── agentic-core/             content-addressed object store, snapshot model
│   ├── agentic-memory/           memory adapters (postgres + pgvector first)
│   ├── agentic-proto/            wire types for daemon ↔ SDK ↔ CLI
│   ├── agentic-cli/              the `agentic` binary
│   └── agenticd/                 the daemon
├── sdk/
│   └── python/                   `agentic-sdk` package + LangGraph integration
├── examples/
│   └── langgraph-rollback/       the "broken prompt" demo (placeholder)
├── docs/
│   ├── product/
│   │   ├── mvp-spec.md           what we ship and for whom
│   │   ├── roadmap.md            week-by-week plan to 2026-08-11
│   │   └── demo-scenario.md      the canonical demo
│   ├── adr/
│   │   └── 0001-architecture-foundations.md
│   └── architecture/
│       ├── overview.md           system diagram and component boundaries
│       └── snapshot-model.md     the technical heart
├── CONTRIBUTING.md
└── LICENSE                       Apache 2.0
```

## Building locally

Requirements: Rust 1.95+ (pinned via `rust-toolchain.toml`), Python 3.10+, Postgres 15+ with `pgvector`.

```bash
# Rust workspace
cargo check
cargo test
cargo run -p agentic-cli -- --help

# Python SDK
cd sdk/python
pip install -e ".[langgraph,dev]"
pytest
```

A `docker-compose.yml` lands in week 11 to bring up Postgres + agenticd + the demo agent in one command.

## Design partners

If you're running a stateful LangGraph agent in production and have been burned by a prompt or schema change that you couldn't cleanly revert, we want to talk. The MVP is being co-designed with three teams. Reach out to Toni (toni.bergholm@gmail.com).

## License

Apache 2.0. See [LICENSE](LICENSE).

## Further reading

- [`docs/product/mvp-spec.md`](docs/product/mvp-spec.md) — what we're building and why now
- [`docs/adr/0001-architecture-foundations.md`](docs/adr/0001-architecture-foundations.md) — the structural decisions
- [`docs/architecture/snapshot-model.md`](docs/architecture/snapshot-model.md) — the technical model
- [`docs/architecture/overview.md`](docs/architecture/overview.md) — the system view
- [`docs/product/roadmap.md`](docs/product/roadmap.md) — the 12-week plan
- [`docs/product/demo-scenario.md`](docs/product/demo-scenario.md) — the demo
