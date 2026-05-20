# Benchmarks

**Last measured:** 2026-05-20 on Apple Silicon (8-core, libkrun-hosted podman).
**Methodology:** Criterion (`cargo bench -p agentic-core --bench store`) for micro-benchmarks; the broken-prompt demo end-to-end run (`scripts/run-demo.sh`) for integration timings.

This is an **early sanity-check** of where measured performance sits relative to the `docs/architecture/snapshot-model.md` §9 targets, not yet the published commitment. The Criterion run below used reduced sample sizes for quick capture, and the rows covering production-shape inputs (1M-row pgvector with 100 deltas) are still pending an integration benchmark. Treat the numbers as a "no obvious blocker" signal; the published commitment lands once the §9-shaped harness exists.

## Targets vs measured

| Operation | Target (snapshot-model §9) | Measured | Notes |
|---|---|---|---|
| `commit` (1M-row pgvector, 100 deltas) | < 2 s | _not yet benchmarked_ | ⚠ pending Postgres integration bench |
| `commit` (prompts-only, 16-byte system prompt, no memory) | n/a — degenerate input | **2.3 ms median** | ✓ no obvious blocker; **not a §9 measurement** |
| `rollback` (1M-row pgvector, 10 commits) | < 5 s | _not yet benchmarked_ | ⚠ pending Postgres integration bench |
| `rollback` (broken-prompt demo, end-to-end) | n/a — demo scale | **~1 s observed** in run-demo.sh; ~6.7 s total for 12 demo steps incl. Postgres bring-up | ✓ demo discipline met |
| `diff` (1M-row pgvector) | < 1 s | _not yet benchmarked_ | ⚠ pending Postgres integration bench |
| `diff` (demo scenario) | n/a — demo scale | sub-second (observed) | ✓ demo discipline met |
| Per-blob write, **median** | < 5 ms per row (p99) | **2.7 ms median (512 KB)** / **0.83 ms median (1 KB)** | ⚠ Criterion reports median, not p99 — p99 needs `--save-baseline` raw-sample analysis |
| Snapshot storage amortized | < 2× changed data | _not yet measured_ | ⚠ pending segment-size sampling job |

## Raw numbers (Criterion, 2026-05-20)

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

Source: `crates/agentic-core/benches/store.rs`.

## Coverage gaps (intentional, tracked)

The Criterion suite intentionally avoids Postgres so it runs CI-safely with no network and no Docker. The four lines marked _pending_ require a Postgres + pgvector integration benchmark harness (one fresh DB per run, 1M rows of vector data, controlled delta sizes). Adding it is on the Week-A backlog as item A3-followup; not blocking the broken-prompt demo, but blocking the "commits to numbers publicly" line in `snapshot-model.md` §9.

## How to reproduce

```bash
# Micro-benchmarks (CI-safe, ~30 s):
cargo bench -p agentic-core --bench store -- \
  --sample-size 10 --measurement-time 1 --warm-up-time 1

# Demo end-to-end timing (requires podman + Postgres):
cd examples/langgraph-rollback
python -m venv .venv && .venv/bin/pip install -e '../../sdk/python[langgraph]' 'psycopg[binary]>=3.1'
PATH="$PWD/.venv/bin:$PATH" time ./scripts/run-demo.sh
```
