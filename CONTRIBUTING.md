# Contributing to git.agentic

`git.agentic` went public 2026-05-22 with the v1.0 MVP code on `main`. The hardening sprint closed faster than expected; what remains is the verification half — published performance numbers against `snapshot-model.md` §9, a fresh-machine cold-start timing for the broken-prompt demo, and partner-environment validation. The bar is still **demo-readiness**, not feature breadth — contributions are welcome but scoped tightly.

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
# Rust workspace (1.95+ required; pinned via rust-toolchain.toml)
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
  `Status:` records decision state only (Proposed/Accepted), never implementation state;
  an ADR that `Closes:` a threat-model row gains a `Closed in: <PR/commit> (<date>)` line when the control lands,
  and the threat-model row points back at the ADR.

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

See [`SECURITY.md`](SECURITY.md) — please do not open public issues for vulnerabilities.

## Code of conduct

This project follows the Contributor Covenant 2.1. See [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md). Report violations to toni@git-agentic.com.
