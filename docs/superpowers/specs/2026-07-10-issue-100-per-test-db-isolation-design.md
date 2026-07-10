# Issue #100 — agentic-memory integration suite: per-test database isolation — design

**Date:** 2026-07-10
**Issue:** [#100](https://github.com/git-agentic/git.agentic/issues/100)
**Status:** Approved (brainstorm 2026-07-10)

## Problem

`cargo test -p agentic-memory --test integration -- --ignored` fails 5–7 tests
(varying set) at default parallelism against the single `tests/fixtures/pg.yml`
database; all 17 pass with `--test-threads=1` (OpenWolf bug-125, bug-128).

The tests already isolate *user tables* in per-test schemas
(`agentic_test_<nanos>`). What they share — and what actually causes the
flakiness — is three database-level resources the schema trick does not cover:

1. **`public.agentic_change_log`.** The trigger-capture log is deliberately
   pinned to `public` (`triggers.rs` documents why: the trigger must resolve
   the log regardless of the caller's session `search_path`). Parallel tests
   contaminate each other three ways: test B's trigger events look like
   "untracked table" rows to test A's strict-drain snapshot (→ error);
   `restore()` TRUNCATEs the shared log inside its transaction, nuking other
   tests' pending events; and two tests TRUNCATE it explicitly as fixture
   setup.
2. **The snapshot advisory-lock key** (`SNAPSHOT_KEY`, a hardcoded constant).
   Postgres advisory locks are database-scoped, so all tests' snapshots
   serialize through one lock, and `snapshot_serialises_through_advisory_lock`
   holds it for 200 ms+ while asserting timing — blocking sibling tests.
3. **Nanos-only schema names.** `fresh_schema_name()` has no per-test tag;
   two tests starting in the same nanosecond collide (`23505 duplicate
   schema`, bug-128). `agenticd/tests/reverse_migration.rs` already fixed
   this shape with a `tag` argument.

The issue's literal suggestion (apply `reverse_migration.rs`'s per-test-schema
pattern) cannot fix this suite by itself: the reverse-migration tests do not
exercise the trigger/change-log path; this suite does, and the shared log is
pinned to `public` by product design. The issue anticipated this ("watch for
shared cluster-level resources the schema trick doesn't cover").

## Decision

**Per-test database isolation.** Each test provisions its own Postgres
*database* on the fixture instance. The change log and the advisory-lock
space are both database-scoped, so one database per test isolates everything
— with zero product-code changes, no weakened assertions, and still a real
Postgres (per the CLAUDE.md "don't mock Postgres" rule).

### Alternatives rejected

- **Per-test schema + product change** (make `agentic_change_log`
  schema-scoped in `triggers.rs`): contradicts the documented pinned-to-public
  rationale, touches the load-bearing capture path for test convenience, and
  still leaves the advisory-lock key shared. Off the MVP demo path.
- **Migrate to `#[sqlx::test]`** (sqlx's built-in per-test databases):
  maintained infra, but reshapes every test, and drops the graceful
  skip-when-`DATABASE_URL`-unset behavior. Larger diff for the same isolation.

## Design

All changes live in `crates/agentic-memory/tests/integration.rs` (test
support only) plus one CI line. No product code changes.

### `TestDb` helper

```rust
struct TestDb {
    name: String,        // agentic_test_<nanos>_<tag>
    url: String,         // DATABASE_URL with the path swapped to `name`
    maint_pool: PgPool,  // pool on the ORIGINAL DATABASE_URL, for DROP
}
```

- `TestDb::create(tag: &str) -> Option<TestDb>` — returns `None` when
  `DATABASE_URL` is unset (preserves each test's existing graceful skip).
  Otherwise:
  1. Connects a maintenance pool to `DATABASE_URL`.
  2. Takes a process-wide `static` `tokio::Mutex` and, while holding it,
     runs `CREATE DATABASE "<name>"`. The mutex is held only for the CREATE
     — insurance against Postgres' concurrent-create-from-same-template
     contention; the tests themselves still run fully parallel.
  3. Connects to the new database and runs
     `CREATE EXTENSION IF NOT EXISTS vector` (the adapter's `init()`
     validates pgvector but deliberately never installs it). The fixture's
     `agentic` user is the container superuser, so both statements are
     permitted.
- Per-test name: `agentic_test_<nanos>_<tag>` with a short per-test `tag` —
  the same collision fix `reverse_migration.rs` applied for bug-128's
  `23505`.
- The per-test URL is derived by swapping the database path segment of
  `DATABASE_URL` (via `PgConnectOptions::database()` or equivalent).
- `TestDb::drop(self)` — best-effort teardown, called at the end of each
  test (parity with today's `drop_schema`-at-end; a panicking test leaks its
  database exactly as it leaks its schema today):
  `DROP DATABASE "<name>" WITH (FORCE)` via the maintenance pool. `FORCE`
  (pg13+; fixture is pg16) terminates the adapter's still-open pool/poller
  connections so the drop cannot hang on them.

### Test-body changes (mechanical, semantics-preserving)

- Each test calls `TestDb::create(<tag>)` first and uses `testdb.url` where
  it previously used `database_url()`.
- Each test's "admin pool" connects to the **test database** (today it
  connects to the shared base URL — same database by coincidence). This
  matters for `snapshot_serialises_through_advisory_lock`: the lock holder
  must be in the same database as the adapter to be observable.
- Everything else stays exactly as-is: `make_schema`, `schema_scoped_url`,
  the per-test schemas, every assertion, both explicit
  `TRUNCATE public.agentic_change_log` fixtures (now safely scoped to the
  test's own database). Same assertions, no new `#[ignore]`, no
  serialization workaround — acceptance criterion 2.
- Module doc header: remove the `--test-threads=1` contract and the
  shared-change-log caveat; document the per-test-database shape and the
  unchanged run commands (minus `--test-threads=1`).

### CI

`.github/workflows/ci.yml` postgres job: remove `--test-threads=1` from the
`cargo test -p agentic-memory --test integration` invocation and the comment
justifying it. CI becomes the standing regression guard for this isolation.

### Bookkeeping

- OpenWolf: mark bug-125 / bug-128 fixed in `.wolf/buglog.json` (fix
  description pointing here), cerebrum learning entry (schema isolation is
  insufficient when product state is database-scoped), anatomy/memory
  updates.

## Edge cases considered

- **Perf smoke** (`pg_snapshot_perf_smoke`): gets its own database like every
  other test; BENCH_ROWS default (10k) keeps the extra CREATE/DROP noise
  negligible.
- **`install_helpers_is_idempotent`** currently runs unscoped against the
  shared database; it now runs against its own database — strictly more
  isolated, same assertions.
- **DROP failure**: best-effort (`let _`), matching today's `drop_schema`.
- **Cost**: ~100–300 ms per test for CREATE/DROP DATABASE + extension, ×17
  tests, amortized across parallel threads — well under the serialization
  cost it removes.

## Acceptance criteria (from the issue)

1. Three consecutive `cargo test -p agentic-memory --test integration --
   --ignored` runs pass at default parallelism (against the `pg.yml`
   fixture).
2. No test semantics weakened: same assertions, no new `#[ignore]`, no
   serialization workaround.
3. `--test-threads=1` still passes (no new ordering assumptions).

## Execution notes

Work happens in `.worktrees/issue-100-per-test-db/` per repo worktree
discipline.
