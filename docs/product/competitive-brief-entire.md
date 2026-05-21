# Competitive brief — Entire CLI

**Status:** Draft v0.1
**Date:** 2026-05-21
**Owner:** Toni
**Subject:** [entireio/cli](https://github.com/entireio/cli) — "Entire CLI hooks into your Git workflow to capture AI agent sessions as you work."
**Adjacent reference:** [OpenWolf](https://openwolf.com) (`cytostack/openwolf`) — included for contrast; not a competitor.

This brief exists so we can answer "isn't this just Entire?" in one sentence in front of a design partner, an investor, or a hiring candidate, and can do it the same way every time.

## TL;DR

- **Entire captures *why code changed*.** A session log keyed to a commit: transcript, prompts, files, tokens, tool calls, stored on a separate `entire/checkpoints/v1` branch. Multi-agent (Claude Code, Codex, Gemini, OpenCode, Cursor, Factory AI Droid, Copilot CLI). MIT, Go, 4.3k stars, shipping since ~late 2025.
- **git.agentic rolls back *agent behavior*.** An atomic snapshot of the six-dimension tuple `(code + prompts + tools + model + memory + schema)`, restored in <5s including reverse SQL migrations, with the demo as discipline.
- **One-sentence answer:** *"Entire indexes the prompt history that explains a commit; git.agentic rewinds the agent state — memory, schema, model, prompts — that the commit alone can't put back together."*
- **OpenWolf** (`.wolf/` already in this repo) is not in this lane at all — it's hook-based middleware over Claude Code for token tracking, anatomy maps, and learned preferences. Complementary, not competitive.

## What Entire actually does

A `git enable`-style setup writes hooks for each named agent into that agent's settings file (`.claude/settings.json`, `.codex/hooks.json`, `.cursor/hooks.json`, `.gemini/settings.json`, `.opencode/plugins/entire.ts`, etc.). Each hook reports session events to `entire`. On `git commit`, the in-flight session is sealed into a **checkpoint** — a 12-char hex id — and persisted as session metadata on a *separate* branch `entire/checkpoints/v1`.

The user-facing surface is small and very legible:

- `entire status` — what session is current.
- `entire checkpoint rewind` — interactive picker; restores files to a previous checkpoint without rewriting commit history.
- `entire session resume <branch>` — switch to a branch and restore the latest checkpointed session metadata.
- `entire checkpoint explain` — explain a session/commit/checkpoint.
- `entire clean` / `entire doctor` — operational hygiene.

Two patterns underneath are well-judged and worth naming:

1. **Metadata branch isolation.** Nothing is ever written to the user's active branch. All session/checkpoint data lives on `entire/checkpoints/v1`. Git history stays clean. (We already do the equivalent via a content-addressed blob store under `.agentic/objects/`; the principle is the same — "agent state lives next to the code, not in it.")
2. **Checkpoint remote.** `entire enable --checkpoint-remote github:org/private-repo` pushes the metadata branch to a *separate* repo. The use case: OSS code repo is public, but the session transcripts contain proprietary prompts. `ENTIRE_CHECKPOINT_TOKEN` env var auths the checkpoint repo independently of `origin`. This is small and obviously right.

They also ship auto-summarization at commit time (intent, outcome, learnings, friction points, open items), generated via `claude` CLI, non-blocking. Best-effort secret redaction at write. Worktree-aware. Concurrent sessions on the same commit tracked separately.

## How that compares to our wedge

This is the load-bearing section. The categorical difference is *what gets restored on rewind*.

| Dimension | Entire `checkpoint rewind` | `agentic rollback` |
|---|---|---|
| Code (files) | Yes — restores files to checkpoint state | Yes — via Git SHA reference in the Commit object |
| Prompts | **No** — recorded as transcript metadata, not restored | Yes — content-addressed in the snapshot |
| Tools / MCP fingerprint | **No** | Yes — MCP manifest hash pinned in the Commit |
| Model version string | **No** | Yes — `provider:model:rev` pinned |
| Memory (pgvector contents) | **No** — out of scope | Yes — Postgres segment manifest restored row-for-row |
| Schema version (incl. reverse SQL migrations) | **No** | Yes — bounded for destructive migrations per [ADR-0002](../adr/0002-substrate-and-supercommit.md) Decision 5 |

The [broken-prompt demo](demo-scenario.md) is exactly the scenario Entire cannot solve. A "small prompt tweak" ships alongside a contaminated memory seed and a schema bump. `git revert` puts the code back. `entire checkpoint rewind` puts the *files* back (which subsumes code) and gives you the prompt transcript to read. Neither un-poisons pgvector and neither runs the reverse migration. `agentic rollback` does both, atomically, in one gesture.

This is the wedge. Don't let conversation drift away from it.

## Where we agree (and that's evidence the shape is right)

These are choices Entire and git.agentic *both* make, which is useful: when two independent projects converge on the same pattern under different goals, that's an externality signaling the pattern is load-bearing.

- **Agent metadata on a separate ref / store, never on the active branch.** Entire: `entire/checkpoints/v1`. Us: `.agentic/objects/` content-addressed store + refs under `.agentic/refs/`. Same principle.
- **No commits on the user's branch.** Entire: explicit non-goal. Us: same — `agentic commit` writes the Commit object to our store and updates `.agentic/refs/heads/<name>`, never `git commit`.
- **Manual-commit strategy.** Entire seals checkpoints on `git commit`. Our `AgenticSessionStore` ([ADR-0005](../adr/0005-sessionstore-amendment-to-adr-0004.md)) snapshots on `SessionStore.append` per turn or per frame, but the `agentic commit` gesture is also manual. Neither tool auto-commits the user's intent.
- **Best-effort secret redaction at write.** Entire's `redact/` package; our daemon's high-entropy scanner per `CLAUDE.md` §"What not to do". Both call it best-effort. That framing is the honest one.
- **Worktrees + concurrent sessions are first-class.** Both treat them as required, not nice-to-have.

The convergent evidence does *not* establish our wedge. It establishes that the table-stakes plumbing is settling into a convention. Useful for design partners who've used Entire — the metaphors map cleanly.

## What's worth borrowing outright

Three patterns ship cheap and add real value. We've moved each into a dedicated ADR or v1.1 workstream rather than leave them as orphan tickets.

### 1. `checkpoint_remote` — push agent state to a separate repo/bucket

The OSS-with-private-prompts use case is going to come up the first week we have an OSS design partner. Entire's shape (`provider:owner/repo` config + dedicated auth token + HTTPS coercion when token is set + graceful skip on unreachable remote) is the right shape. Adapted to our `ObjectStore` trait: a secondary backend for "non-code" objects (Segment, Commit, signatures), with the same selection per repo.

→ **ADR-0008 (Proposed)** [`docs/adr/0008-secondary-objectstore-for-agent-state.md`](../adr/0008-secondary-objectstore-for-agent-state.md). Extends [ADR-0006](../adr/0006-objectstore-backend-trait.md) Decision 5 (backend selection is config, not API).

### 2. Commit-time narrative summarization

Entire's auto-summary (intent / outcome / learnings / friction / open items) generated by `claude` at commit time, non-blocking, is small and high-leverage. We can slot it directly into the extended Commit object's `intent` and `evals` fields per [ADR-0002 Decision 2](../adr/0002-substrate-and-supercommit.md) — the field is already there, we just haven't been writing to it. Makes `agentic diff` and `agentic log` materially more useful at no protocol cost.

→ **ADR-0009 (Proposed)** [`docs/adr/0009-commit-time-narrative-summarization.md`](../adr/0009-commit-time-narrative-summarization.md). Slots into the extended Commit object directly, no schema change.

### 3. Multi-agent hook installer matrix

Entire ships hooks for seven agents and the install layout (`entire enable --agent <name>` → write to `.claude/settings.json` | `.codex/hooks.json` | `.cursor/hooks.json` | `.gemini/settings.json` | `.opencode/plugins/entire.ts` | `.factory/settings.json` | `.github/hooks/entire.json`) is the right shape. We don't need that breadth in v1.0 ([ADR-0003](../adr/0003-claude-agent-sdk-integration.md) commits us to LangGraph + Claude Agent SDK only), but the layout is what `agentic enable --agent <name>` should look like when CrewAI / AutoGen / Cursor / Gemini integrations land. Borrow the file-location matrix and CLI verb shape; don't borrow the implementation.

→ Tracked in [`v1.1-plan.md`](v1.1-plan.md) §Workstream 4 (new), gated on a second framework integration being design-partner-pulled.

## What we should *not* borrow

- **Their positioning.** "Searchable record of how code was written" is the audit/compliance/onboarding axis. That is not our axis and chasing it would dilute the wedge. Our axis is *recovery*, full stop. The demo is the discipline.
- **Their summarization-as-a-product-surface framing.** Their `entire checkpoint explain` is a first-class CLI command; the summary is a feature you go look at. For us the summary is *content of the Commit object*, retrievable via `agentic show <commit>` like any other field. We don't add a new top-level verb for it. (ADR-0009 enforces this.)
- **The breadth of agent hooks in v1.0.** Seven agents is great when shipping multi-agent capture is the product. For us it would be scope creep dressed as parity. v1.1, design-partner-pulled, behind the framework-neutral SDK contract per [ADR-0003 Decision 3](../adr/0003-claude-agent-sdk-integration.md).
- **Auto-summarization on every commit by default.** Entire defaults to off (`strategy_options.summarize.enabled: false`). We should do the same — opt-in per-repo, honest about the LLM call cost and the transcript-egress privacy implication. ADR-0009 §Decision 4.

## OpenWolf (for the record, since it was bundled with the ask)

OpenWolf is *middleware over Claude Code*, not a competitor. Its scope: six lifecycle hooks (`SessionStart`, `PreToolUse` × 2, `PostToolUse` × 2, `Stop`), pure Node.js file I/O, no network. Outputs are markdown-as-source-of-truth: `anatomy.md` (token-cost map of the repo), `cerebrum.md` (learnings + Do-Not-Repeat list), `memory.md` (action log), `buglog.json` (searchable bug-fix memory). Already in this repo per `.claude/rules/openwolf.md`; CLAUDE.md mandates the read-check-update discipline at every session.

The only thing in OpenWolf adjacent to our work is the **anatomy map** pattern — a per-file index with token estimates and one-line descriptions, generated on write. We do not need to ingest that pattern, but it's a useful reference for if we ever want `agentic log` to show per-commit "what changed and roughly how much" without paying for a full diff render. v1.2+ at the earliest.

## Threats Entire poses (and don't pose)

**Real threats:**

- **Mindshare in the "git for agent context" lane.** 4.3k stars and growing means when a developer searches for the category, they find Entire first. Our `git.agentic` name and tuple framing fight this directly, but mindshare is the long game. Mitigation: ship the demo, get the three design partners writing about the recovery use case in public, get a clear category-defining post out by v1.0 ship (`docs/product/launch-narrative.md` candidate).
- **Multi-agent footprint.** A team that's already using Entire for Codex + Cursor + Claude Code transcripts has invested in their hook infrastructure. Our LangGraph + Executor v1.0 starts narrower. Mitigation: don't compete on breadth; lead with the wedge. The first time a team's pgvector gets poisoned, they will install both.

**Not threats:**

- **Their session-capture surface.** They're not building toward rollback of the six-dimension tuple. Their checkpoint rewind is file-restore; extending it to memory/schema is an architectural pivot, not an incremental feature. We have a ~12–18 month lead at the architecture layer.
- **MIT vs. Apache 2.0.** Both are commercial-friendly OSS; neither blocks the other. Not a decision input.
- **Go vs. Rust.** Implementation-language differences don't affect what either tool *does*. Not a positioning input.

## Action items

1. [x] ADR-0008 drafted: secondary `ObjectStore` for agent state (the `checkpoint_remote` pattern, adapted).
2. [x] ADR-0009 drafted: commit-time narrative summarization into the extended Commit object's `intent`/`evals` fields.
3. [x] `v1.1-plan.md` extended with Workstream 4 (multi-agent hook installer matrix), gated on design-partner pull.
4. [ ] Add the one-sentence answer to `docs/product/design-partners.md` outreach brief so every cold conversation starts from the same framing. v1.0 sprint scope.
5. [ ] Add a "How is this different from Entire?" section to the v1.0 README, lifted from this doc's TL;DR. v1.0 sprint scope.
6. [ ] Watch their release cadence (currently nightly + stable, releases monthly). If they ship a memory-restore primitive, this brief gets a Status update and a sprint check-in. Calendar reminder: 2026-07-15.

## Prior art / sources

- [entireio/cli](https://github.com/entireio/cli) — README + commands reference (fetched 2026-05-21).
- [entire.io](https://entire.io) — product site.
- [OpenWolf](https://openwolf.com) — for contrast; not a competitor.
- Internal: [ADR-0002](../adr/0002-substrate-and-supercommit.md) (extended Commit object as platform API), [ADR-0003](../adr/0003-claude-agent-sdk-integration.md) (framework-neutral SDK contract), [ADR-0006](../adr/0006-objectstore-backend-trait.md) (ObjectStore trait), [`demo-scenario.md`](demo-scenario.md) (the discipline).
