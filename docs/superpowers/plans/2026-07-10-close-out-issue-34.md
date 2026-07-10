# Close Out Tracking Issue #34 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close GitHub issue #34 with code-grounded evidence that all 11 audit recommendations (A1–A11) landed on `main`, after syncing the audit doc's one stale status marker.

**Architecture:** Read-only verification fans out to 3 parallel Explore agents (one per priority tier), each checking code on `main` against the audit doc's A-sections. A gate task synthesizes verdicts: any `partial`/`missing` verdict halts the close-out and reports to the user instead. The only repo write is a one-line marker fix in the audit doc (worktree `.worktrees/close-out-issue-34/`, branch `close-out-issue-34`). GitHub writes (body edit, closing comment, close) happen only after the gate passes and the user approves the comment text.

**Tech Stack:** `gh` CLI, git worktree, Explore subagents. No code compilation required (Markdown-only repo change).

## Global Constraints

- Worktree discipline: all repo file writes happen in `.worktrees/close-out-issue-34/`; the main checkout is read-only (per CLAUDE.md).
- Evidence standard (from spec): an item counts as **landed** only if the artifact exists in code on `main` — a merged PR description alone is insufficient.
- Error handling (from spec): any **partial** or **missing** verdict → do NOT close #34; post the evidence table showing the gap and ask the user whether to open a new child issue or reopen the original.
- The user approves the closing-comment text before it is posted (outward-facing write).
- Commits: plain prose, imperative mood, no conventional-commits prefixes (per CLAUDE.md).
- Verification agents read files from the worktree path `.worktrees/close-out-issue-34/` (pinned at `main` = `75c29de`) so Toni's parallel edits to the main checkout can't shift the view mid-verification.
- Audit doc path: `docs/ops/2026-05-21-agenticd-architectural-analysis.md`. A-section anchors: `#a1` … `#a12`.

---

### Task 1: Pin the verification baseline

**Files:**
- None modified. Read-only git commands.

**Interfaces:**
- Produces: a confirmed statement that worktree HEAD == `origin/main` tip (or an explicit note of any delta), used by Task 2's agent prompts.

- [ ] **Step 1: Fetch and compare**

```bash
cd /Users/tonibergholm/Developer/github/git.agentic
git fetch origin
git log --oneline 75c29de..origin/main
```

Expected: empty output (worktree base `75c29de` is the tip). If NOT empty: run `cd .worktrees/close-out-issue-34 && git merge --ff-only origin/main` to fast-forward the worktree (the spec commit rides on top, so use `git rebase origin/main` instead if ff fails), and note the new SHA for Task 2 prompts.

- [ ] **Step 2: Record the baseline SHA**

```bash
git -C .worktrees/close-out-issue-34 rev-parse --short HEAD~0
```

Expected: a short SHA. Record it; it goes verbatim into the closing comment ("verified at `<sha>`").

---

### Task 2: Fan out tier verification (3 parallel Explore agents)

**Files:**
- None modified. Agents are read-only.

**Interfaces:**
- Consumes: baseline SHA from Task 1.
- Produces: 11 verdict rows, each `{item, verdict: landed|partial|missing, evidence: file:line, pr}` — consumed by Task 3's gate and Task 5's closing comment.

- [ ] **Step 1: Dispatch all three agents in one message (parallel)**

Each agent gets the same preamble plus its tier block. Preamble (verbatim):

```text
You are verifying that architectural recommendations from an audit actually
landed in code. Work ONLY under this directory (a git worktree pinned at
main): /Users/tonibergholm/Developer/github/git.agentic/.worktrees/close-out-issue-34

First read the relevant A-sections of
docs/ops/2026-05-21-agenticd-architectural-analysis.md (each section has an
anchor like <a name="a1"></a> and a "Shipped shape" subsection describing
what was built). Then verify each claim below against the actual code.
Evidence standard: the artifact must exist in code — cite file:line for
each. A "Shipped shape" paragraph in the doc is a CLAIM to check, not
evidence.

Return one row per item, exactly this format, no prose around it:
ITEM | VERDICT (landed/partial/missing) | EVIDENCE (file:line, comma-separated) | NOTES (one line)
```

