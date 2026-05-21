# Implementation plan — A8 reverse-migration outer tx + restore-guard fix + wire `accept_data_loss`

## Source Specification

- **GitHub issue #37** (`must-fix-v1.0`, milestone `v1.0`) — acceptance criteria.
- [`source.md`](source.md) — source pointer documenting the three audit findings (B8/B9/B10) and the three open behavioral questions Q1/Q2/Q3 surfaced during context load.
- [`../../ops/2026-05-21-agenticd-architectural-analysis.md`](../../ops/2026-05-21-agenticd-architectural-analysis.md) — audit evidence (anchors `#a8`, `#b8`, `#b9`, `#b10`, `#r4`, `#r5`).
- [`artifacts/implementation-decision-log.md`](artifacts/implementation-decision-log.md) — full + trivial decisions.
- [`artifacts/implementation-iteration-history.md`](artifacts/implementation-iteration-history.md) — Round 1 record.
- [`artifacts/.discovery-notes.md`](artifacts/.discovery-notes.md) — project context shared across the team.

No `feature-specification.md` exists for this work — it is a bug-fix PR driven by issue #37 and the audit recommendation §A8.

## Outcome

Ship one PR that closes the three audit-evidenced correctness defects bundled in issue #37:

- **B8** — reverse migration sequences become atomic. Mid-sequence failure rolls all completed steps back.
- **B9** — silent skip of memory restore when `target.memory_snapshot=Some, target.schema_version=None` is fixed by **rejecting the rollback loudly** at validation time (per [D-1](artifacts/implementation-decision-log.md#d-1-q2-resolution--memory_snapshotsome-schema_versionnone-commits-are-rejected-as-malformed)); the unreachable state is treated as malformed.
- **B10** — `accept_data_loss=true` actually bypasses the IRREVERSIBLE check, allowing the operator-flagged path to run the destructive down.sql (per [D-3](artifacts/implementation-decision-log.md#d-3-q3-semantics--accept_data_losstrue-bypasses-the-irreversible-check-option-α)).

After this PR, all three acceptance criteria in issue #37 pass with integration test gates, and the broken-prompt demo (`examples/langgraph-rollback/scripts/run-demo.sh`) still runs end-to-end in under 5 minutes.

## Context

Per [`artifacts/.discovery-notes.md`](artifacts/.discovery-notes.md):

- Touch points: `crates/agenticd/src/migrate.rs`, `crates/agenticd/src/rollback.rs`, `crates/agentic-memory/src/postgres.rs`. No changes to `crates/agentic-memory/src/adapter.rs` (preserves `MemoryAdapter` trait contract per [D-1](artifacts/implementation-decision-log.md#d-1-q2-resolution--memory_snapshotsome-schema_versionnone-commits-are-rejected-as-malformed)).
- No `crates/agenticd/tests/` directory exists yet; A8 creates it.
- Postgres + pgvector via `examples/langgraph-rollback/docker-compose.yml` (port 54322) is the conventional integration-test target. CLAUDE.md forbids mocking Postgres for snapshot/restore paths.
- No churn on target files in last 90 days — no rebase hazard.

## Team Composition

Size: **small** (single-subsystem bug fix, no cross-service, no PII, S effort). Team cap 3, round cap 1.

- **project-manager** — synthesis only (this document and the decision log).
- **junior-developer** — generalist stress-test in R1; produced 10 findings, of which J1 was the critical assumption-refuted that re-shaped the Q1 fix.
- **test-engineer** — chosen specialist; produced 4 test slices, 7 YAGNI candidates, and the TDD-ordering recommendation that is now [D-8](artifacts/implementation-decision-log.md#d-8-tdd-test-ordering--ac3ab-first--ac1--ac2-rejection--docstring--demo-smoke).

Per-round detail: [`artifacts/implementation-iteration-history.md`](artifacts/implementation-iteration-history.md).

## Implementation Approach

### B8 fix — reverse-migration outer transaction ([D-2](artifacts/implementation-decision-log.md#d-2-q1-mechanic--single-executor-threading-via-apply_down_migration_tx))

Single-executor threading. New method on `PostgresAdapter`:

```rust
// crates/agentic-memory/src/postgres.rs
impl PostgresAdapter {
    pub async fn apply_down_migration_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        name: &str,
        sql: &str,
    ) -> Result<()> {
        sqlx::raw_sql(sql)
            .execute(&mut **tx)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("executing down migration {name:?}: {e}")))?;
        let result = sqlx::query("DELETE FROM agentic_migrations WHERE name = $1")
            .bind(name)
            .execute(&mut **tx)
            .await?;
        if result.rows_affected() != 1 {
            return Err(Error::Other(anyhow::anyhow!(
                "expected to delete exactly 1 agentic_migrations row for down migration {name:?}, deleted {}",
                result.rows_affected()
            )));
        }
        Ok(())
    }
}
```

`run_reverse` opens one outer transaction and threads it:

```rust
// crates/agenticd/src/migrate.rs
pub async fn run_reverse(
    adapter: &PostgresAdapter,
    steps: Vec<MigrationStep>,
) -> anyhow::Result<()> {
    if steps.is_empty() {
        return Ok(());
    }
    let mut tx = adapter.pool().begin().await
        .context("opening outer transaction for reverse-migration sequence")?;
    for step in &steps {
        adapter
            .apply_down_migration_tx(&mut tx, &step.name, &step.sql)
            .await
            .with_context(|| format!("applying reverse migration {:?}", step.name))?;
        tracing::info!(migration = %step.name, "reverse migration applied (in outer tx)");
    }
    tx.commit().await.context("committing reverse-migration sequence")?;
    Ok(())
}
```

This requires exposing `pool()` (or `pool_ref()`) as a `pub(crate)` accessor on `PostgresAdapter`, or — alternative — `adapter.begin_reverse_tx().await` returning the outer `Transaction`. Implementer's choice; preserve adapter encapsulation either way.

The old per-step `apply_down_migration` either stays for single-step callers or is removed if no callers remain after the migration. Grep for usage; remove if unused.

### B9 fix — reject malformed Commits early ([D-1](artifacts/implementation-decision-log.md#d-1-q2-resolution--memory_snapshotsome-schema_versionnone-commits-are-rejected-as-malformed))

Add a validation block in `rollback::execute` immediately after the target Commit is loaded (around current line 50-52, before any phase work):

```rust
// crates/agenticd/src/rollback.rs (just after load_commit on line 51)
if target.memory_snapshot.is_some() && target.schema_version.is_none() {
    return Err(anyhow!(
        "target commit {} has memory_snapshot but no schema_version; \
         this state should not be reachable through normal commit-write paths \
         (see ADR-0002 D6 and docs/plans/a8-reverse-migration/source.md §Q2). \
         Refusing rollback. If you reached this through a custom SDK or a legacy \
         commit, this is a v1.1 work item.",
        target_hash.short()
    ));
}
```

The existing `if let Some(ref target_schema)` block at `rollback.rs:86` stays as-is — its gating on `schema_version` is correct given the validation above ensures the (Some, None) combination can never reach it.

**Side effect noted in [D-9](artifacts/implementation-decision-log.md#trivial-decisions):** AC2's literal wording in issue #37 ("performs the memory restore") is contradicted; update the issue text before merge.

### B10 fix — wire `accept_data_loss` into `load_steps` ([D-3](artifacts/implementation-decision-log.md#d-3-q3-semantics--accept_data_losstrue-bypasses-the-irreversible-check-option-α))

Signature change. `load_steps` accepts the flag and forwards it to `check_irreversible`:

```rust
// crates/agenticd/src/migrate.rs
pub fn load_steps(
    agentic_dir: &Path,
    names: &[String],
    accept_data_loss: bool,
) -> anyhow::Result<Vec<MigrationStep>> {
    // ... unchanged until the per-file loop
    for name in names {
        validate_migration_name(name)?;
        let path = schema_dir.join(format!("{name}.down.sql"));
        if !path.exists() {
            return Err(anyhow!("reverse migration file {} is missing; ...", path.display()));
        }
        let sql = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        check_irreversible(name, &sql, accept_data_loss)?;
        // ...
    }
    // ...
}

fn check_irreversible(name: &str, sql: &str, accept_data_loss: bool) -> anyhow::Result<()> {
    if accept_data_loss {
        return Ok(());
    }
    // ... existing logic unchanged
}
```

Call site update in `rollback.rs`:

```rust
// crates/agenticd/src/rollback.rs (line ~116)
let steps = migrate::load_steps(state.refs.agentic_dir(), &migration_names, args.accept_data_loss)
    .context("loading reverse migration files")?;
```

Remove the discarded `let _ = args.accept_data_loss;` at the old line 216.

### Module docstring update ([D-6](artifacts/implementation-decision-log.md#trivial-decisions))

Replace `crates/agenticd/src/migrate.rs:17-22`:

```rust
//! ## Irreversible marker
//!
//! A `.down.sql` whose first non-empty line is `-- IRREVERSIBLE` causes
//! rollback to fail loudly by default. The operator can override this with
//! `agentic rollback --accept-data-loss <ref>` to run the down.sql anyway,
//! accepting that the migration's original forward operation was destructive
//! and the reverse may not restore lost data.
//!
//! The ADR-0002 Decision 5 bounded-rollback path (restore from a snapshot
//! taken before the migration was applied) is a separate v1.1 work item;
//! `--accept-data-loss` does NOT trigger that path in v1.0.
```

## Decomposition and Sequencing

Per [D-8](artifacts/implementation-decision-log.md#d-8-tdd-test-ordering--ac3ab-first--ac1--ac2-rejection--docstring--demo-smoke), in order. Each commit is independently reviewable; the PR ships as one merge commit per CLAUDE.md.

1. **Tests AC3a + AC3b** (unit). Update `check_irreversible` signature, write tests in `migrate.rs#cfg(test)` covering: (a) `accept_data_loss=true` returns Ok for IRREVERSIBLE-marked file; (b) `accept_data_loss=false` still returns Err (no regression). Update existing `irreversible_*` tests to pass `false` explicitly. Verify cargo test runs.
2. **Implementation: B10 (D-3)** — wire the flag through `load_steps` → `check_irreversible`. Remove discarded `let _ = args.accept_data_loss;`. AC3a/AC3b pass.
3. **Test AC1** (integration). Create `crates/agenticd/tests/reverse_migration.rs`. Fixture: 3-step reverse where step 2's down.sql contains `SELECT 1/0;` deliberately. Assert state is fully reverted (current_schema_version unchanged, all 3 agentic_migrations rows present, step 3's table still exists). Test fails on current code.
4. **Implementation: B8 (D-2)** — add `apply_down_migration_tx`; rewrite `run_reverse` to thread one outer transaction. AC1 passes.
5. **Test AC2** (unit on `rollback::execute`, integration if necessary). Fixture: construct a target Commit with `memory_snapshot=Some(arbitrary_hash), schema_version=None`. Assert `rollback::execute` returns Err with message matching the new validation block. Test fails on current code.
6. **Implementation: B9 (D-1)** — add validation block early in `rollback::execute`. AC2 passes.
7. **Test AC3c** (integration in same `reverse_migration.rs`). Fixture: IRREVERSIBLE-marked migration applied; rollback with `accept_data_loss=true` succeeds; rollback with `accept_data_loss=false` returns the IRREVERSIBLE error. Test fails before step 2 is applied; passes after both 2 and 4 land.
8. **Docstring update (D-6)** — replace `migrate.rs:17-22`. Lint passes.
9. **Pre-flight (D-7)** — run `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --check`, `cargo test --workspace`, and `examples/langgraph-rollback/scripts/run-demo.sh`. All green.
10. **Audit doc update** — mark `docs/ops/2026-05-21-agenticd-architectural-analysis.md` §A8 as done with a status line and the PR link.
11. **Issue #37 update (D-9)** — comment on the issue with the AC2 rewording: "Acceptance criterion AC2 updated: rollback with `memory_snapshot=Some, schema_version=None` is rejected with a clear error message (the state is unreachable through normal commit-write paths per ADR-0002; loud rejection resolves B9's 'silent skip' bug)."

## RAID Log

| Type | Item | Mitigation |
|---|---|---|
| Risk | Outer-tx fix breaks an existing single-step caller of `apply_down_migration` | Grep before deletion; mark `#[deprecated]` if any caller remains. |
| Risk | `pool()` accessor leak on `PostgresAdapter` violates adapter encapsulation | Use `pub(crate)` or implement `begin_reverse_tx(&self) -> Result<Transaction<'_,_>>` returning the transaction directly. Implementer's choice. |
| Assumption | Demo's commits all have `(schema_version, memory_snapshot)` both `Some` together | Verified via `crates/agenticd/src/server.rs:302-312`. Will hold for v1.0 demo. |
| Issue | Integration tests need real Postgres on `:54322` | Demo's `docker-compose.yml` provides it; CI must spin one up. |
| Dependency | `cargo test` integration run depends on Postgres availability | Gate test names with `#[ignore]` or env var `DATABASE_URL` similar to existing `agentic-memory/tests/integration.rs` pattern. |

## Testing Strategy

- **Unit tests** in `crates/agenticd/src/migrate.rs#cfg(test)` — AC3a, AC3b, regressions on existing `check_irreversible`/`load_steps` callers.
- **Integration tests** in new `crates/agenticd/tests/reverse_migration.rs` — AC1, AC3c. Real Postgres+pgvector at `:54322` from the demo's compose file.
- **Validation test** for D-1 — either unit-level on `rollback::execute` (preferred; no Postgres dependency for the early-validation path) or integration if the test requires a real `DaemonState`.
- **No mocking** of Postgres, `PostgresAdapter`, or `MemoryAdapter` for snapshot/restore tests (CLAUDE.md).
- **Test ordering enforced by TDD discipline** — each test written and run RED before its implementation commit (see Decomposition).

Coverage gaps explicit:
- AC2 verifies the validation path; verifies the *restore* path for the (Some, None) combination is unreachable by design and therefore untestable. Documented as intentional.
- Bounded-rollback path (Q3 option β) — v1.1 work; not tested.

## Security Posture

- No new wire-protocol surface. `accept_data_loss` is already on `RollbackArgs`; A8 only wires it correctly.
- No new authentication, authorization, or trust boundaries.
- `accept_data_loss=true` is operator-flagged; no escalation of privilege. The destructive SQL ran by user request.
- No new secrets handling. Existing daemon secret-scanner is unaffected.

## Operational Readiness

- **Observability:** existing `tracing::info!(migration = %step.name, "reverse migration applied")` at `migrate.rs:109` remains; consider adding `tracing::warn!(migration = %step.name, accept_data_loss = true, "running IRREVERSIBLE down.sql with operator override")` when the flag bypasses the check. (Cheap, helps incident debugging.)
- **Rollout:** ships in next normal merge to `main`. No feature flag; the daemon must accept the new behavior universally. SDK / CLI are unchanged.
- **Demo gate ([D-7](artifacts/implementation-decision-log.md#trivial-decisions)):** `examples/langgraph-rollback/scripts/run-demo.sh` must complete in under 5 minutes. Run as pre-flight before merge.
- **No infrastructure changes**, no IaC changes, no Dockerfile changes.

## Definition of Done

- [ ] AC3a + AC3b unit tests pass; existing `check_irreversible`/`load_steps` tests still pass with the new boolean parameter.
- [ ] AC1 integration test passes against real Postgres; current_schema_version unchanged after deliberate mid-sequence failure.
- [ ] AC2 test passes; `rollback::execute` returns an error for the malformed-Commit shape.
- [ ] AC3c integration test passes; flag flows correctly end-to-end.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo fmt --check` passes.
- [ ] `cargo test --workspace` passes (assuming Postgres available for integration tests).
- [ ] `examples/langgraph-rollback/scripts/run-demo.sh` runs to completion in under 5 minutes.
- [ ] `docs/ops/2026-05-21-agenticd-architectural-analysis.md` §A8 marked done with PR link.
- [ ] Issue #37 AC2 wording updated with comment per [D-9](artifacts/implementation-decision-log.md#trivial-decisions).
- [ ] PR description references this plan and the audit anchors.

## Specialist Handoffs

None unresolved. Specialist handoff requests from R1:

- `software-architect` for Q2 — routed to user (small classification round cap 1), resolved by user pick on 2026-05-21.
- `behavioral-analyst` for L5 (forward-record propagation) — moot under D-1 (no forward-record produced for rejected Commits).
- `user-experience-designer` for `--accept-data-loss` affordance (audit log, warning prompt) — YAGNI-deferred to v1.1; see `## Deferred (YAGNI)`.

## Deferred (YAGNI)

Each item deferred under the rule at `(internal YAGNI rule)`. Source: test-engineer R1 ledger + project-manager synthesis sweep.

| Item | Failure | Resolution | Source |
|---|---|---|---|
| Tests for Q3(β) bounded-rollback semantic | No upstream finding; (β) is v1.1 per ADR-0002 D5 | Defer | TE R1 L11 |
| Tests for Q3(γ) hybrid (α)+(β) | Speculative; issue #37 commits to (α) | Defer | TE R1 L12 |
| Tests for Q2 options (i)/(ii)/(iv) — alternatives not chosen | No committed behavior to verify | Defer | TE R1 L13 |
| Tests for non-A8 rollback paths (prompt sweep, forward-record, tools/model) | Symmetry-not-evidence anti-pattern; A8 doesn't touch these | Defer | TE R1 L14 |
| Tests for `schema_version=Some, memory_snapshot=None` path | A8 doesn't change this branch; existing behavior preserved | Defer | TE R1 L15 |
| Golden-file/snapshot tests for error messages | Substring assertions suffice (simpler version test) | Replace with substring assertions | TE R1 L16 |
| Shared test-fixture DSL beyond A8 needs | Rule-of-three fails (first integration test in this crate) | Defer; reopen if third use case demands it | TE R1 L17, J6 |
| `accept_data_loss=false` integration smoke duplicate | AC3b unit already covers; integration adds no discriminating signal | Defer | TE R1 L18 |
| `--accept-data-loss` UX affordance (warning prompt, audit log entry) | No v1.0 evidence requiring it; operator opt-in already serves consent | Defer; reopen if support/operator pain surfaces in production | JD R1 OQ-3 |
| `tracing::warn!` on bypassed IRREVERSIBLE — partial implementation | Recommended above in Operational Readiness as cheap observability; treat as nice-to-have inside this PR rather than YAGNI-deferred | Include if trivial; otherwise defer | PM synthesis |
| Preventing commit-time production of `(memory_snapshot=Some, schema_version=None)` Commits | Server-side code already only produces Some/Some or None/None; defensive check at commit write is redundant | Defer; reopen if a custom SDK or legacy commit produces the state in practice (D-1 returns a clear error if so) | JD J8 (moot under D-1) |

Reopening triggers are named per row. None require dependency on another in-flight PR.

## Open Items

- **Issue #37 AC2 wording update** — see [D-9](artifacts/implementation-decision-log.md#trivial-decisions). Add a comment to issue #37 with the rewording before merging the PR, or push a small edit to the AC. Specific wording suggestion: *"AC2: Rollback to a target commit with `memory_snapshot = Some` and `schema_version = None` returns a clear error (this state is unreachable through normal commit-write paths; loud rejection resolves B9's 'silent skip' bug per [D-1](docs/plans/a8-reverse-migration/artifacts/implementation-decision-log.md))."*
- **`begin_reverse_tx` vs `pool()` accessor** — small API choice in B8 implementation. Pick at implementation time; preserve adapter encapsulation either way. (See `## Implementation Approach`.)
- **`apply_down_migration` (per-step) — keep or remove** — grep for callers after the new method lands. Remove if unused; `#[deprecated]` otherwise.

## Summary

A8 fixes three composing correctness defects (B8 atomicity, B9 silent-skip, B10 dead-flag) in one PR. The plan resolves the audit's structurally-broken pseudocode (J1) by threading a single sqlx transaction through a new `apply_down_migration_tx`; resolves the (Some, None) Commit-state ambiguity (Q2) by rejecting the state as malformed per [D-1](artifacts/implementation-decision-log.md#d-1-q2-resolution--memory_snapshotsome-schema_versionnone-commits-are-rejected-as-malformed) (user-confirmed); and commits to option (α) semantics for `accept_data_loss` per AC3 in issue #37.

The PR ships with 4 new tests (AC3a, AC3b, AC1, AC3c) plus 1 validation test (AC2, in whichever shape is cheapest), preserves the existing `MemoryAdapter` trait contract, and gates merge on the broken-prompt demo running in under 5 minutes. Estimated S effort matches the audit's prediction.

PM recommendation: **ready to implement**. Open items are housekeeping; no blockers remain.
