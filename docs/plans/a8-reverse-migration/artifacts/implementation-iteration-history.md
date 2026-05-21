# Implementation iteration history — A8

Tracks the round-by-round findings for `feature-implementation-plan.md`. Companion to `implementation-decision-log.md`.

## R1 — Round 1 (test-engineer + junior-developer, parallel)

**Date:** 2026-05-21
**Specialists engaged:** `test-engineer`, `junior-developer`
**Briefs used:** domain-scoped per `references/plan-implementation` rules. Both agents read `.discovery-notes.md` and `source.md` first; not re-grepped.
**Spec inputs:** GH issue #37 + audit §A8 / B8 / B9 / B10 / R4 / R5 + source.md Q1/Q2/Q3.

### New input provided

- **test-engineer**: produced 4 test slices (AC1 integration; AC3a/AC3b unit; AC3c integration smoke), 7 YAGNI candidates, 1 hard-blocker (AC2 ↔ Q2), 1 specialist handoff request (software-architect for Q2).
- **junior-developer**: produced 10 generalist findings (J1-J10), reframed Q1/Q2/Q3 in plain language, found 1 critical assumption-refuted (J1, audit pseudocode breaks because of connection pool separation).

### Claim ledger

#### Category: `assumption-refuted`
- **L1** — Audit §A8 pseudocode `conn.begin() / apply_down(&mut tx, m) / tx.commit()` does NOT deliver atomicity. `apply_down_migration` at `postgres.rs:483-484` calls `self.pool.begin()` — a separate Postgres session. Outer-tx rollback has zero effect on inner committed sessions.
  - State: **Evidenced** (cites `postgres.rs:483-484`, `migrate.rs:104-108`).
  - Spec-maturity: **plan-level** (resolvable by picking single-executor threading mechanic).
  - Raised by: `junior-developer` (J1).
  - Corroborated by: `test-engineer` (TE notes AC1 fixture is Q1-agnostic — but the literal audit pseudocode would fail AC1).

#### Category: `mechanic-leak`
- **L2** — `accept_data_loss` bypass must thread into `load_steps`/`check_irreversible`, not into `run_reverse`. Current discard at `rollback.rs:216` is downstream of where the IRREVERSIBLE check fires (`migrate.rs:79`). `load_steps` signature must change to accept the flag.
  - State: **Evidenced** (`rollback.rs:216`, `migrate.rs:51-88`).
  - Spec-maturity: **plan-level**.
  - Raised by: `junior-developer` (J2), corroborated by `test-engineer` (AC3a unit test forces the signature change).

- **L3** — Q2 option (i) — `SnapshotHandle.schema_version: Option<String>` — has trait-contract blast radius. Changes the `MemoryAdapter::snapshot/restore` interface. Affects every present and future backend.
  - State: **Evidenced** (`adapter.rs:17`).
  - Spec-maturity: **plan-level** (architectural assessment).
  - Raised by: `junior-developer` (J4).

#### Category: `overlap`
- **L4** — B9 implementation structurally depends on Q2 resolution. The `if let Some(ref target_schema)` block at `rollback.rs:86-157` must be split, but the post-split memory-restore branch needs a `SnapshotHandle.schema_version` value that Q2 has not decided.
  - State: **Evidenced** (`rollback.rs:86-157`, `rollback.rs:140-143`).
  - Spec-maturity: **plan-level** but **blocked pending user input on Q2**.
  - Raised by: `test-engineer` (AC2 hard-blocker) AND `junior-developer` (J5).

- **L5** — After B9 fix, forward-record path at `rollback.rs:200-215` will copy `target.schema_version=None` and `target.memory_snapshot=Some` into the new rollback Commit. The B9 edge state propagates forward in history.
  - State: **Anecdotal** (derived consequence; not directly proven).
  - Spec-maturity: **plan-level**.
  - Raised by: `junior-developer` (J8). Out-of-scope for A8 fix per audit recommendation focus.

- **L6** — Demo end-to-end smoke (`examples/langgraph-rollback/scripts/run-demo.sh`) should be added to A8 pre-flight checklist. CLAUDE.md "demo is the discipline" mandates it; A8 touches the demo's critical path.
  - State: **Evidenced** (CLAUDE.md "demo is the discipline").
  - Spec-maturity: **plan-level** (checklist addition).
  - Raised by: `junior-developer` (J9).

#### Category: `ambiguity`
- **L7** — `migrate.rs:21-22` docstring claims `accept_data_loss` is "reserved for the bounded-rollback v1.1 path". After A8 commits to meaning (α), the docstring is stale and misleading.
  - State: **Evidenced** (`migrate.rs:20-22`).
  - Spec-maturity: **plan-level** (docstring update in PR scope).
  - Raised by: `junior-developer` (J3).

- **L8** — AC1 fixture convention (deliberately-failing mid-sequence `.down.sql`) does not exist. `crates/agenticd/tests/` does not exist either. Convention will be invented by A8; future tests may diverge.
  - State: **Evidenced** (`.discovery-notes.md` "Gaps").
  - Spec-maturity: **plan-level**.
  - Raised by: `junior-developer` (J6), corroborated by `test-engineer` (TE: inline helpers per test file are sufficient; no shared DSL needed).

- **L9** — AC3 test split between unit (`load_steps` accepts flag, returns Ok for IRREVERSIBLE) and integration (actual DDL execution end-to-end) needs to be explicit. CLAUDE.md "no Postgres mocking" reinforces.
  - State: **Evidenced** (`migrate.rs:128-148`, CLAUDE.md).
  - Spec-maturity: **plan-level**.
  - Raised by: `junior-developer` (J7), corroborated by `test-engineer` (AC3a/AC3b/AC3c split).

