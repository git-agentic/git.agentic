# PR-1 — Documentation honesty + PgConfig Debug redaction — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** First PR of the pre-public-release hardening sprint. Closes TM-010 (PgConfig `Debug` may leak password to logs) and the documentation half of TM-009 (the scanner claim across four doc files becomes accurate by forward-referencing ADR-0013, which will land before the scanner code in PR-3).

**Architecture:** Two unrelated but small changes bundled because both are pure-text/single-test cleanups with no dependencies on other PRs. Doc edits replace the existing "the daemon scans every blob..." claim with a forward-reference to ADR-0013. Code change adds a custom `impl std::fmt::Debug for PgConfig` that redacts the password portion of the connection URL; existing `#[derive(Debug)]` is removed and a regression test asserts the redaction.

**Tech Stack:** Markdown (docs); Rust (`crates/agentic-memory/src/postgres.rs`); `cargo test -p agentic-memory`.

**Spec reference:** [`docs/superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md`](../specs/2026-05-21-pre-public-hardening-sprint-design.md) §"PR-1: Documentation honesty + `PgConfig` Debug redaction".

---

## File Structure

**Modified:**
- `crates/agentic-memory/src/postgres.rs` — remove `Debug` from the `#[derive]` at line 50; add a custom `impl Debug for PgConfig` that prints all fields verbatim EXCEPT the URL, where the password component is replaced with `***`. Add `#[cfg(test)] mod tests` (or extend existing tests module) with the redaction regression test.
- `CLAUDE.md:86` — replace the "Don't commit secrets" bullet's scanner sentence with a forward-reference to ADR-0013.
- `AGENTS.md:86` — same edit, identical to CLAUDE.md.
- `docs/architecture/overview.md:212` — update the sentence in the "Threat model" paragraph that asserts the scanner.
- `docs/product/competitive-brief-entire.md:61` — update the "Best-effort secret redaction at write" bullet to point at ADR-0013 instead of "`CLAUDE.md` §'What not to do'".

No new files.

---

## Branch + Setup

### Task 0: Create the working branch

**Files:** none.

- [ ] **Step 1: Confirm we are on main and clean**

```bash
git checkout main
git status --short
```

Expected: empty output (no modified/staged files), or only untracked files unrelated to this work (`.cursor/`, `.opencode/`, `.pi/`, `PROMPT-open-source-cleanup.md`, `website/public/git-agentic-avatar-*.png`).

- [ ] **Step 2: Pull latest main**

```bash
git pull --ff-only
```

Expected: `Already up to date.` (or a clean fast-forward).

- [ ] **Step 3: Create the branch**

```bash
git checkout -b chore/pr1-docs-honesty-and-pgconfig-redact
```

Expected: `Switched to a new branch 'chore/pr1-docs-honesty-and-pgconfig-redact'`.

---

## Task 1 — PgConfig Debug redaction (TM-010)

**Files:**
- Modify: `crates/agentic-memory/src/postgres.rs:50` (the `#[derive(Clone, Debug)]` line and the struct just below)
- Test: `crates/agentic-memory/src/postgres.rs` (extend the existing `#[cfg(test)] mod tests` block, or add one if absent)

### Task 1.1: Locate the existing test module (if any)

- [ ] **Step 1: Find the test module**

```bash
grep -n '#\[cfg(test)\]\|mod tests' crates/agentic-memory/src/postgres.rs | head -5
```

Expected: zero or more matches. If a `mod tests` exists, extend it in Step 4 below. If not, add it at end of file.

### Task 1.2: Write the failing test first

- [ ] **Step 2: Add the failing test**

Add this test at the bottom of `crates/agentic-memory/src/postgres.rs` (inside the existing `#[cfg(test)] mod tests { ... }` if present; otherwise wrap it in a new such block):

