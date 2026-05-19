# anatomy.md

> Auto-maintained by OpenWolf. Last scanned: 2026-05-19T14:30:19.350Z
> Files: 47 tracked | Anatomy hits: 0 | Misses: 0

## ../../../.claude/plans/

- `read-the-project-folder-s-functional-lerdorf.md` — git.agentic — 12-Week MVP Build Plan (~2809 tok)

## ./

- `.DS_Store` (~1640 tok)
- `.gitignore` — Git ignore rules (~74 tok)
- `Cargo.toml` — Rust package manifest (~406 tok)
- `CLAUDE.md` — Project standing context for AI assistants: identity, phase, authoritative ADRs (0001 + 0002), strategic-tension flag (LangGraph-team MVP vs. platform-led long arc), layout, build commands, anti-patterns, demo discipline, OpenWolf rules (~2200 tok)
- `CONTRIBUTING.md` — Contributing to git.agentic (~782 tok)
- `LICENSE` — Project license (~3020 tok)
- `README.md` — Project documentation (~1428 tok)
- `rust-toolchain.toml` (~25 tok)

## .claude/

- `settings.json` (~441 tok)

## .claude/rules/

- `openwolf.md` (~313 tok)

## .github/workflows/

- `ci.yml` — CI: ci (~216 tok)

## crates/agentic-cli/

- `Cargo.toml` — Rust package manifest (~151 tok)

## crates/agentic-cli/src/

- `client.rs` — Unix-socket client for the `agentic` CLI. (~678 tok)
- `main.rs` — The `agentic` command-line interface. (~2266 tok)

## crates/agentic-core/

- `Cargo.toml` — Rust package manifest (~156 tok)

## crates/agentic-core/src/

- `commit.rs` — Two-phase commit staging — the load-bearing plumbing. (~2812 tok)
- `hash.rs` — BLAKE3-based content addresses. (~838 tok)
- `lib.rs` — agentic-core (~394 tok)
- `object.rs` — The four object kinds: Blob, Tree, Segment (placeholder), Commit. (~1805 tok)
- `refs.rs` — Branch refs and `HEAD`, written atomically. (~1614 tok)
- `store.rs` — On-disk content-addressed object store. (~1262 tok)

## crates/agentic-memory/

- `Cargo.toml` — Rust package manifest (~155 tok)

## crates/agentic-memory/src/

- `adapter.rs` — The trait every memory backend implements. (~482 tok)
- `lib.rs` — agentic-memory (~330 tok)
- `postgres.rs` — Postgres + pgvector adapter — the MVP's only first-class memory backend. (~580 tok)
- `segment.rs` — Segment objects: content-addressed, immutable chunks of memory rows. (~501 tok)

## crates/agentic-proto/

- `Cargo.toml` — Rust package manifest (~97 tok)

## crates/agentic-proto/src/

- `framing.rs` — Length-prefixed JSON framing over an async byte stream. (~817 tok)
- `lib.rs` — agentic-proto (~882 tok)

## crates/agenticd/

- `Cargo.toml` — Rust package manifest (~158 tok)

## crates/agenticd/src/

- `main.rs` — agenticd — the git.agentic daemon. (~594 tok)
- `server.rs` — The daemon's per-connection request dispatcher. (~1579 tok)

## docs/adr/

- `0001-architecture-foundations.md` — ADR-0001: Architectural Foundations (~2897 tok)
- `0002-substrate-and-supercommit.md` — ADR-0002: Substrate Approach — Git Core, Content-Addressed Manifest, Coordinator-Mediated Two-Phase Commit (~3400 tok)

## docs/architecture/

- `overview.md` — Architecture Overview (~3108 tok)
- `snapshot-model.md` — The Snapshot Model (~3326 tok)

## docs/product/

- `demo-scenario.md` — The Demo: "The Broken Prompt" (~1818 tok)
- `mvp-spec.md` — git.agentic — MVP Product Spec (~1595 tok)
- `roadmap.md` — 12-Week MVP Roadmap (~2536 tok)

## examples/langgraph-rollback/

- `README.md` — Project documentation (~211 tok)

## sdk/python/

- `pyproject.toml` — Python project configuration (~218 tok)
- `README.md` — Project documentation (~156 tok)

## sdk/python/agentic/

- `__init__.py` — agentic — the Python SDK for git.agentic. (~551 tok)
- `client.py` — The client that talks to `agenticd` over a Unix domain socket. (~559 tok)
- `langgraph.py` — LangGraph integration: drop-in checkpointer that commits the agent (~392 tok)
- `types.py` — Typed data classes mirroring the daemon's wire protocol. (~210 tok)
