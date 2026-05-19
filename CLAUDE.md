# CLAUDE.md — git.agentic

> **Identity.** `git.agentic` is "Git for agent behavior" — atomic, reversible snapshots of the full `(code + prompts + tools + model + memory + schema)` tuple that determines how an AI agent acts. `git revert` knows about code; we version everything else that determines behavior, and roll it back coherently.

This file is the standing context for any AI assistant working in this repo. Read it before doing anything substantive. When a referenced doc and this file disagree, the doc wins and this file needs updating.

## Phase

**Pre-MVP scaffolding. 12-week build to 2026-08-11.** Currently early in the build. The MVP target is a single named demo — ["the broken prompt"](docs/product/demo-scenario.md) — running reliably from `git clone` to working rollback in under 5 minutes. Every design decision must trace back to making that demo crisp.

If a feature, abstraction, or dependency is not on the path to the demo, it does not ship in MVP. See [`docs/product/mvp-spec.md`](docs/product/mvp-spec.md) §"Explicitly out of scope" — the boundary is hard.

## Authoritative decisions

Two ADRs govern the architecture. Read both before designing anything substantial:

- [`docs/adr/0001-architecture-foundations.md`](docs/adr/0001-architecture-foundations.md) — the 10 foundational decisions: tuple-as-version, content-addressed store, Rust core + Python SDK split, Postgres+pgvector only, LangGraph only, Apache 2.0, CLI-first, self-hosted Docker compose.
- [`docs/adr/0002-substrate-and-supercommit.md`](docs/adr/0002-substrate-and-supercommit.md) — Approach C (Git core + content-addressed blob store + coordinator), the extended Commit object as the platform API contract, the mandatory 2PC staging order, bounded rollback for destructive migrations.

[`docs/architecture/snapshot-model.md`](docs/architecture/snapshot-model.md) is the technical heart — object model, segment-based snapshot algorithm, rollback semantics, performance targets. [`docs/architecture/overview.md`](docs/architecture/overview.md) is the runtime topology and component boundaries.

## Strategic tension to be aware of

The in-repo MVP spec (May 2026) targets **stateful LangGraph teams of 2–15 engineers on Postgres+pgvector** as design partners and **explicitly disqualifies "coding agent" companies (Cursor, Cognition class)** on the grounds that they have their own infrastructure.

Recent strategy work (also May 2026) shifted the long-arc positioning toward **"the git host built for when most commits are written by agents,"** with a **platform-led GTM toward the very agent platforms** the MVP spec disqualifies. ADR-0002's extended Commit object is the substrate-level commitment to that direction (Commit object IS the platform API contract).

These two framings are not yet reconciled. The 12-week build remains anchored on the LangGraph-team ICP and the broken-prompt demo. When working on MVP-path code, default to the in-repo spec. When making decisions that lock in long-term API or substrate (object schemas, daemon protocol, SDK surface), prefer choices that don't preclude the platform-led direction — for example, the storage layer must stay swappable per ADR-0002 Decision 6. If a decision genuinely splits between the two framings, flag it rather than picking silently.

## Repository layout

```
crates/
  agentic-core/     content-addressed object store, snapshot model, hash machinery
  agentic-memory/   memory adapters (Postgres+pgvector first; trait-based for v1.1)
  agentic-proto/    wire types for daemon ↔ SDK ↔ CLI
  agentic-cli/      the `agentic` binary
  agenticd/         the daemon (single binary, tokio, one commit at a time)
sdk/python/         `agentic-sdk` package + LangGraph integration
examples/           demo agent code (placeholder until ~week 9)
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

`docker-compose.yml` for the full Postgres + agenticd + demo-agent stack lands in week 11 of the roadmap.

## Code style

- **Rust.** `rustfmt` defaults; `clippy` with `-D warnings`. No `unwrap()` in non-test code without a `// SAFETY:` or `// INVARIANT:` comment explaining why it cannot panic. `thiserror` for library crates, `anyhow` for binary crates.
- **Python.** `ruff` lint + format; `mypy --strict` on the public SDK surface. Type-hint everything in `agentic/`.
- **Docs.** Markdown with semantic line breaks. Every ADR has a numeric prefix, a `Status:` line, an owner, and a date.
- **Commits.** Plain prose, imperative mood. No conventional-commits ceremony in the MVP phase. One conceptual change per PR; refactors and feature work go in separate PRs. PR descriptions explain the *why*, not just the *what*.

## What not to do