Tier blocks:

**Agent 1 — must-fix tier:**

```text
A1 (#35, PR #48): Quiesceable trait + QuiesceToken in
crates/agentic-memory/src/triggers.rs; RestoreGuard in
crates/agentic-memory/src/restore.rs; PostgresAdapter::begin_restore() and
restore_with_guard(); TRUNCATE public.agentic_change_log inside the restore
transaction; rollback path in crates/agenticd uses the explicit
guard form; integration test ac1_writes_during_restore_are_reverted in
crates/agentic-memory/tests/integration.rs.

A2 (#36, PR #50): crates/agenticd/src/lifecycle.rs with Lifecycle struct
(CancellationToken + commit_lock), install_signal_handlers(), drain();
main.rs accept loop uses tokio::select! on the shutdown token and calls
drain after loop exit; handle_commit defers HEAD symbolic write until after
stage_and_commit succeeds (B7 phantom-HEAD fix); reconcile_refs_on_startup
runs before socket bind and errors listing every broken branch;
Refs::list_branches() helper in agentic-core.

A8 (#37, PR #46): reverse migrations run inside an outer transaction;
memory-restore guard cannot be silently skipped (restore-guard fix);
accept_data_loss flag is wired end-to-end (proto -> daemon -> migration
path); unit tests for migrate + rollback validation and integration tests
against real Postgres exist.
```

**Agent 2 — hardening tier:**

```text
A3 (#38, PR #52): crates/agenticd/src/commit.rs orchestrator exists with
named 2PC phases matching ADR-0002 D3 order (blobs -> hashes -> Commit blob
-> ref update); handle_commit in server.rs delegates to it.

A4 (#39, PR #51): rollback is a directory module: mod.rs (orchestration),
loaders.rs (typed object readers), writeback.rs (FS prompts/tools) under
crates/agenticd/src/rollback/.

A5 (#40, PR #55): GCS-bound object-store calls in the daemon are wrapped in
tokio::task::spawn_blocking (tactical shape; full async-trait fix is
ADR-0011 — verify the tactical wrapping OR its ADR-0011 successor, and say
which one is present).

A6 (#41, PR #74): Response::Error is a structured variant (not a bare
string) in agentic-proto; framing errors get an error envelope; per
ADR-0010 (docs/adr/0010-wire-protocol-error-model.md) — check the shipped
shape matches the ADR's wire model.

A7 (#42, PR #54): MCP fingerprinting fans out concurrently via
futures::stream::FuturesUnordered (or equivalent buffered concurrency) —
cite the exact site.
```

**Agent 3 — v1.1 tier:**

```text
A9 (#43, PRs #81 + #98): MemoryAdapter trait is complete enough for
non-Postgres backends: apply_reverse_migrations is on the trait;
DaemonState.memory is Option<Arc<dyn MemoryAdapter>> with no adapter mutex;
MemoryBackendSpec exists mirroring ObjectStoreSpec; InMemoryAdapter fixture
passes the daemon rollback path in crates/agenticd/tests/rollback_in_memory.rs.

A10 (#44, PR #75): SegmentManifest::from_canonical_bytes exists and
rollback loaders route through it instead of raw serde_json::from_slice.

A11 (#45, PR #77): Refs::snapshot() returns a RefsSnapshot freezing HEAD +
all branch refs; the daemon's diff path takes the snapshot under
commit_lock then resolves both endpoints from the frozen map.
```

- [ ] **Step 2: Collect the 11 rows**

Expected: 11 rows total (3 + 5 + 3), every row with at least one `file:line` citation. If an agent returns prose instead of rows, extract the rows; if a row has no citation, treat its verdict as `partial` regardless of what the agent claimed.

---

### Task 3: Gate on verdicts

**Files:**
- None modified.

**Interfaces:**
- Consumes: 11 verdict rows from Task 2.
- Produces: go/no-go decision. Go → Tasks 4–6. No-go → the halt path below, and the plan ends there.

