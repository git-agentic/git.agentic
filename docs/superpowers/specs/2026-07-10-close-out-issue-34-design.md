# Design: Verify & close tracking issue #34

**Date:** 2026-07-10
**Status:** Approved
**Scope:** Close-out of GitHub issue #34 — "agenticd architectural-analysis follow-ups (2026-05-21)"

## Goal

Close tracking issue #34 with a durable, evidence-backed record that all 11
architectural recommendations (A1–A11) from the 2026-05-21 agenticd
architectural analysis landed on `main` as specified — not merely that their
child issues were closed — and sync the audit doc's per-section done markers.

## Background

Issue #34 tracks 11 recommendations across three tiers, each with a child
issue (#35–#45). All child issues are closed, each via a merged PR:

| Tier | Item | Issue | PR |
|---|---|---|---|
| must-fix-v1.0 | A1 quiesce trigger poller during restore | #35 | #48 |
| must-fix-v1.0 | A2 lifecycle module: SIGTERM drain + ref reconciliation | #36 | #50 |
| must-fix-v1.0 | A8 reverse-migration outer tx + restore guard + `accept_data_loss` | #37 | #46 |
| hardening-sprint | A3 `commit.rs` orchestrator, named 2PC phases | #38 | #52 |
| hardening-sprint | A4 rollback split: `mod`/`loaders`/`writeback` | #39 | #51 |
| hardening-sprint | A5 `spawn_blocking` around GCS I/O (tactical) | #40 | #55 |
| hardening-sprint | A6 structured `Response::Error` + framing envelope | #41 | #74 (ADR-0010 via #73) |
| hardening-sprint | A7 parallelise MCP fingerprinting | #42 | #54 |
| v1.1 | A9 complete `MemoryAdapter` trait | #43 | #81 + #98 (ADR-0005 via #78) |
| v1.1 | A10 `SegmentManifest::from_canonical_bytes` | #44 | #75 |
| v1.1 | A11 diff atomicity via `Refs::snapshot` | #45 | #77 (ADR-0007 via #76) |

Deferred system-level concerns landed separately via ADR-0010 + ADR-0011
(#33, merged). A12 is intentionally-not-addressed per the audit.

Audit doc: `docs/ops/2026-05-21-agenticd-architectural-analysis.md`.

## Verification (read-only, main checkout)

Fan out 3 parallel read-only Explore agents, one per tier (must-fix,
hardening-sprint, v1.1). Each agent receives the audit doc's relevant
A-sections as its spec and returns, per item:

- **Verdict:** landed / partial / missing
- **Evidence:** `file:line` on `main` for the key artifact the audit asked for
- **The merging PR**

Evidence standard: the artifact must exist in code on `main`, not merely be
described in a PR description.

## Audit-doc marker sync (the only repo write)

Read the audit doc. If any A-section lacks the done/resolution marker
convention (§A8's marker shows the pattern), add the missing markers in this
worktree (`.worktrees/close-out-issue-34/`), one small PR, plain-prose
commit. If all markers are already present, this step is a no-op and no PR
is opened.

## Close-out mechanics (GitHub only)

1. Edit #34's body: tick all 11 checkboxes.
2. Post a closing comment containing:
   - evidence table (item → PR → key artifact `file:line` → verdict),
   - note that deferred system-level work landed via ADR-0010/0011 (#33),
   - note that A12 was intentionally-not-addressed per the audit.
3. Close #34 as completed.

The user approves the closing-comment text before it is posted
(outward-facing write).

## Error handling

If any item comes back **partial** or **missing**: do *not* close #34.
Post the evidence table showing the gap and propose either a new child
issue or re-opening the original — the user decides at that point. A
verdict downgrade beats a false "all done".

## Verification of the close-out itself

After closing: confirm #34 shows closed/completed, the checkboxes render
checked, and — if an audit-doc PR was opened — it is green on CI
(Markdown-only change; standard checks).

## Out of scope

- Any new architectural analysis of `agenticd` (a follow-up round is a
  separate project).
- Re-litigating any A-item's design; the child issues and their ADRs are
  authoritative.
- Code changes of any kind beyond audit-doc markers.