```rust
#[cfg(test)]
mod debug_redaction_tests {
    use super::*;

    #[test]
    fn pgconfig_debug_redacts_password() {
        let cfg = PgConfig::new(
            "postgres://agentic:super-secret-pw@localhost:54322/agentic",
            Vec::new(),
        );
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("super-secret-pw"),
            "Debug output must redact the password; got: {dbg}"
        );
        assert!(
            dbg.contains("***"),
            "Debug output must mark the redacted segment with ***; got: {dbg}"
        );
        // Sanity: other URL pieces remain visible so debugging is still useful.
        assert!(
            dbg.contains("localhost") && dbg.contains("agentic"),
            "Debug output should preserve host and db name; got: {dbg}"
        );
    }

    #[test]
    fn pgconfig_debug_handles_url_without_password() {
        let cfg = PgConfig::new("postgres://agentic@localhost:54322/agentic", Vec::new());
        let dbg = format!("{cfg:?}");
        // A URL without a password must still format without panicking,
        // and must not introduce a spurious *** marker.
        assert!(dbg.contains("localhost"));
        assert!(!dbg.contains("***"));
    }

    #[test]
    fn pgconfig_debug_handles_malformed_url() {
        // If parsing fails, fall back to a redacted placeholder rather
        // than echoing the raw URL — fail-secure.
        let cfg = PgConfig::new("not-a-valid-url", Vec::new());
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("not-a-valid-url"));
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

```bash
cargo test -p agentic-memory --lib debug_redaction_tests 2>&1 | tail -15
```

Expected: at least one of the tests fails. The likely failure is `pgconfig_debug_redacts_password` because the derived `Debug` impl prints the full URL including the password. The output will contain `super-secret-pw`.

If the build itself fails first (e.g., compile errors), fix those before continuing. If all three tests pass, the codebase already has a custom `Debug` and this PR is a no-op for TM-010 — re-read `postgres.rs` and reconcile.

### Task 1.3: Implement the custom Debug

- [ ] **Step 4: Remove `Debug` from the derive on `PgConfig`**

Change `crates/agentic-memory/src/postgres.rs` line 50 from:

```rust
#[derive(Clone, Debug)]
pub struct PgConfig {
```

to:

```rust
#[derive(Clone)]
pub struct PgConfig {
```

- [ ] **Step 5: Add the custom Debug impl**

Add this `impl` block immediately after the `impl PgConfig { ... }` block (i.e., after the closing `}` of `impl PgConfig` around line 75):

```rust
impl std::fmt::Debug for PgConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgConfig")
            .field("url", &redact_password_in_url(&self.url))
            .field("tables", &self.tables)
            .field("segment_target_bytes", &self.segment_target_bytes)
            .field("replication_slot", &self.replication_slot)
            .field("poll_interval", &self.poll_interval)
            .finish()
    }
}

/// Replace the password segment of a Postgres URL with `***` for safe
/// formatting in logs and error chains. Falls back to a fully-redacted
/// placeholder if the URL cannot be parsed — fail-secure.
fn redact_password_in_url(raw: &str) -> String {
    // Postgres URLs are of the form
    //   postgres[ql]://[user[:password]@]host[:port][/db][?params]
    // We split conservatively: find "://", then look for "@" in the
    // remainder; if found, look for ":" in the userinfo half. Anything
    // weirder than that gets fully redacted to avoid leaking on parse
    // failure.
    let Some((scheme, rest)) = raw.split_once("://") else {
        return "<redacted: unparseable url>".to_string();
    };
    let Some((userinfo, hostpart)) = rest.split_once('@') else {
        // No userinfo means no password to redact.
        return raw.to_string();
    };
    let userinfo_redacted = match userinfo.split_once(':') {
        Some((user, _password)) => format!("{user}:***"),
        None => userinfo.to_string(),
    };
    format!("{scheme}://{userinfo_redacted}@{hostpart}")
}
```

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cargo test -p agentic-memory --lib debug_redaction_tests 2>&1 | tail -15
```

Expected: `test result: ok. 3 passed; 0 failed`.

If `pgconfig_debug_handles_malformed_url` fails because the malformed URL is being echoed, the fallback logic in `redact_password_in_url` isn't covering whatever shape the test produced. Re-examine the fallback and ensure `<redacted: unparseable url>` is returned for inputs missing `://`.

