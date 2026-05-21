# Implementation decision log — A8

Companion to `feature-implementation-plan.md`. Records every committed decision with rationale, evidence, and rejected alternatives. Cross-referenced by the main plan via inline `([D-N](artifacts/implementation-decision-log.md#d-N-...))` links.

## Full decisions

### D-1: Q2 resolution — `(memory_snapshot=Some, schema_version=None)` Commits are rejected as malformed

**Outcome:** Add a validation check early in `rollback::execute` (before any schema or memory work) that refuses to roll back to a Commit whose `memory_snapshot.is_some()` and `schema_version.is_none()`. Return a clear error naming the contradiction.

**Rationale:** Server-side commit-write code at `crates/agenticd/src/server.rs:302-312` always produces `(Some(manifest_hash), Some(handle.schema_version))` together — the `(Some, None)` state is unreachable through normal v1.0 code paths. Rejecting loudly is the smallest fix that resolves the B9 audit finding ("memory restore silently skipped"). It preserves the `MemoryAdapter` trait contract (per CLAUDE.md "don't add memory backends in MVP" / ADR-0001 Decision 1) and is forward-compatible to option (i) if v1.1 needs `Option<String>`.

**Evidence:**
- `crates/agenticd/src/server.rs:302-312` — the only commit-write code path that populates memory fields; always produces both `Some` together.
- `docs/ops/2026-05-21-agenticd-architectural-analysis.md#b9` — audit B9 text emphasises "silently skipped"; loud rejection resolves the audit finding.
- `crates/agentic-memory/src/adapter.rs:16-19` — `SnapshotHandle.schema_version: String` (non-optional); preserving the contract avoids trait-wide change.
- `CLAUDE.md` "don't add memory backends in MVP" — option (i)'s blast radius is misaligned with the MVP scope rule.

**Rejected alternatives:**
- **(i) `Option<String>` on `SnapshotHandle.schema_version`** — correct in principle but changes the `MemoryAdapter` trait contract. Affects every future backend. Bigger blast radius than the unreachable state warrants for v1.0.
- **(ii) Pass live schema_version** — pragmatic but silently bypasses the schema-mismatch safety guard at `postgres.rs:415`. Junior-developer J4 flagged risk of restoring schema-mismatched row data.
- **(iv) Sentinel string** — introduces a hidden protocol. Trait contract becomes less honest. Operationally fragile.