- **Don't expand scope past the demo path.** If it isn't required for the broken-prompt demo, it's v1.1+ work. The deferral list in [`CONTRIBUTING.md`](CONTRIBUTING.md) and [`docs/product/mvp-spec.md`](docs/product/mvp-spec.md) is the authoritative scope boundary.
- **Don't add memory backends in MVP.** Postgres + pgvector only. Mem0 / Zep / Letta adapters are v1.1, behind the same `MemoryAdapter` trait.
- **Don't add framework integrations in MVP.** LangGraph only. CrewAI / AutoGen / LlamaIndex are v1.1, behind the same SDK contract.
- **Don't mock Postgres for snapshot/restore tests.** The snapshot algorithm uses Postgres-specific features (logical decoding, advisory locks, pgvector storage); mocking defeats the test. Snapshot/restore code paths must exercise a real Postgres+pgvector via CI integration tests.
- **Don't reorder the 2PC staging in commit code.** Per ADR-0002 Decision 3: blobs to object store → collect content hashes → build Commit blob → Git push (single commit point) → branch ref update. Failure-injection tests are required at each boundary. This is the plumbing that makes "atomic rollback" honest rather than aspirational.
- **Don't expose storage-layer concepts to platform integrators.** Per ADR-0002 Decision 6, the SDK's public surface trades in `Commit` objects only. No Git ref names, no object store paths, no internal segment IDs in public types — those preclude the v2+ storage swap.
- **Don't add a web UI.** CLI-first is a deliberate ADR-0001 Decision 9. v1.1 distribution lever, not MVP product.
- **Don't commit secrets.** The daemon scans every blob it would write for high-entropy strings matching common token patterns and refuses to write them. Don't bypass the scanner; fix the input.

## The demo is the discipline

Every design decision in the MVP serves [the broken-prompt demo](docs/product/demo-scenario.md). When stuck on a tradeoff, ask: *does this make the demo crisper, faster, or more honest?* If not, defer.

What the demo requires:

- A clean `git clone` + `docker-compose up` produces a working agent in under 5 minutes.
- A scripted "bad change" reliably breaks the agent in a way that `git revert` alone cannot fix (prompt drift + memory contamination + schema bump).
- `agentic diff` makes the multi-dimensional regression visible at a glance.
- `agentic rollback` restores all six tuple dimensions atomically, including reverse schema migrations, in under 5s end-to-end on the target hardware.

Performance targets (from [`snapshot-model.md`](docs/architecture/snapshot-model.md) §9): commit < 2s, rollback < 5s, diff < 1s, write overhead < 5ms p99, snapshot storage < 2× changed data amortized. These are commitments, not aspirations.

## OpenWolf workflow

This project uses OpenWolf for context management. The standing rules apply to every session:

- Read [`.wolf/OPENWOLF.md`](.wolf/OPENWOLF.md) at session start.
- Check [`.wolf/anatomy.md`](.wolf/anatomy.md) before reading project files (it's the token-cost map of the repo).
- Check [`.wolf/cerebrum.md`](.wolf/cerebrum.md) Decision Log and Do-Not-Repeat list before generating code.
- Before fixing any bug / error / failed test / failed build, read [`.wolf/buglog.json`](.wolf/buglog.json) for known fixes.
- After writing or editing files, update `.wolf/anatomy.md` and append to `.wolf/memory.md`.
- After fixing any bug, log it to `.wolf/buglog.json` with `error_message`, `root_cause`, `fix`, and `tags`.
- After receiving a user correction, update `.wolf/cerebrum.md` immediately (Preferences, Learnings, or Do-Not-Repeat).
- If you edit the same file more than twice in a session, that likely indicates a bug — log it to `.wolf/buglog.json`.

@.wolf/OPENWOLF.md

## Canonical references

When in doubt, these are authoritative:

- [`docs/product/mvp-spec.md`](docs/product/mvp-spec.md) — what we ship, for whom, what's out
- [`docs/product/roadmap.md`](docs/product/roadmap.md) — week-by-week plan to 2026-08-11
- [`docs/product/demo-scenario.md`](docs/product/demo-scenario.md) — the demo as discipline
- [`docs/adr/0001-architecture-foundations.md`](docs/adr/0001-architecture-foundations.md) — foundational decisions
- [`docs/adr/0002-substrate-and-supercommit.md`](docs/adr/0002-substrate-and-supercommit.md) — substrate, extended Commit, 2PC staging order
- [`docs/architecture/overview.md`](docs/architecture/overview.md) — runtime topology
- [`docs/architecture/snapshot-model.md`](docs/architecture/snapshot-model.md) — the technical heart
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — what's accepted right now, code style, ADR process

For anything that affects the architecture (new crate, wire-protocol change, new dependency in the daemon), open a new ADR under `docs/adr/` using ADR-0001's format. Submit it before the implementation so the design can be argued without the code in the way.
