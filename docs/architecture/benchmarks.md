# Benchmarks

**Last measured:** 2026-05-22 on Apple Silicon (8-core, Docker Desktop on macOS host; demo's Postgres+pgvector at `localhost:54322`).
**Methodology:** Criterion (`cargo bench -p agentic-core --bench store`) for micro-benchmarks; a new `#[ignore]`-gated integration test, `pg_snapshot_perf_smoke`, drives the §9-shaped Postgres path against a real `PostgresAdapter`; the broken-prompt demo end-to-end run (`scripts/run-demo.sh`) for the operator-level integration timing.

This is a **measured early sanity-check** of where performance sits relative to the [`snapshot-model.md`](snapshot-model.md) §9 targets — not yet the published v1.0.x commitment. The numbers below come from a developer machine (laptop-class), not a representative cloud instance, and the §9 line that asks for *1M-row pgvector + 100 deltas* is approximated by *N-row episodes (text-only) without deltas*. Treat the §9-shape rows as "no obvious blocker at the laptop-scale shape we've measured"; the published commitment lands when we have a CI-driven harness on a known instance class.

## Targets vs measured

| Operation | Target (`snapshot-model` §9) | Measured | Notes |
|---|---|---|---|
| `commit` (memory snapshot of N-row pgvector) | < 2 s @ 1M rows + 100 deltas | **5 ms @ 10K / 5.6 ms @ 100K / 18.81 ms @ 1M** (laptop) | text-only `episodes` schema; no embeddings, no deltas. ✓ comfortably within target. |
| `commit` (prompts-only, 16-byte system prompt, no memory) | n/a — degenerate input | **2.3 ms median** (Criterion) | ✓ no obvious blocker; not a §9 measurement |
| `rollback` (memory restore of N-row pgvector) | < 5 s @ 1M rows + 10 commits | **102 ms @ 10K / 1.07 s @ 100K / 10.34 s @ 1M** (laptop, batched-INSERT) | ⚠ 1M still ~2× over §9 on this hardware shape, but linear and predictable. Multi-row INSERT closed the 12× gap from the per-row path. `COPY FROM STDIN` was prototyped 2026-05-24 and **did not help on this hardware** (see "Coverage gaps" below for measured numbers and reasoning). A representative cloud-class machine remains the unambiguous §9-meeting path. |
| `rollback` (broken-prompt demo, end-to-end) | n/a — demo scale | **~1 s observed** in `run-demo.sh`; ~6.7 s total for 12 demo steps incl. Postgres bring-up | ✓ demo discipline met |
| `diff` (manifest hash compare across snapshots) | < 1 s @ 1M-row pgvector | **3 µs @ 10K / 4 µs @ 100K / 5 µs @ 1M** (laptop) | manifest comparison doesn't touch Postgres — cost scales with manifest size (segment-entry count), not row count or DB I/O. Trivially within target. |
| `diff` (demo scenario) | n/a — demo scale | sub-second (observed) | ✓ demo discipline met |
| Per-blob write, **median** | < 5 ms per row (p99) | **2.7 ms median (512 KB)** / **0.83 ms median (1 KB)** | ⚠ Criterion reports median, not p99 — p99 needs `--save-baseline` raw-sample analysis (tracked) |
| Snapshot storage amortized | < 2× changed data | _not yet measured_ | ⚠ pending segment-size sampling job |

## Postgres-integration smoke (`pg_snapshot_perf_smoke`)

The harness lives at [`crates/agentic-memory/tests/integration.rs`](../../crates/agentic-memory/tests/integration.rs); see "How to reproduce" below for the invocation. Each run seeds N rows into a fresh schema, takes a snapshot, restores it (no-op), takes a second snapshot, and computes the manifest-hash diff. Operations are wall-clock-timed via `std::time::Instant`, not statistically sampled — these are single-shot wall numbers, not Criterion-style medians.

Numbers below are from a developer laptop (Apple Silicon, 8-core; Postgres 16 + pgvector in Docker Desktop on the same host).

### `BENCH_ROWS=10000` (2026-05-22, post-snapshot-fix + batched restore)

| Operation | Measured | §9 target |
|---|---|---|
| bulk seed (Postgres INSERTs, 10000 rows) | 22 ms | n/a — setup cost |
| `snapshot()` | 5 ms | < 2 s @ 1M-row pgvector |
| `restore()` (no-op replay) | **102 ms** (was 1.33 s; 13× from batched INSERT) | < 5 s @ 1M-row + 10 commits |
| `diff` (manifest hash compare) | 3 µs | < 1 s @ 1M-row pgvector — manifest-size-bounded, not row-bounded |

Both manifest hashes match across the snapshot → restore → snapshot cycle (round-trip determinism).

### `BENCH_ROWS=100000` (2026-05-22, post-snapshot-fix + batched restore)

| Operation | Measured | §9 target |
|---|---|---|
| bulk seed (Postgres INSERTs, 100000 rows) | 210 ms | n/a — setup cost |
| `snapshot()` | 5.60 ms | < 2 s @ 1M-row pgvector |
| `restore()` (no-op replay) | **1.07 s** (was 12.29 s; 11.5× from batched INSERT) | < 5 s @ 1M-row + 10 commits |
| `diff` (manifest hash compare) | 4 µs | < 1 s @ 1M-row pgvector |

Manifest hashes match across the snapshot → restore → snapshot cycle.

**Diagnosis 1 — Snapshot O(n²) on segment size** *(fixed earlier)*: `bootstrap_table` called `current.canonical_size()` after every row to decide whether to seal a segment. `canonical_size()` re-serialised the **entire growing segment** on every iteration. With the default 64 MiB segment target, a 100K-row bootstrap re-encoded the segment ~100K times for cumulative O(n²) cost. Replaced with a running-byte counter that accumulates each row's encoded size as it lands.

**Diagnosis 2 — Restore O(n) round-trips** *(fixed in this revision)*: `restore_manifest`'s inner loop issued one parameterised INSERT per row, hitting a hard floor of N × Postgres-round-trip-cost. `apply_segment_rows` now groups consecutive same-shape envelopes (same column-set + same upsert-vs-delete mode) and emits one multi-row `INSERT … VALUES (...), (...), … ON CONFLICT` per group, capped at 1000 rows per statement to stay well under Postgres's 65535-parameter ceiling. Delete envelopes batch as `DELETE … WHERE pk IN (...)`. Order across groups is preserved — a shape change always flushes — so the streamer's per-row event ordering still drives the final state.

### `BENCH_ROWS=1000000` — the §9-shaped row, now measurable

| Operation | Measured | §9 target |
|---|---|---|
| bulk seed (Postgres INSERTs, 1000000 rows) | 1.78 s | n/a — setup cost |
| `snapshot()` | **18.81 ms** | < 2 s @ 1M-row pgvector — ✓ |
| `restore()` (no-op replay) | **10.34 s** | < 5 s @ 1M-row + 10 commits — ⚠ ~2× over |
| `diff` (manifest hash compare) | 5 µs | < 1 s @ 1M-row pgvector — ✓ |

Pre-batched, 1M was unrunnable on this hardware (extrapolated to ≈ 130 s). Now linear and predictable: 1M = ~10 × 100K, which matches the measurement.

The remaining ~2× to hit §9 on this laptop was hypothesised to come from `COPY FROM STDIN`. That prototype landed and was reverted 2026-05-24 — see "Coverage gaps" for the measured-not-faster result and the cloud-class-machine framing. At a representative cloud instance class — fewer noisy neighbours, faster fsync, larger shared_buffers — the present multi-row-INSERT shape may already meet §9 on its own.

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

## `get_raw` integrity-verification overhead (2026-07-10)

Audit finding #3 added a `Hash::of(bytes)` check to every `get_raw` read
(segments + manifests on the restore path). The added cost is exactly one
BLAKE3 pass over the decompressed bytes. Wall timings (release laptop;
200-iter mean per size):

```
size        get_raw (with verify)   added Hash::of      added share
4 KB                33.7 µs              3.42 µs            10.2 %
64 KB               77.7 µs             39.9 µs             51.4 %
1 MB               577 µs              488 µs               84.6 %
```

BLAKE3 runs at ~2 GB/s here, so the verification adds ~0.5 ms per MiB read.
The added-share climbs with object size only because `get_raw` itself is
cheap (file read + zstd decode); in absolute terms a full restore's added
cost is `total_restored_bytes / 2 GB/s` — sub-millisecond at demo scale and
a few milliseconds even for large manifests. Relative to the demo-scale
rollback target (< 5 s), Postgres INSERT replay is still dominant (102 ms @
10K rows; 10.34 s @ 1M rows on this laptop, above). The §9 rollback/commit/write-overhead targets are
unaffected.

## Coverage gaps (tracked)

- **Restore batched-INSERT landed.** `apply_segment_rows` now groups consecutive same-shape envelopes and emits one multi-row `INSERT ... VALUES (...), (...)` (or `DELETE ... WHERE pk IN (...)` for deletes) per group, capped at 1000 rows / 60000 params per statement. 100K restore went from 12.29 s to 1.07 s (11.5×); 1M is now tractable at 10.34 s on the laptop.
- **Restore `COPY FROM STDIN` was prototyped 2026-05-24 and reverted.** The hypothesis was a ~10× speed-up on top of batched INSERT; the measurement instead showed flat-to-15%-slower on this hardware:
  - 10K: 102 ms → 104 ms (+2%)
  - 100K: 1.07 s → 1.06 s (flat)
  - 1M: 10.34 s → 11.88 s (**+15% slower**)

  Diagnosis: on Docker-Desktop Postgres at localhost, the bottleneck for restore is per-row write cost (WAL emission + btree insert), not the frontend/backend protocol overhead COPY skips. Multi-row INSERT at 1000 rows/statement already amortises the protocol round-trip; COPY's text-encoding pass added CPU work in the hot loop without saving anything Postgres-side. Manifest hashes stayed byte-identical, so the change was correct — just not faster.

  What the measurement does NOT rule out: COPY winning on a representative cloud-class instance where (a) network RTT is higher than localhost IPC, (b) WAL fsync overhead is amortised by group commit, or (c) the binary COPY format (not yet tried) skips the text-encoding CPU cost. If a future revisit happens, start with binary COPY and run against managed Postgres, not Docker-Desktop.

  Worktree + branch were deleted; no PR opened. Recorded here so the next person doesn't repeat the experiment expecting a different result on the same hardware.
- **Snapshot O(n²) fix landed earlier.** `bootstrap_table` previously called `canonical_size()` (full re-serialisation) on every row. Replaced with a running-byte counter; 100K snapshot went from unrunnable to 8.87 ms (now 5.60 ms with measurement noise).
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
