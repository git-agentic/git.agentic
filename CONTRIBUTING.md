# Contributing to git.agentic

`git.agentic` is in pre-MVP scaffolding (May–August 2026). The bar for the next twelve weeks is **demo-readiness**, not feature breadth. Contributions are welcome but scoped tightly.

## What we will merge right now

- Fixes to docs (typos, broken links, wrong claims).
- Test coverage for `agentic-core` (the object store and hash machinery).
- Performance benchmarks (criterion-based) on the snapshot path.
- Issue reports describing real stateful-agent rollback pain you've experienced.

## What we will defer until after the MVP ships

Anything outside the wedge defined in [`docs/product/mvp-spec.md`](docs/product/mvp-spec.md) — including:

- Additional memory backends (Mem0, Zep, Letta) — v1.1.
- Additional framework integrations (CrewAI, AutoGen, LlamaIndex) — v1.1.
- Web UI or dashboard — v1.1.
- SaaS / hosting work — post-seed.
- Evaluation pipelines, MCP registry hosting, sandbox execution — out of category.

Please read `docs/adr/0001-architecture-foundations.md` before proposing changes; that ADR is the load-bearing scope contract.

## Development setup

```bash
# Rust workspace (1.78+ required; pinned via rust-toolchain.toml)
cargo check
cargo test
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check

# Python SDK
cd sdk/python
pip install -e ".[langgraph,dev]"
pytest
ruff check .
mypy agentic
```

## Code style

- **Rust:** `rustfmt` defaults; `clippy` with `-D warnings`. No `unwrap()` in non-test code paths without a `// SAFETY:` or `// INVARIANT:` comment explaining why it cannot panic.
- **Python:** `ruff` for lint and format; `mypy --strict` for the public SDK surface. Type-hint everything in `agentic/`.
- **Docs:** Markdown, semantic line breaks. Every ADR has a stable numeric prefix and `Status:` line. New ADRs require an owner and a date.

## Commit messages

Plain prose, imperative mood. No conventional-commits ceremony in the MVP phase. Reference an issue number when applicable. Keep the subject under 70 chars; longer rationale belongs in the body.

## Pull requests

- Branch from `main`. Rebase, don't merge.
- Tests pass locally before pushing.
- One conceptual change per PR. Refactors and feature work go in separate PRs.
- The PR description should explain the *why*, not just the *what*. Reviewers care about the reasoning more than the diff.

## Decision records

For any change that affects the architecture — adding a new crate, changing the wire protocol, picking a new dependency that touches the daemon — open a new ADR under `docs/adr/`. Use the format established by ADR-0001. Submit it as a separate PR before the implementation lands so we can argue about the design without the code in the way.

## Reporting security issues

For anything that looks like a vulnerability (especially in the daemon's filesystem handling or in the secret-scanning logic), email Toni at toni.bergholm@gmail.com rather than opening a public issue.

## Code of conduct

Be kind. Disagree about ideas, not about people. The MVP is small enough that we will resolve any conflict by talking; if that fails, project lead has final say.