### Task 1.4: Verify nothing else broke

- [ ] **Step 7: Run all of the agentic-memory tests**

```bash
cargo test -p agentic-memory --lib 2>&1 | tail -10
```

Expected: all tests pass. If anything else used `format!("{cfg:?}")` and asserted on the URL substring, those tests now need updating — find them with `grep -n 'PgConfig.*:?' crates/agentic-memory/` and adjust.

- [ ] **Step 8: Run workspace clippy**

```bash
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -10
```

Expected: `Finished` with no warnings/errors.

### Task 1.5: Commit Task 1

- [ ] **Step 9: Commit**

```bash
git add crates/agentic-memory/src/postgres.rs
git commit -m "agentic-memory: redact password in PgConfig Debug impl (TM-010)

TM-010 from the pre-public-release threat model: PgConfig derives Debug,
so any tracing::debug!(\"{cfg:?}\") or panic-chain formatter that touches
the config prints the full DATABASE_URL including the password. Replace
the derived impl with a custom one that runs the URL through a
conservative parser and substitutes the password with ***. Falls back
to a fully-redacted placeholder on parse failure (fail-secure).

Three regression tests assert (a) the password is removed, (b) URLs
without a password are unaffected, (c) malformed URLs are not echoed.

Refs threat model TM-010."
```

Expected: commit succeeds.

---

## Task 2 — Documentation honesty (TM-009 docs side)

**Files:**
- Modify: `CLAUDE.md:86`
- Modify: `AGENTS.md:86`
- Modify: `docs/architecture/overview.md:212`
- Modify: `docs/product/competitive-brief-entire.md:61`

Each edit replaces the "the daemon scans every blob..." sentence with a forward-reference to ADR-0013. Phrasing is consistent across the four files but adapts to local context (CLAUDE/AGENTS use the "Don't" rule shape; overview uses prose; competitive-brief uses a feature comparison bullet).

### Task 2.1: Update CLAUDE.md

- [ ] **Step 1: Make the edit**

Edit `CLAUDE.md` line 86 from:

```markdown
- **Don't commit secrets.** The daemon scans every blob it would write for high-entropy strings matching common token patterns and refuses to write them. Don't bypass the scanner; fix the input.
```

to:

```markdown
- **Don't commit secrets.** Per [ADR-0013](docs/adr/0013-secret-scanner.md), the daemon hard-rejects blobs containing matched secret patterns or high-entropy substrings at `put_raw` time, returning `Error::SecretDetected`. Don't bypass the scanner; fix the input.
```

- [ ] **Step 2: Verify the edit**

```bash
grep -n -A1 'Don.t commit secrets' CLAUDE.md
```

Expected: shows the new sentence pointing at ADR-0013.

### Task 2.2: Update AGENTS.md

- [ ] **Step 3: Make the edit**

The line at `AGENTS.md:86` is character-for-character identical to the CLAUDE.md one. Apply the identical replacement.

- [ ] **Step 4: Verify the edit**

```bash
grep -n -A1 'Don.t commit secrets' AGENTS.md
```

Expected: shows the new sentence pointing at ADR-0013.

### Task 2.3: Update docs/architecture/overview.md

- [ ] **Step 5: Make the edit**

Edit `docs/architecture/overview.md` line 212 from:

```markdown
The daemon runs as the same user as the application. There is no authentication on the socket beyond filesystem permissions. The object store is unencrypted at rest. Secrets are *never* committed (the daemon scans every blob it would write for high-entropy strings matching common token patterns and refuses to write them).
```

to:

```markdown
The daemon runs as the same user as the application. There is no authentication on the socket beyond filesystem permissions. The object store is unencrypted at rest. Secrets are hard-rejected at `put_raw` per [ADR-0013](../adr/0013-secret-scanner.md) — the scanner runs both a curated pattern set and a Shannon-entropy heuristic, and any match returns `Error::SecretDetected` without writing the blob.
```

- [ ] **Step 6: Verify the edit**

```bash
grep -n 'put_raw' docs/architecture/overview.md
```

