# Benchmarks

**Last measured:** 2026-05-22 on Apple Silicon (8-core, Docker Desktop on macOS host; demo's Postgres+pgvector at `localhost:54322`).
**Methodology:** Criterion (`cargo bench -p agentic-core --bench store`) for micro-benchmarks; a new `#[ignore]`-gated integration test, `pg_snapshot_perf_smoke`, drives the §9-shaped Postgres path against a real `PostgresAdapter`; the broken-prompt demo end-to-end run (`scripts/run-demo.sh`) for the operator-level integration timing.

This is a **measured early sanity-check** of where performance sits relative to the [`snapshot-model.md`](snapshot-model.md) §9 targets — not yet the published v1.0.x commitment. The numbers below come from a developer machine (laptop-class), not a representative cloud instance, and the §9 line that asks for *1M-row pgvector + 100 deltas* is approximated by *N-row episodes (text-only) without deltas*. Treat the §9-shape rows as "no obvious blocker at the laptop-scale shape we've measured"; the published commitment lands when we have a CI-driven harness on a known instance class.

## Targets vs measured

| Operation | Target (`snapshot-model` §9) | Measured | Notes |
|---|---|---|---|
| `commit` (memory snapshot of N-row pgvector) | < 2 s @ 1M rows + 100 deltas | **39 ms @ 10K rows** (laptop); larger N untested | text-only `episodes` schema; no embeddings, no deltas |
| `commit` (prompts-only, 16-byte system prompt, no memory) | n/a — degenerate input | **2.3 ms median** (Criterion) | ✓ no obvious blocker; not a §9 measurement |
| `rollback` (memory restore of N-row pgvector) | < 5 s @ 1M rows + 10 commits | **1.33 s @ 10K rows** (laptop); ⚠ 100K run hit CPU wall at >10 min | linear extrapolation does **not** meet §9 at 1M; needs profiler pass |
| `rollback` (broken-prompt demo, end-to-end) | n/a — demo scale | **~1 s observed** in `run-demo.sh`; ~6.7 s total for 12 demo steps incl. Postgres bring-up | ✓ demo discipline met |
| `diff` (manifest hash compare across snapshots) | < 1 s @ 1M-row pgvector | **66 µs @ 10K rows** (laptop) | manifest comparison doesn't touch Postgres — cost scales with manifest size (segment-entry count), not row count or DB I/O. Trivially within target at the segment counts we measure. |
| `diff` (demo scenario) | n/a — demo scale | sub-second (observed) | ✓ demo discipline met |
| Per-blob write, **median** | < 5 ms per row (p99) | **2.7 ms median (512 KB)** / **0.83 ms median (1 KB)** | ⚠ Criterion reports median, not p99 — p99 needs `--save-baseline` raw-sample analysis (tracked) |
| Snapshot storage amortized | < 2× changed data | _not yet measured_ | ⚠ pending segment-size sampling job |

## Postgres-integration smoke (`pg_snapshot_perf_smoke`)

The harness lives at [`crates/agentic-memory/tests/integration.rs`](../../crates/agentic-memory/tests/integration.rs); see "How to reproduce" below for the invocation. Each run seeds N rows into a fresh schema, takes a snapshot, restores it (no-op), takes a second snapshot, and computes the manifest-hash diff. Operations are wall-clock-timed via `std::time::Instant`, not statistically sampled — these are single-shot wall numbers, not Criterion-style medians.

Numbers below are from a developer laptop (Apple Silicon, 8-core; Postgres 16 + pgvector in Docker Desktop on the same host).

### `BENCH_ROWS=10000` (2026-05-22)

| Operation | Measured | §9 target |
|---|---|---|
| bulk seed (Postgres INSERTs, 10000 rows) | 30 ms | n/a — setup cost |
| `snapshot()` | 39 ms | < 2 s @ 1M-row pgvector |
| `restore()` (no-op replay) | 1.33 s | < 5 s @ 1M-row + 10 commits |
| `diff` (manifest hash compare) | 66 µs | < 1 s @ 1M-row pgvector — manifest-size-bounded, not row-bounded |

Both manifest hashes match across the snapshot → restore → snapshot cycle (round-trip determinism).

### `BENCH_ROWS=100000` (2026-05-22) — **interrupted**

Attempted with a release build against the same Docker-hosted Postgres. The process stayed pinned at 100% CPU (Rust-side, not waiting on Postgres) for **>10 minutes** before being killed. Postgres' `pg_stat_activity` showed zero active queries during that window, so the bottleneck is in the snapshot/restore code path itself rather than the database round-trip. This means:

- Linear extrapolation from the 10K row (restore ≈ 1.33 s) predicts ≈ 133 s at 1M rows, which **does not meet the §9 < 5 s target** on this hardware shape.
- The 100K interruption suggests the curve is **super-linear** — restore is doing per-row work that compounds (likely the JSON-serialize → BLAKE3 → zstd path on a single large segment, or repeated full-segment hashing).
- Treat the §9 commitment of "< 5 s rollback at 1M rows + 10 commits" as **unverified against current code on laptop-class hardware**; a profiler pass to localise the hot spot, and a re-run on a representative cloud instance, are both prerequisites to publishing this as a v1.0.x SLA.

### `BENCH_ROWS=1000000` — the §9-shaped row

Not attempted at this hardware shape. The 100K interruption above is the upper bound of what's tractable on the current laptop; a re-run is meaningful only after the profiler-led optimisation that the 100K finding implies.

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

- **Snapshot/restore scaling.** The 100K-row interruption on the laptop (>10 min CPU-bound) is a red flag — the per-row cost of segment assembly + hash + compression appears to compound super-linearly. Before publishing a v1.0.x rollback SLA, this needs a profiler pass to localise the hot spot (suspect candidates: JSON serialisation of a single large segment, full-segment BLAKE3 on every snapshot, zstd compression of the manifest). Track as a follow-up.
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