**Specialist owner:** user (escalated; resolved 2026-05-21).
**Revisit criterion:** if a future SDK / framework integration produces Commits with `(Some, None)` legitimately, revisit by re-opening Q2 — option (i) is the upgrade path.
**Dissent:** none recorded.
**Driven by rounds:** R1.
**Dependent decisions:** D-3 (AC1/AC2/AC3 test slicing); D-9 (AC2 wording update in issue #37).
**Referenced in plan:** `## Implementation Approach`, `## Open Items`.

### D-2: Q1 mechanic — single-executor threading via `apply_down_migration_tx`

**Outcome:** Add a new method `PostgresAdapter::apply_down_migration_tx(tx: &mut sqlx::Transaction<'_, Postgres>, name: &str, sql: &str) -> Result<()>` that uses the caller-supplied transaction for both the down SQL execution AND the `DELETE FROM agentic_migrations` row removal. `migrate::run_reverse` opens one outer `pool.begin()` and threads `&mut tx` through every step, committing once at the end. The original `apply_down_migration` (per-step transactional) stays for single-step callers if any exist (today: none — make it `#[deprecated]` or remove if no callers remain).

**Rationale:** The audit's literal pseudocode (`conn.begin() → apply_down(&mut tx, m) → tx.commit()`) does NOT deliver atomicity because the current `apply_down_migration` opens its own `self.pool.begin()` — a separate Postgres session. Atomicity requires a single sqlx executor (the outer transaction) threaded through every SQL operation in the sequence. This is junior-developer finding J1 (the audit pseudocode is structurally broken).

**Evidence:**
- `crates/agentic-memory/src/postgres.rs:483-501` — `apply_down_migration` opens its own `pool.begin()`. A second outer `pool.begin()` from `run_reverse` would yield a different session — Postgres transaction atomicity is per-session.
- `crates/agenticd/src/migrate.rs:100-112` — `run_reverse` iterates and calls `adapter.apply_down_migration` per step.
- junior-developer finding `J1` in `implementation-iteration-history.md#r1`.

**Rejected alternatives:**
- **(a) literal audit pseudocode** — broken per J1.
- **(b) expose `PgPool` on the adapter and have `run_reverse` open transactions directly** — leaks pool access through the adapter; ADR-0002 Decision 6 forbids exposing storage-layer concepts at the SDK boundary, and the same principle applies to the adapter trait.

**Specialist owner:** `junior-developer` raised; resolved deterministically in R1 aggregation.
**Revisit criterion:** if `apply_down_migration` (per-step) gains other callers, the deprecation strategy needs revisiting.
**Dissent:** none recorded.
**Driven by rounds:** R1.
**Dependent decisions:** none.
**Referenced in plan:** `## Implementation Approach`, `## Decomposition and Sequencing`.

### D-3: Q3 semantics — `accept_data_loss=true` bypasses the IRREVERSIBLE check (option α)

**Outcome:** Thread `accept_data_loss: bool` from `RollbackArgs` through `migrate::load_steps` into `check_irreversible`. When `accept_data_loss=true`, `check_irreversible` returns `Ok` even for files whose first non-empty line starts with `-- IRREVERSIBLE`. The down.sql executes as written. The flag does NOT trigger the v1.1 bounded-rollback path (option β); that path stays unimplemented in v1.0.

**Rationale:** Acceptance criterion AC3 in issue #37 explicitly endorses option (α). The flag's docstring at `migrate.rs:21-22` describes option (β) (bounded-rollback) but acknowledges "not yet implemented (v1.1 work item)." A8 commits to (α) for v1.0 and updates the docstring in the same PR (see D-6).

**Evidence:**
- GH issue #37 AC3: "`accept_data_loss = true` actually bypasses the IRREVERSIBLE check."
- `crates/agenticd/src/migrate.rs:21-22, 137-142` — current docstring describes (β); will be updated per D-6.
- `crates/agenticd/src/rollback.rs:216` — flag is discarded; must be wired into `load_steps`.

**Rejected alternatives:**
- **(β) bounded-rollback path activation** — explicitly v1.1 work per ADR-0002 Decision 5; not in A8 scope.
- **(γ) hybrid (α) + (β) fallback** — speculative; issue #37 commits only to (α).

**Specialist owner:** issue #37 author (Toni); resolved by AC text.
**Revisit criterion:** when v1.1 bounded-rollback ships, `accept_data_loss=true` semantics may extend to also trigger that path. Reopen Q3 at that point.
**Dissent:** none recorded.
**Driven by rounds:** R1.
**Dependent decisions:** D-6 (docstring update); D-5 (AC3 test split).
**Referenced in plan:** `## Implementation Approach`, `## Decomposition and Sequencing`.

### D-8: TDD test ordering — AC3a/b first → AC1 → AC2 (rejection) → docstring + demo smoke

**Outcome:** Write tests in this order, each test FAILING on current code and PASSING only after the corresponding fix:

1. **AC3a + AC3b (unit, `crates/agenticd/src/migrate.rs#cfg(test)`)** — `check_irreversible` / `load_steps` honor the `accept_data_loss` parameter. Drives D-3's signature change.
2. **AC1 (integration, new `crates/agenticd/tests/reverse_migration.rs`)** — 3-step reverse, step 2 fails, none committed. Drives D-2's outer-transaction fix.
3. **AC2 (integration / unit, in `crates/agenticd/src/rollback.rs` test module or `tests/`)** — Commit with `memory_snapshot=Some, schema_version=None` is rejected with a clear error. Drives D-1's validation check.
4. **AC3c (integration, in same `tests/reverse_migration.rs` as AC1)** — `accept_data_loss=true` flag flows from `RollbackArgs` to `load_steps`; an IRREVERSIBLE-marked migration is reversed end-to-end. Drives the wiring change.
5. **Pre-flight: `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --check` + `examples/langgraph-rollback/scripts/run-demo.sh`** (per D-7).

**Rationale:** AC3a/b have zero external dependencies (no Postgres, no daemon) and drive the cheapest API-shape change first. AC1 next because its outcome assertions are agnostic to D-2's implementation shape; the test will fail until atomicity is delivered. AC2 third because D-1 is the smallest code change and AC2 was the user-escalation that produced the answer. AC3c is the integration smoke that confirms the flag wiring end-to-end. Demo smoke last as a regression gate.

**Evidence:**
- test-engineer's R1 output: explicit TDD-ordering recommendation matching this sequence.
- junior-developer J6 + J7: fixture and split conventions established in this sequence.

**Rejected alternatives:**
- **Implementation-first, tests-after** — violates the user's explicit "han:plan-implementation, then TDD" workflow choice. Tests-after also fails to surface API-shape decisions early (e.g., D-3's `load_steps` signature change).
- **All-integration tests** — over-tests AC3a/b which are pure filesystem logic; CLAUDE.md ethos prefers fast tests where the behavior is observable at unit level.

**Specialist owner:** `test-engineer`.
**Revisit criterion:** if a test in this sequence reveals a discovery that changes a fix's structure (e.g., AC1 reveals D-2's executor threading needs a different shape), pause and re-plan.
**Dissent:** none recorded.
**Driven by rounds:** R1.
**Dependent decisions:** D-1, D-2, D-3, D-4, D-5.
**Referenced in plan:** `## Decomposition and Sequencing`, `## Testing Strategy`.

## Trivial decisions

- **D-4**: AC1 integration test lives at new file `crates/agenticd/tests/reverse_migration.rs`. — Referenced in plan: `## Decomposition and Sequencing`, `## Testing Strategy`.
- **D-5**: AC3 splits into AC3a (unit: `check_irreversible` honours flag) + AC3b (unit: regression for `accept_data_loss=false`) + AC3c (integration smoke: flag flows from `RollbackArgs` to call site). — Referenced in plan: `## Decomposition and Sequencing`, `## Testing Strategy`.
- **D-6**: `crates/agenticd/src/migrate.rs:21-22` module docstring is updated in the same PR to describe Q3(α) semantics and remove the misleading "not yet implemented" line about (β). The (β) bounded-rollback path is referenced as v1.1 work per ADR-0002 Decision 5. — Referenced in plan: `## Implementation Approach`.
- **D-7**: `examples/langgraph-rollback/scripts/run-demo.sh` end-to-end run is added to the A8 pre-flight checklist alongside clippy/fmt/test. — Referenced in plan: `## Operational Readiness`, `## Definition of Done`.
- **D-9**: AC2's literal wording in issue #37 ("performs the memory restore") is updated to reflect option (iii)'s rejection semantic before the PR merges. Comment on the issue with the rewording or push an edit. — Referenced in plan: `## Open Items`.