- **L10** — Plan output requirement says "resolve Q1/Q2/Q3" but Q2 in `source.md` declines to recommend. Either name a decision-maker or escalate.
  - State: **Evidenced** (`source.md#q2`).
  - Spec-maturity: **plan-level** requiring **user escalation**.
  - Raised by: `junior-developer` (J10), corroborated by `test-engineer` (Q2 specialist handoff).

#### Category: `YAGNI-candidate`
Seven YAGNI candidates from `test-engineer`. All deferrals with named reopen triggers. Synthesised in §7.5 sweep.

- **L11** — Tests for Q3(β) bounded-rollback. Reopen on v1.1 ADR-0002 D5 implementation.
- **L12** — Tests for Q3(γ) hybrid (α)+(β). Reopen if user chooses (γ).
- **L13** — Tests for Q2 options not chosen. Reopen if unchosen option ever chosen.
- **L14** — Tests for `rollback::execute` non-A8 code paths (prompt sweep, forward-record, tools/model). Reopen on bug found / PR modifying them.
- **L15** — Tests for `schema_version=Some, memory_snapshot=None` path. Reopen on bug.
- **L16** — Golden-file/snapshot tests for error messages. Replace with substring assertions (simpler version).
- **L17** — Shared test-fixture DSL beyond A8 needs. Reopen on third use case.
- **L18** — `accept_data_loss=false` integration smoke duplicate. Reopen if propagation refactored.

### Open Questions raised this round

- **OQ-1** — Q2 resolution: `SnapshotHandle.schema_version` when target has none. Four options (i/ii/iii/iv) in `source.md§q2`. **Status: needs user input.** Recommendation drafted in `feature-implementation-plan.md`.
- **OQ-2** — Q1 sqlx-executor-threading mechanic. **Status: plan-level decision** — go with single-executor option (a) per J1 critique; surface in plan as committed decision.
- **OQ-3** — `--accept-data-loss` UX affordance (audit log, warning prompt). **Status: plan-level YAGNI defer** with reopen trigger. Do not escalate.
- **OQ-4** — Should commit-write code prevent producing Commits in B9 state? **Status: out of A8 scope.** Recommendation: file a follow-up issue if needed.
- **OQ-5** — Should `scripts/run-demo.sh` smoke be in A8 pre-flight checklist? **Status: plan-level decision = yes** per CLAUDE.md "demo is the discipline". Do not escalate.

### Spec-maturity tag summary

- `T#`-contradictions: 0 (no `feature-technical-notes.md` exists for this plan; `T#`-contradiction category does not apply).
- `spec-level` findings: 0.
- **Spec-maturity gate: NOT tripped.**

### Resolution source per question

- **OQ-1**: resolved by **user input** on 2026-05-21. User selected option **(iii) reject as malformed**. Recorded as `D-1` in `implementation-decision-log.md`. Side-effect: J8 (forward-record propagation) is moot — rejection means no forward-record Commit is produced for the buggy state. Side-effect: AC2 wording in issue #37 needs update (`D-9`) — literal AC was "performs memory restore," committed behavior is "rejects malformed Commit."
- **OQ-2**: resolved by **evidence** (J1's structural critique forces option (a) with single-executor threading).
- **OQ-3**: resolved by **YAGNI evidence test** (no upstream finding requires v1.0 affordance).
- **OQ-4**: resolved by **scope test** (out of A8 per audit §A8 focus).
- **OQ-5**: resolved by **evidence** (CLAUDE.md mandate).

### Specialist handoff requests

- `test-engineer` requested `software-architect` for Q2 resolution. **Routing**: small classification, round cap 1 → escalate Q2 to user (OQ-1) instead of running a second round.
- `junior-developer` recommended `behavioral-analyst` for L5 (forward-record propagation) and `user-experience-designer` for OQ-3 (--accept-data-loss UX). **Routing**: both are out-of-A8-scope or YAGNI; not engaged in this PR.

### Deterministic next-step recommendation

**"Blocked pending user input"** on OQ-1 (Q2). All other Open Questions resolved by evidence, YAGNI rule, or scope. After OQ-1 answer, proceed to Step 7.5 (YAGNI sweep) and Step 8 (PM synthesis).

### Decisions produced

- **D-1** (full) — Q2 resolution = option (iii) reject malformed. User-decided.
- **D-2** (full) — Q1 mechanic = single-executor threading via `apply_down_migration_tx`. Driven by J1.
- **D-3** (full) — Q3 semantics = option (α) bypass IRREVERSIBLE when `accept_data_loss=true`. Per AC3.
- **D-4** (trivial) — AC1 test file location.
- **D-5** (trivial) — AC3 test split (3a/3b/3c).
- **D-6** (trivial) — `migrate.rs` docstring update in PR scope.
- **D-7** (trivial) — Demo smoke in pre-flight.
- **D-8** (full) — TDD test ordering.
- **D-9** (trivial) — Issue #37 AC2 wording update.

### Changed in plan

- `## Outcome` — committed behavior for all three defects.
- `## Implementation Approach` — three subsections (B8, B9, B10) with concrete code shapes.
- `## Decomposition and Sequencing` — 11-step TDD-ordered sequence.
- `## Testing Strategy` — explicit test levels + Postgres mandate.
- `## Definition of Done` — 10 acceptance gates.
- `## Deferred (YAGNI)` — 11 items with reopen triggers.
- `## Open Items` — 3 housekeeping items (issue #37 wording, accessor API choice, dead-method cleanup).
