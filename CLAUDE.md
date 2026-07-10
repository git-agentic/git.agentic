# OpenWolf

@.wolf/OPENWOLF.md

This project uses OpenWolf for context management. Read and follow .wolf/OPENWOLF.md every session. Check .wolf/cerebrum.md before generating code. Check .wolf/anatomy.md before reading files.


# CLAUDE.md — git.agentic

> **Identity.** `git.agentic` is "Git for agent behavior" — atomic, reversible snapshots of the full `(code + prompts + tools + model + memory + schema)` tuple that determines how an AI agent acts. `git revert` knows about code; we version everything else that determines behavior, and roll it back coherently.

This file is the standing context for any AI assistant working in this repo. Read it before doing anything substantive. When a referenced doc and this file disagree, the doc wins and this file needs updating.

## Phase

**MVP code complete on `main`; repo went public 2026-05-22.** The original 2026-05-26 ship target was pulled forward from a planned 2026-08-11 and then again pulled in by four days when the hardening sprint closed faster than expected. Full v1.0 scope is preserved; design-partner onboarding (originally roadmap Week 12) is post-launch. As of 2026-05-22 the implementations for roadmap weeks 1–11 have all landed: object store, atomic memory snapshot, rollback (incl. reverse migrations), MCP fingerprinting, six-dimension diff, Python SDK + LangGraph checkpointer, and the broken-prompt demo (`examples/langgraph-rollback/scripts/run-demo.sh`). Remaining work is verification + outreach, not new features — the 2026-05-20 hardening sprint that drove the verification push is fully executed and archived at [`docs/archive/sprint-2026-05-20.md`](docs/archive/sprint-2026-05-20.md); [`docs/architecture/benchmarks.md`](docs/architecture/benchmarks.md) has early performance numbers. The "< 5 min from `git clone`" demo claim is CI-verified as of 2026-07-10: the `demo` CI job runs `run-demo.sh` end-to-end on a fresh ubuntu runner in under 2 minutes (with cached cargo dependencies; a fully cold cache adds build time but has ample headroom in the 5-minute budget). The MVP target is a single named demo — ["the broken prompt"](docs/product/demo-scenario.md) — running reliably from `git clone` to working rollback in under 5 minutes. Every design decision must trace back to making that demo crisp.

If a feature, abstraction, or dependency is not on the path to the demo, it does not ship in MVP. See [`docs/product/mvp-spec.md`](docs/product/mvp-spec.md) §"Explicitly out of scope" — the boundary is hard.

## Authoritative decisions

Three ADRs govern the architecture. Read all three before designing anything substantial:

- [`docs/adr/0001-architecture-foundations.md`](docs/adr/0001-architecture-foundations.md) — the 10 foundational decisions: tuple-as-version, content-addressed store, Rust core + Python SDK split, Postgres+pgvector only, LangGraph + Claude Agent SDK (per ADR-0003), Apache 2.0, CLI-first, self-hosted Docker compose.
- [`docs/adr/0002-substrate-and-supercommit.md`](docs/adr/0002-substrate-and-supercommit.md) — Approach C (Git core + content-addressed blob store + coordinator), the extended Commit object as the platform API contract, the mandatory 2PC staging order, bounded rollback for destructive migrations.
- [`docs/adr/0003-claude-agent-sdk-integration.md`](docs/adr/0003-claude-agent-sdk-integration.md) — the first platform-partner integration as the first non-LangGraph integration in v1.0, with **atomic real-time integration** via a sidecar `agenticd` (topology specified in ADR-0004). Hard runtime dependency, full product parity with LangGraph on rollback. Amends ADR-0001 Decision 7.

[`docs/architecture/snapshot-model.md`](docs/architecture/snapshot-model.md) is the technical heart — object model, segment-based snapshot algorithm, rollback semantics, performance targets. [`docs/architecture/overview.md`](docs/architecture/overview.md) is the runtime topology and component boundaries.

## Strategic tension to be aware of

The in-repo MVP spec (May 2026) targets **stateful LangGraph teams of 2–15 engineers on Postgres+pgvector** as design partners and **explicitly disqualifies "coding agent" companies (Cursor, Cognition class)** on the grounds that they have their own infrastructure.