Expected: at least one hit on the new sentence.

### Task 2.4: Update docs/product/competitive-brief-entire.md

- [ ] **Step 7: Make the edit**

Edit `docs/product/competitive-brief-entire.md` line 61 from:

```markdown
- **Best-effort secret redaction at write.** Entire's `redact/` package; our daemon's high-entropy scanner per `CLAUDE.md` §"What not to do". Both call it best-effort. That framing is the honest one.
```

to:

```markdown
- **Best-effort secret redaction at write.** Entire's `redact/` package; our daemon's pattern + entropy scanner per [ADR-0013](../adr/0013-secret-scanner.md). Both call it best-effort. That framing is the honest one.
```

- [ ] **Step 8: Verify the edit**

```bash
grep -n 'ADR-0013' docs/product/competitive-brief-entire.md
```

Expected: at least one hit.

### Task 2.5: Cross-check all four edits

- [ ] **Step 9: Confirm the old phrasing is gone everywhere**

```bash
git grep -n 'high-entropy strings matching common token patterns'
```

Expected: zero matches. If anything still has the old phrasing, edit it the same way.

- [ ] **Step 10: Confirm the forward-reference is in place**

```bash
git grep -nl 'ADR-0013'
```

Expected: four files at minimum (`CLAUDE.md`, `AGENTS.md`, `docs/architecture/overview.md`, `docs/product/competitive-brief-entire.md`). If any are missing, recheck Step 1/3/5/7.

Note: at this point `docs/adr/0013-secret-scanner.md` does not yet exist — it lands with PR-3. The forward-reference is intentional; markdown link checkers will warn, which is the expected signal until PR-3 merges. If your repo has a link-check CI job that blocks the PR, mark those warnings as expected in the PR description and link to this plan.

### Task 2.6: Commit Task 2

- [ ] **Step 11: Commit**

```bash
git add CLAUDE.md AGENTS.md docs/architecture/overview.md docs/product/competitive-brief-entire.md
git commit -m "docs: forward-reference ADR-0013 for the secret scanner claim (TM-009)

Pre-public-release threat model TM-009: CLAUDE.md, AGENTS.md, the
architecture overview, and the competitive brief all claim the daemon
runs a high-entropy secret scanner. That scanner does not exist in code
today; the claim ships honest only after PR-3 lands the implementation.

This commit updates the four mentioning files to point at ADR-0013,
which formalises the scanner contract. After PR-3 the link target
exists and the claim resolves; before PR-3 the language reads as a
deliberate forward reference rather than a false invariant.

Refs threat model TM-009; full sprint plan in
docs/superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md."
```

Expected: commit succeeds.

---

## Task 3 — Pre-flight verification

**Files:** none (verification only).

### Task 3.1: Full workspace verification

- [ ] **Step 1: Workspace build**

```bash
cargo check --workspace 2>&1 | tail -3
```

Expected: `Finished`.

- [ ] **Step 2: Workspace tests**

```bash
cargo test --workspace --lib 2>&1 | tail -5
```

Expected: `test result: ok` for every crate; total passing should be the pre-PR count + 3 (the three new redaction tests).

- [ ] **Step 3: Workspace clippy**

```bash
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -5
```

Expected: `Finished` with no errors.

- [ ] **Step 4: Format check**

```bash
cargo fmt --check 2>&1 | tail -3
```

Expected: zero diff output. If `cargo fmt --check` reports differences, run `cargo fmt -p agentic-memory` then re-stage and amend the Task 1 commit (this is the project convention; format fixes do not warrant a separate commit).

### Task 3.2: Verify the acceptance criteria

- [ ] **Step 5: Verify TM-010 acceptance**

```bash
cargo test -p agentic-memory --lib debug_redaction_tests 2>&1 | tail -5
```

Expected: `test result: ok. 3 passed; 0 failed; ...`.

- [ ] **Step 6: Verify TM-009 docs acceptance**

```bash
git grep 'high-entropy strings matching common token patterns'
git grep -l 'ADR-0013' | head
```

