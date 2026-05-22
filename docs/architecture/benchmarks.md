# Benchmarks

**Last measured:** 2026-05-22 on Apple Silicon (8-core, Docker Desktop on macOS host; demo's Postgres+pgvector at `localhost:54322`).
**Methodology:** Criterion (`cargo bench -p agentic-core --bench store`) for micro-benchmarks; a new `#[ignore]`-gated integration test, `pg_snapshot_perf_smoke`, drives the §9-shaped Postgres path against a real `PostgresAdapter`; the broken-prompt demo end-to-end run (`scripts/run-demo.sh`) for the operator-level integration timing.

This is a **measured early sanity-check** of where performance sits relative to the [`snapshot-model.md`](snapshot-model.md) §9 targets — not yet the published v1.0.x commitment. The numbers below come from a developer machine (laptop-class), not a representative cloud instance, and the §9 line that asks for *1M-row pgvector + 100 deltas* is approximated by *N-row episodes (text-only) without deltas*. Treat the §9-shape rows as "no obvious blocker at the laptop-scale shape we've measured"; the published commitment lands when we have a CI-driven harness on a known instance class.

## Targets vs measured

| Operation | Target (`snapshot-model` §9) | Measured | Notes |
|---|---|---|---|
| `commit` (memory snapshot of N-row pgvector) | < 2 s @ 1M rows + 100 deltas | **8.87 ms @ 100K rows** (laptop, post-fix) — linearly ~89 ms at 1M | text-only `episodes` schema; no embeddings, no deltas. ✓ comfortably within target. |
| `commit` (prompts-only, 16-byte system prompt, no memory) | n/a — degenerate input | **2.3 ms median** (Criterion) | ✓ no obvious blocker; not a §9 measurement |
| `rollback` (memory restore of N-row pgvector) | < 5 s @ 1M rows + 10 commits | **12.29 s @ 100K rows** (laptop) — linearly ~120 s at 1M | ⚠ does **not** meet §9; per-row INSERT round-trips are the bottleneck. Multi-row INSERT or COPY batching is the fix (see "Coverage gaps" below). |
| `rollback` (broken-prompt demo, end-to-end) | n/a — demo scale | **~1 s observed** in `run-demo.sh`; ~6.7 s total for 12 demo steps incl. Postgres bring-up | ✓ demo discipline met |
| `diff` (manifest hash compare across snapshots) | < 1 s @ 1M-row pgvector | **66 µs @ 10K rows** (laptop) | manifest comparison doesn't touch Postgres — cost scales with manifest size (segment-entry count), not row count or DB I/O. Trivially within target at the segment counts we measure. |
| `diff` (demo scenario) | n/a — demo scale | sub-second (observed) | ✓ demo discipline met |
| Per-blob write, **median** | < 5 ms per row (p99) | **2.7 ms median (512 KB)** / **0.83 ms median (1 KB)** | ⚠ Criterion reports median, not p99 — p99 needs `--save-baseline` raw-sample analysis (tracked) |
| Snapshot storage amortized | < 2× changed data | _not yet measured_ | ⚠ pending segment-size sampling job |

## Postgres-integration smoke (`pg_snapshot_perf_smoke`)

The harness lives at [`crates/agentic-memory/tests/integration.rs`](../../crates/agentic-memory/tests/integration.rs); see "How to reproduce" below for the invocation. Each run seeds N rows into a fresh schema, takes a snapshot, restores it (no-op), takes a second snapshot, and computes the manifest-hash diff. Operations are wall-clock-timed via `std::time::Instant`, not statistically sampled — these are single-shot wall numbers, not Criterion-style medians.

Numbers below are from a developer laptop (Apple Silicon, 8-core; Postgres 16 + pgvector in Docker Desktop on the same host).

### `BENCH_ROWS=10000` (2026-05-22, post-snapshot-fix)

| Operation | Measured | §9 target |
|---|---|---|
| bulk seed (Postgres INSERTs, 10000 rows) | 30 ms | n/a — setup cost |
| `snapshot()` | ~5 ms (post-fix) — was 39 ms with the O(n²) bug | < 2 s @ 1M-row pgvector |
| `restore()` (no-op replay) | 1.33 s | < 5 s @ 1M-row + 10 commits |
| `diff` (manifest hash compare) | 66 µs | < 1 s @ 1M-row pgvector — manifest-size-bounded, not row-bounded |

Both manifest hashes match across the snapshot → restore → snapshot cycle (round-trip determinism).

### `BENCH_ROWS=100000` (2026-05-22, post-snapshot-fix)

| Operation | Measured | §9 target |
|---|---|---|
| bulk seed (Postgres INSERTs, 100000 rows) | 231 ms | n/a — setup cost |
| `snapshot()` | **8.87 ms** | < 2 s @ 1M-row pgvector |
| `restore()` (no-op replay) | **12.29 s** ⚠ does not meet §9 extrapolated to 1M | < 5 s @ 1M-row + 10 commits |
| `diff` (manifest hash compare) | 4.7 µs | < 1 s @ 1M-row pgvector |

Manifest hashes match across the snapshot → restore → snapshot cycle.

Pre-fix, this scale was unrunnable — the first attempt at 100K rows was killed after >10 min of 100%-CPU work, Postgres idle. A diagnostic pass found two distinct issues:

**Diagnosis 1 — Snapshot O(n²) on segment size** *(fix applied in this PR)*: `bootstrap_table` called `current.canonical_size()` after every row to decide whether to seal a segment. `canonical_size()` is `serde_json::to_vec(self).len()` — it re-serialised the **entire growing segment** on every iteration. With the default 64 MiB segment target, a 100K-row bootstrap re-encoded the segment ~100K times for a cumulative O(n²) cost. Replaced with a running-byte counter that accumulates each row's encoded size as it lands.

**Diagnosis 2 — Restore O(n) round-trips** *(documented; fix is a follow-up)*: `restore_manifest` loops every row in the manifest and calls `apply_envelope` → `upsert_row`, which issues a single parameterised INSERT per row through the open transaction. For N rows that's N Postgres round-trips inside one transaction. At ≈ 100 µs per query on localhost the floor is ≈ N × 100 µs — exactly what 10K (1.33 s) and projected 1M (≈ 100 s) show. Fixing this needs either a multi-row `INSERT VALUES (...), (...)` batched at e.g. 1000 rows per statement (~100× speedup expected on localhost), or `COPY FROM STDIN` (~1000× expected). Multi-row INSERT is simpler and stays within sqlx's typed API; COPY needs `PgCopyIn` wire-format work. Tracking as a separate refactor — `apply_envelope` currently assumes one row per call and the inner loop would need to group by column-set first (delta segments may carry rows with different shapes).

### `BENCH_ROWS=1000000` — the §9-shaped row

Still not attempted on laptop hardware. With the snapshot fix the bootstrap is now linear, but restore's O(N) round-trips dominate. Re-run after the restore-side batching lands; that's the prerequisite for a representative §9 number.

## Raw Criterion micro-benchmarks (2026-05-20)

Reduced sample size (`--sample-size 10 --measurement-time 1 --warm-up-time 1`) for quick capture; values are the median of the [low high] band Criterion reports.

```
hash/1 KB                770 ns
hash/64 KB              27.6 µs
hash/1 MB                446 µs
blob_put/1 KB            833 µs
blob_put/64 KB           1.17 ms
blob_put/512 KB          2.68 ms
blob_roundtrip/1 KB      58.3 µs
blob_roundtrip/64 KB     1.10 ms
blob_roundtrip/512 KB    7.90 ms
tree_hash/10             2.75 µs
tree_hash/100           20.5 µs
tree_hash/1000           190 µs
tree_put/10              960 µs
tree_put/100             991 µs
tree_put/500            1.20 ms
commit/prompts_only     2.35 ms
```

Source: [`crates/agentic-core/benches/store.rs`](../../crates/agentic-core/benches/store.rs).

## Coverage gaps (tracked)

- **Restore round-trip count.** Restore loops every row in the manifest and issues one parameterised INSERT per row through the open transaction. At 100K rows this is 12.29 s — linearly ~120 s at 1M, which doesn't meet §9. The fix is multi-row `INSERT VALUES (...), (...)` batched at ≈ 1000 rows per statement (~100× expected speedup on localhost) or `COPY FROM STDIN` (~1000× expected). Multi-row INSERT is simpler — `apply_envelope` currently assumes one row per call and the inner loop would need to group by column-set first (delta segments may carry rows with different shapes). Track as a separate refactor on `agentic-memory::restore`.
- **Snapshot O(n²) fix landed.** `bootstrap_table` previously called `canonical_size()` (== full re-serialisation of the segment) on every row to decide when to seal. The fix accumulates a running byte counter as rows land; 100K snapshot is now 8.87 ms (was unrunnable). This is in the diff that ships this benchmarks.md update.
- **p99 per-blob write number** requires Criterion's `--save-baseline` + raw-sample analysis; currently we publish the median only.
- **Snapshot storage amortisation** (< 2× changed data) needs a segment-size sampling job that walks the object store after a series of commits with varying delta sizes.
- **Representative-cloud-instance run** of `pg_snapshot_perf_smoke` at `BENCH_ROWS=1000000` is the unambiguous §9 commitment. Laptop numbers here are a "ranged signal, not an SLA" — useful for spotting regressions, not for public commitments.

## How to reproduce

```bash
# Micro-benchmarks (CI-safe, ~30 s):
cargo bench -p agentic-core --bench store -- \
  --sample-size 10 --measurement-time 1 --warm-up-time 1

# Postgres-integration smoke (requires Docker or Podman):
docker compose -f examples/langgraph-rollback/docker-compose.yml up -d
docker exec agentic-demo-pg psql -U agentic -d agentic \
    -c "CREATE EXTENSION IF NOT EXISTS vector"
DATABASE_URL=postgres://agentic:agentic@localhost:54322/agentic \
BENCH_ROWS=100000 \
    cargo test -p agentic-memory --test integration --release -- \
    --ignored --test-threads=1 --nocapture pg_snapshot_perf_smoke

# Demo end-to-end timing (requires podman + Postgres):
cd examples/langgraph-rollback
python -m venv .venv && .venv/bin/pip install -e '../../sdk/python[langgraph]' 'psycopg[binary]>=3.1'
PATH="$PWD/.venv/bin:$PATH" time ./scripts/run-demo.sh
```