Recent strategy work (also May 2026) shifted the long-arc positioning toward **"the git host built for when most commits are written by agents,"** with a **platform-led GTM toward the very agent platforms** the MVP spec disqualifies. ADR-0002's extended Commit object is the substrate-level commitment to that direction (Commit object IS the platform API contract).

ADR-0003 reconciles these by accepting the first platform-partner integration (Claude Agent SDK) as the first platform-led integration alongside the LangGraph MVP work — at **full product parity, including atomic rollback**. The broken-prompt demo discipline still runs on LangGraph; the Executor integration is real-time via a sidecar `agenticd` per [ADR-0004](docs/adr/0004-realtime-agenticd-for-executor.md), which also pulls the GCS-backed `ObjectStore` forward from v2+ (exercising ADR-0002 Decision 6's swappable storage). If the sidecar/GCS work threatens the broken-prompt demo, the documented escape hatch (ADR-0003 Decision 2, end-of-week-8 decision point) is to revert to a layered manifest-export shape for v1.0 and defer atomic to v1.1. When working on MVP-path code, default to the in-repo spec. When making decisions that lock in long-term API or substrate (object schemas, daemon protocol, SDK surface), prefer choices that don't preclude the platform-led direction — in particular the SDK contract must stay framework-neutral per ADR-0003 Decision 3 (no framework-specific Commit fields). If a decision genuinely splits between the two framings, flag it rather than picking silently.

## Repository layout

```
crates/
  agentic-core/     content-addressed object store, snapshot model, hash machinery
  agentic-memory/   memory adapters (Postgres+pgvector first; trait-based for v1.1)
  agentic-proto/    wire types for daemon ↔ SDK ↔ CLI
  agentic-cli/      the `agentic` binary
  agenticd/         the daemon (single binary, tokio, one commit at a time)
sdk/python/         `agentic-sdk` package + LangGraph integration
examples/           the broken-prompt demo (langgraph-rollback/, incl. run-demo.sh)
docs/
  product/          mvp-spec, roadmap, demo-scenario
  adr/              accepted architectural decision records
  architecture/     overview + snapshot-model
```

`crates/agentic-core/src/object.rs` defines the four object kinds (Blob, Tree, Segment, Commit). **The `Commit` struct is the load-bearing data type** — per ADR-0002 it is also the platform API contract. Extend with care; never break wire compatibility without a new ADR.

## Build, test, run

```bash
# Rust workspace (toolchain pinned to 1.95 via rust-toolchain.toml)
cargo check
cargo test
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo run -p agentic-cli -- --help

# Python SDK
cd sdk/python
pip install -e ".[langgraph,dev]"
pytest
ruff check .
mypy agentic
```

`examples/langgraph-rollback/docker-compose.yml` brings up Postgres + pgvector for the demo on port 54322. `agenticd` and the Python agent are started by `scripts/run-demo.sh` (built locally from the Rust workspace, not containerised).

## Worktree discipline

Any time you are about to modify, create, or delete files in this repo you MUST do the work in a git worktree under `.worktrees/<slug>/`, never directly in the main checkout. Toni edits this repo in parallel on the same machine, so `HEAD` in the main checkout can move under an agent at any moment — see [[feedback_check_git_state_before_amend]]. Worktrees isolate agent edits from that movement, keep each conceptual change on its own branch, and make abandoned attempts cheap to discard.

How to apply:

- Create with `git worktree add .worktrees/<slug> -b <slug> <base-branch>`. Base is usually `main`; use the existing branch name (without `-b`) when continuing prior work. The slug is kebab-case and descriptive: `.worktrees/a8-reverse-migrations/`, `.worktrees/fix-clippy-warnings/`, `.worktrees/adr-0006-draft/`.
- Run `cargo`, `pytest`, `ruff`, `mypy`, and `scripts/run-demo.sh` from inside the worktree. Build artifacts (`target/`, `.venv/`, `.agentic/`) belong to the worktree, not the main checkout.
- When the branch is merged or abandoned, tear it down: `git worktree remove .worktrees/<slug>` and `git branch -D <slug>` if the branch is no longer needed.
- Read-only operations may run in the main checkout. The rule is about *writes* — `git log`, `git diff`, `grep`, `cargo check` for inspection, and reading files are all fine in the main checkout.
- Exceptions, narrowly scoped: trivial single-line edits to `CLAUDE.md` itself or to ADR `Status:` lines may be made in the main checkout, because they are metadata maintenance and not code/doc work that warrants its own branch.

`.worktrees/` is in `.gitignore` so worktree paths never leak into a parent tree.

## Code style

- **Rust.** `rustfmt` defaults; `clippy` with `-D warnings`. No `unwrap()` in non-test code without a `// SAFETY:` or `// INVARIANT:` comment explaining why it cannot panic. `thiserror` for library crates, `anyhow` for binary crates.
- **Python.** `ruff` lint + format; `mypy --strict` on the public SDK surface. Type-hint everything in `agentic/`.
- **Docs.** Markdown with semantic line breaks. Every ADR has a numeric prefix, a `Status:` line, an owner, and a date.
- **Commits.** Plain prose, imperative mood. No conventional-commits ceremony in the MVP phase. One conceptual change per PR; refactors and feature work go in separate PRs. PR descriptions explain the *why*, not just the *what*.

## What not to do

- **Don't expand scope past the demo path.** If it isn't required for the broken-prompt demo, it's v1.1+ work. The deferral list in [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`docs/product/mvp-spec.md`](docs/product/mvp-spec.md) is the authoritative scope boundary.
- **Don't add memory backends in MVP.** Postgres + pgvector only. Mem0 / Zep / Letta adapters are v1.1, behind the same `MemoryAdapter` trait.
- **Don't add framework integrations in MVP beyond LangGraph and the Claude Agent SDK (the first platform-partner integration), per [ADR-0003](docs/adr/0003-claude-agent-sdk-integration.md).** CrewAI / AutoGen / LlamaIndex are v1.1, behind the same SDK contract. The Executor integrates via a sidecar `agenticd` co-located with the Coding worker in its Cloud Run instance per [ADR-0004](docs/adr/0004-realtime-agenticd-for-executor.md), over the existing Unix-socket `agentic-proto` wire — through the framework-neutral SDK contract, never via a framework-specific adapter crate. Remote or in-process daemon deployments are explicitly out of scope for v1.0; if the sidecar path proves unworkable mid-MVP, the documented fallback is layered manifest-export (ADR-0003 Decision 2's original framing), not a different deployment shape.
- **Don't mock Postgres for snapshot/restore tests.** The snapshot algorithm uses Postgres-specific features (logical decoding, advisory locks, pgvector storage); mocking defeats the test. Snapshot/restore code paths must exercise a real Postgres+pgvector via CI integration tests.
- **Don't reorder the 2PC staging in commit code.** Per ADR-0002 Decision 3: blobs to object store → collect content hashes → build Commit blob → Git push (single commit point) → branch ref update. Failure-injection tests are required at each boundary. This is the plumbing that makes "atomic rollback" honest rather than aspirational.
- **Don't expose storage-layer concepts to platform integrators.** Per ADR-0002 Decision 6, the SDK's public surface trades in `Commit` objects only. No Git ref names, no object store paths, no internal segment IDs in public types — those preclude the v2+ storage swap.
- **Don't add a web UI.** CLI-first is a deliberate ADR-0001 Decision 9. v1.1 distribution lever, not MVP product.
- **Don't commit secrets.** [ADR-0013](docs/adr/0013-secret-scanner.md) specifies a `put_raw`-time pattern + entropy scanner that hard-rejects matched blobs with a typed `SecretDetected` error. The scanner is implemented and enforced — do not bypass it; fix the input or add a blob-hash allowlist entry per ADR-0013 Decision 4. One sanctioned relaxation exists: blobs under declared checkpoint-path prefixes (default `__langgraph__/`, flag `--scanner-exempt-entropy-prefix`) skip **only the entropy heuristic** per [ADR-0017](docs/adr/0017-entropy-exemption-for-checkpoint-paths.md) — pattern rules always run. Do not widen that exemption without amending the ADR.
- **Don't edit files in the main checkout.** Use a worktree under `.worktrees/<slug>/` — see "Worktree discipline" above. Toni works in parallel on the same machine, so `HEAD` can move under you mid-edit.