- [ ] **Step 1: Check every verdict**

All 11 rows `landed` → proceed to Task 4.

- [ ] **Step 2 (only on partial/missing): Halt and report**

Do NOT close #34 and do NOT post the closing comment. Present the full evidence table to the user in the terminal, highlighting the gap rows, and ask one question: for each gap, open a new child issue or reopen the original (#35–#45)? Wait for the user; subsequent tasks are cancelled.

---

### Task 4: Fix the stale A9 marker in the audit doc

**Files:**
- Modify: `.worktrees/close-out-issue-34/docs/ops/2026-05-21-agenticd-architectural-analysis.md:445`

**Interfaces:**
- Consumes: nothing from prior tasks (independent of verdicts in content, but gated behind Task 3 so a halt stops all writes).
- Produces: branch `close-out-issue-34` containing spec + plan + marker fix, ready for the PR in Task 6.

Known state: §A9's section header (line 401) says **DONE 2026-07-09**, and issue #43 is closed, but the follow-up table row still says **PARTIAL**. All other markers are already consistent (verified 2026-07-10). This task is exactly one line.

- [ ] **Step 1: Apply the edit**

In `.worktrees/close-out-issue-34/docs/ops/2026-05-21-agenticd-architectural-analysis.md`, change:

```markdown
| A9 | Complete `MemoryAdapter` trait | [#43](https://github.com/git-agentic/git.agentic/issues/43) **PARTIAL** | `v1.1` | — |
```

to:

```markdown
| A9 | Complete `MemoryAdapter` trait | [#43](https://github.com/git-agentic/git.agentic/issues/43) **DONE** | `v1.1` | — |
```

- [ ] **Step 2: Verify no other stale markers**

```bash
grep -n 'PARTIAL\|TODO\|TBD' .worktrees/close-out-issue-34/docs/ops/2026-05-21-agenticd-architectural-analysis.md
```

Expected: no output. If anything else surfaces, stop and show the user before editing further — the spec scopes this task to marker sync only.

- [ ] **Step 3: Commit**

```bash
cd .worktrees/close-out-issue-34
git add docs/ops/2026-05-21-agenticd-architectural-analysis.md
git commit -m "Sync A9 status in the audit follow-up table to DONE

Section A9 was marked DONE 2026-07-09 when #43 closed (PRs #81, #98),
but the follow-up table row still said PARTIAL."
```

Expected: clean commit on `close-out-issue-34`.

- [ ] **Step 4: Push and open the docs PR**

```bash
cd .worktrees/close-out-issue-34
git push -u origin close-out-issue-34
gh pr create --repo git-agentic/git.agentic \
  --title "Close-out records for tracking issue #34: spec, plan, audit-table A9 marker sync" \
  --body "Docs-only. Adds the design spec and implementation plan for the #34 close-out, and syncs the audit follow-up table's A9 row to DONE (section §A9 was already DONE 2026-07-09 via #43, the table row lagged).

Verification evidence lives in the closing comment on #34.

https://claude.ai/code/session_01V9TtRvFnDS4eGkcxZhnyf9"
```

Expected: PR URL — record it; it goes into Task 5's closing comment. (Note for the worker: creating a PR is an outward-facing write, but it was pre-approved in the spec's design review; the closing-comment approval in Task 5 Step 2 is the only remaining approval gate.)

---

### Task 5: Close out issue #34 on GitHub

**Files:**
- None in-repo. GitHub writes only.

**Interfaces:**
- Consumes: verdict rows + evidence (Task 2), baseline SHA (Task 1).
- Produces: #34 closed as completed with checked boxes and an evidence-table closing comment.

- [ ] **Step 1: Tick the 11 checkboxes in the issue body**

```bash
cd /private/tmp/claude-501/-Users-tonibergholm-Developer-github-git-agentic/7cadaefd-612c-49a2-8903-2ea0fb3504de/scratchpad
gh issue view 34 --repo git-agentic/git.agentic --json body --jq .body > issue34-body.md
python3 -c "
import re
body = open('issue34-body.md').read()
new, n = re.subn(r'^- \[ \] ', '- [x] ', body, flags=re.M)
assert n == 11, f'expected 11 checkboxes, found {n}'
open('issue34-body-new.md','w').write(new)
print(f'ticked {n} boxes')
"
```

