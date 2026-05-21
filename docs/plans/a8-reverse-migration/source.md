# A8 source — reverse-migration outer tx + restore-guard fix + wire `accept_data_loss`

This is the spec pointer for the implementation plan in this folder. The plan is not for a new feature — it is for a bug-fix that bundles three audit findings into one PR per the audit's recommended ship order.

## Authoritative sources

- **GitHub issue #37** (`must-fix-v1.0`, milestone `v1.0`) — acceptance criteria.
- **Audit doc:** [`../../ops/2026-05-21-agenticd-architectural-analysis.md`](../../ops/2026-05-21-agenticd-architectural-analysis.md) — recommendation §A8, behavioral findings B8/B9/B10, risk items R4/R5.

## The three composing defects (verbatim from the audit)

- **B8** — `migrate.rs:91-112` — reverse migration sequence has no outer transaction. Each step is transactional in isolation (via `PostgresAdapter::apply_down_migration` at `postgres.rs:483-501`); mid-sequence failure orphans the schema at an intermediate version no snapshot was taken against.
- **B9** — `rollback.rs:86-157` — memory restore silently skipped when `target.schema_version=None` but `target.memory_snapshot=Some`. The outer `if let Some(ref target_schema)` at `rollback.rs:86` gates BOTH the schema-migration branch AND the memory-restore branch.
- **B10** — `rollback.rs:216` — `accept_data_loss` flag arrives on the wire (`RollbackArgs.accept_data_loss: bool` at `rollback.rs:38`) and is discarded (`let _ = args.accept_data_loss;`). `migrate.rs:21-22, 137-142` reference the flag but the semantic is undefined for v1.0.

## Acceptance criteria from issue #37

- Mid-sequence reverse-migration failure rolls back (test: 3-step reverse, step 2 fails, none committed).
- Rollback with `memory_snapshot = Some` and `schema_version = None` performs the memory restore.
- `accept_data_loss = true` actually bypasses the IRREVERSIBLE check.
- Audit doc §A8 marked done.

## Open behavioral questions surfaced during planning context-load

These are decisions the issue body's pseudocode glosses; the plan must resolve them.

### Q1 — `apply_down_migration` outer-tx shape

The audit's pseudocode shows `conn.begin() … apply_down(&mut tx, m) … tx.commit()`. The actual code path goes through `PostgresAdapter::apply_down_migration` which already opens its own per-step transaction via `self.pool.begin()`. Two ways to add the outer tx:

- **(a)** Add a new adapter method `apply_down_migration_tx(&self, tx: &mut Transaction, name, sql)` that uses a caller-supplied transaction; `run_reverse` opens one outer `pool.begin()` and threads it.
- **(b)** Expose the `PgPool` (or a connection) on the adapter and have `run_reverse` open the outer transaction directly, bypassing `apply_down_migration` entirely.

(a) preserves the adapter's encapsulation but adds API surface. (b) is closer to the audit pseudocode but leaks pool access. **Recommendation track: (a)** — preserves the "everything goes through the adapter" boundary that S1 in the audit already calls out as fragile.

### Q2 — `SnapshotHandle.schema_version` when target has none

`SnapshotHandle` at `crates/agentic-memory/src/adapter.rs:16-19` requires a `String` schema_version. `PostgresAdapter::restore` (per audit S5, `postgres.rs:413-417`) uses this to validate live-vs-target before restoring. If `target.schema_version = None` but `target.memory_snapshot = Some`, what gets passed?

- **(i)** Make `SnapshotHandle.schema_version` `Option<String>` (breaks the inner validation contract; needs a default behavior when None).
- **(ii)** Pass the live schema version (assumes the snapshot is compatible with whatever schema is live).
- **(iii)** Refuse: require commits with `memory_snapshot=Some` to also have `schema_version=Some`. Treat the case as a malformed Commit object and reject at parse time. Demo's commits all have both, so this passes through.
- **(iv)** Special sentinel string ("none", empty) handled by `restore` as "skip the schema check".

(iii) is cleanest but changes the wire/Commit contract — should never have been Some/None in the first place. (i) is the "right" fix but bigger blast radius (affects every backend impl, not just Postgres). (ii) is pragmatic. **No recommendation; user/team decides.**

### Q3 — `accept_data_loss` semantics for v1.0

`migrate.rs:21-22` and `:137-142` say the flag is reserved for the **bounded-rollback path (ADR-0002 D5: snapshot restore instead of forward-migration)**, and explicitly notes "That path is not yet implemented (v1.1 work item)."

The audit's §A8 pseudocode (`if !args.accept_data_loss && plan.has_irreversible_step()`) implies a **different** semantic: skip the IRREVERSIBLE check at load time so the down.sql runs anyway.

- **(α)** Wire the audit's semantic: `accept_data_loss=true` skips `check_irreversible` and runs the down.sql as written. Operator-error-prone, but operator opt-in.
- **(β)** Wire the docstring's semantic: `accept_data_loss=true` selects bounded-rollback path. Not implemented in v1.0; flag is validated and the daemon returns a clear "v1.1" error.
- **(γ)** Both: `accept_data_loss=true` first tries (α); only if no down.sql exists at all, then (β) would apply (but is rejected for v1.0).

The acceptance criterion in issue #37 (`accept_data_loss = true actually bypasses the IRREVERSIBLE check`) explicitly endorses **(α)**. Treat (α) as the committed behavior unless overridden.

## What the plan must produce

A test-driven implementation plan with:

- Integration test slices for the three acceptance criteria (mid-sequence failure rollback; `memory_snapshot=Some, schema_version=None` triggers restore; `accept_data_loss=true` runs IRREVERSIBLE down.sql).
- Decisions on Q1/Q2/Q3 (with rejected alternatives recorded).
- A sequence of small, reviewable code changes mapping to the test slices.
- A pre-flight checklist (clippy, fmt, integration test gate, audit-doc updates marking §A8 done).