## The demo is the discipline

Every design decision in the MVP serves [the broken-prompt demo](docs/product/demo-scenario.md). When stuck on a tradeoff, ask: *does this make the demo crisper, faster, or more honest?* If not, defer.

What the demo requires:

- A clean `git clone` + `docker-compose up` produces a working agent in under 5 minutes.
- A scripted "bad change" reliably breaks the agent in a way that `git revert` alone cannot fix (prompt drift + memory contamination + schema bump).
- `agentic diff` makes the multi-dimensional regression visible at a glance.
- `agentic rollback` restores all six tuple dimensions atomically, including reverse schema migrations, in under 5s end-to-end on the target hardware.

Performance targets (from [`snapshot-model.md`](docs/architecture/snapshot-model.md) §9): commit < 2s, rollback < 5s, diff < 1s, write overhead < 5ms p99, snapshot storage < 2× changed data amortized. These are commitments, not aspirations.

## Agent skills

### Issue tracker

Issues are tracked in GitHub Issues (`git-agentic/git.agentic`) via the `gh` CLI; external PRs are **not** a triage surface. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix` (the last already exists in the repo). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` (not yet written) + `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Canonical references

When in doubt, these are authoritative:

- [`docs/product/mvp-spec.md`](docs/product/mvp-spec.md) — what we ship, for whom, what's out
- [`docs/product/roadmap.md`](docs/product/roadmap.md) — 12-week build narrative; repo went public 2026-05-22 (pulled in from the original 2026-05-26 ship target)
- [`docs/product/demo-scenario.md`](docs/product/demo-scenario.md) — the demo as discipline
- [`docs/adr/0001-architecture-foundations.md`](docs/adr/0001-architecture-foundations.md) — foundational decisions (Decision 7 amended by ADR-0003)
- [`docs/adr/0002-substrate-and-supercommit.md`](docs/adr/0002-substrate-and-supercommit.md) — substrate, extended Commit, 2PC staging order
- [`docs/adr/0003-claude-agent-sdk-integration.md`](docs/adr/0003-claude-agent-sdk-integration.md) — first non-LangGraph integration target (the first platform-partner integration / Claude Agent SDK), layered session-manifest path
- [`docs/adr/0004-realtime-agenticd-for-executor.md`](docs/adr/0004-realtime-agenticd-for-executor.md) — sidecar `agenticd` topology for the Executor's real-time atomic integration: deployment shape, snapshot triggers, failure semantics, GCS-backed `ObjectStore` (Decisions 3 and 4 amended by ADR-0005)
- [`docs/adr/0005-sessionstore-amendment-to-adr-0004.md`](docs/adr/0005-sessionstore-amendment-to-adr-0004.md) — Accepted amendment (2026-05-22): snapshot primitive is the Claude Agent SDK's `SessionStore.append`; loud-fail preserved via a synchronising `PreToolUse` hook
- [`docs/adr/`](docs/adr/) — the full accepted set runs through ADR-0017; notably [ADR-0013](docs/adr/0013-secret-scanner.md) (secret scanner), [ADR-0016](docs/adr/0016-mcp-url-policy.md) (MCP URL policy), and [ADR-0017](docs/adr/0017-entropy-exemption-for-checkpoint-paths.md) (checkpoint-path entropy exemption)
- [`docs/architecture/overview.md`](docs/architecture/overview.md) — runtime topology
- [`docs/architecture/snapshot-model.md`](docs/architecture/snapshot-model.md) — the technical heart
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — what's accepted right now, code style, ADR process

For anything that affects the architecture (new crate, wire-protocol change, new dependency in the daemon), open a new ADR under `docs/adr/` using ADR-0001's format. Submit it before the implementation so the design can be argued without the code in the way.