Expected: `ticked 11 boxes`. If the assert fires, inspect `issue34-body.md` — the body may have changed; adjust the count only after confirming every unticked box is one of A1–A11.

```bash
gh issue edit 34 --repo git-agentic/git.agentic --body-file issue34-body-new.md
```

Expected: issue URL printed.

- [ ] **Step 2: Draft the closing comment and get user approval**

Fill this template with Task 2's real evidence (one row per item) and Task 1's SHA, write it to `scratchpad/issue34-close-comment.md`, show it to the user in full, and WAIT for approval before posting:

```markdown
## Close-out: all 11 recommendations verified landed on `main`

Code-grounded verification at `main` = `<baseline-sha>` (2026-07-10): each
artifact below was confirmed to exist in code, not just in a merged PR
description.

| Rec | Issue | PR(s) | Key artifact (evidence) | Verdict |
|---|---|---|---|---|
| A1 | #35 | #48 | `<file:line>` | landed |
| A2 | #36 | #50 | `<file:line>` | landed |
| A8 | #37 | #46 | `<file:line>` | landed |
| A3 | #38 | #52 | `<file:line>` | landed |
| A4 | #39 | #51 | `<file:line>` | landed |
| A5 | #40 | #55 | `<file:line>` | landed |
| A6 | #41 | #74 (ADR-0010 via #73) | `<file:line>` | landed |
| A7 | #42 | #54 | `<file:line>` | landed |
| A9 | #43 | #81 + #98 (ADR-0005 via #78) | `<file:line>` | landed |
| A10 | #44 | #75 | `<file:line>` | landed |
| A11 | #45 | #77 (ADR-0007 via #76) | `<file:line>` | landed |

Deferred system-level concerns were split out via #33 into ADR-0010
(Accepted; implemented via #74) and ADR-0011 (Proposed; A5's tactical
`spawn_blocking` shim covers it meanwhile). A12 remains
intentionally-not-addressed per the audit (documents strengths / benign
findings, not work).

Audit doc marker sync: `<PR link from Task 4 Step 4>`.

Closing as completed.
```

- [ ] **Step 3: Post and close (after approval only)**

```bash
gh issue comment 34 --repo git-agentic/git.agentic --body-file scratchpad/issue34-close-comment.md
gh issue close 34 --repo git-agentic/git.agentic --reason completed
```

Expected: comment URL, then `✓ Closed issue #34`.

---

### Task 6: Verify the close-out

**Files:**
- None new. Verification + bookkeeping only.

**Interfaces:**
- Consumes: closed #34 (Task 5), open docs PR (Task 4 Step 4).
- Produces: verified-closed #34; OpenWolf bookkeeping entries.

- [ ] **Step 1: Verify the close-out**

```bash
gh issue view 34 --repo git-agentic/git.agentic --json state,stateReason --jq '"\(.state) \(.stateReason)"'
gh issue view 34 --repo git-agentic/git.agentic --json body --jq .body | grep -c -- '- \[x\]'
gh pr checks --repo git-agentic/git.agentic close-out-issue-34 || true
```

Expected: `CLOSED COMPLETED`, then `11`, then CI checks pending/green (Markdown-only change).

- [ ] **Step 2: OpenWolf bookkeeping**

Append to `.wolf/memory.md` (main checkout — `.wolf/` is gitignored, confirmed 2026-07-10):

```bash
cd /Users/tonibergholm/Developer/github/git.agentic
printf '| %s | Closed tracking issue #34 with code-grounded evidence; docs PR opened | docs/ops audit table, GH #34 | closed COMPLETED | ~15k |\n' "$(date +%H:%M)" >> .wolf/memory.md
```

Expected: silent success. Do NOT tear down the worktree yet — it hosts the open PR branch; remove it after merge per CLAUDE.md worktree discipline.