Expected: first command empty; second lists at least the four touched files.

---

## Task 4 — Push and open PR

### Task 4.1: Push the branch

- [ ] **Step 1: Push**

```bash
git push -u origin chore/pr1-docs-honesty-and-pgconfig-redact 2>&1 | tail -3
```

Expected: branch pushed.

### Task 4.2: Open the PR

- [ ] **Step 2: Open the PR**

```bash
gh pr create --title "chore: PR-1 docs honesty + PgConfig Debug redaction (TM-009/010)" --body "$(cat <<'EOF'
## Summary

First PR of the pre-public-release hardening sprint. Closes TM-010 fully and the documentation half of TM-009 (the implementation half ships in PR-3).

- **TM-010** — \`PgConfig\` derives \`Debug\`, so any \`tracing::debug!\` / panic-chain formatter that touches the config prints the full \`DATABASE_URL\` including the password. Custom \`Debug\` impl now runs the URL through a conservative parser and replaces the password with \`***\`; falls back to a fully-redacted placeholder on parse failure. Three regression tests in \`debug_redaction_tests\` assert the redaction, the no-password case, and the malformed-URL fail-secure case.
- **TM-009 (docs)** — \`CLAUDE.md\`, \`AGENTS.md\`, \`docs/architecture/overview.md\`, and \`docs/product/competitive-brief-entire.md\` all claim the daemon runs a high-entropy secret scanner. That scanner does not exist in code today — it lands in PR-3. This PR updates the four files to forward-reference ADR-0013, so the claim is honest about *what* will exist and unambiguous about *when*.

Sprint design doc: [\`docs/superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md\`](docs/superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md).
Plan for this PR: [\`docs/superpowers/plans/2026-05-21-pr1-docs-honesty-and-pgconfig-redact.md\`](docs/superpowers/plans/2026-05-21-pr1-docs-honesty-and-pgconfig-redact.md).

## Test plan

- [x] \`cargo test -p agentic-memory --lib debug_redaction_tests\` — 3 passed
- [x] \`cargo test --workspace --lib\` — green
- [x] \`cargo clippy --workspace --all-targets -- -D warnings\` — green
- [x] \`cargo fmt --check\` — clean
- [x] \`git grep 'high-entropy strings matching common token patterns'\` — empty
- [x] \`git grep -l 'ADR-0013'\` — lists the four touched docs

## Forward reference note

The links to \`docs/adr/0013-secret-scanner.md\` are intentional forward references — the ADR lands separately before PR-3. Markdown link-check CI may warn until then; that warning is expected.
EOF
)" 2>&1 | tail -3
```

Expected: PR URL printed.

---

## Self-Review

Run after the plan is fully drafted (i.e., now):

**Spec coverage:** Spec §"PR-1" lists two outputs — TM-010 PgConfig Debug redaction and TM-009 docs forward-reference. Both are covered by Task 1 and Task 2 respectively. ✓

**Placeholder scan:** No "TBD", "TODO", "implement later", "appropriate error handling", or "similar to Task N" in the plan. Every step has either concrete code, a concrete command, or a concrete file edit. ✓

**Type consistency:** The plan introduces `redact_password_in_url` (Task 1.3 Step 5) and references it from the `Debug` impl in the same step. The function signature is consistent (`fn redact_password_in_url(raw: &str) -> String`). The three test names (`pgconfig_debug_redacts_password`, `pgconfig_debug_handles_url_without_password`, `pgconfig_debug_handles_malformed_url`) are used identically in Task 1.2 and the verification commands in Task 3.1 / Task 3.2. ✓

**Scope:** plan implements only what the spec assigns to PR-1. No drift into PR-2/3/4/5/6 territory. ✓

---

## Done definition for this plan

- Branch `chore/pr1-docs-honesty-and-pgconfig-redact` pushed.
- PR opened on GitHub.
- All workspace checks (`check`, `test`, `clippy`, `fmt --check`) green.
- TM-010 redaction tests passing.
- `git grep 'high-entropy strings matching common token patterns'` empty.
- PR ready for review.
