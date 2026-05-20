# The "broken prompt" demo

The canonical `git.agentic` demo, end-to-end. A tiny LangGraph
customer-support agent gets broken by a deceptively-small change to
its system prompt — bundled with contaminated rows in its `episodes`
memory table. `git revert` of the prompt change alone does not fix
it. One `agentic rollback` does, in seconds.

See [`docs/product/demo-scenario.md`](../../docs/product/demo-scenario.md)
for the design and the 90-second pitch shape this implements.

## What you need

- A POSIX shell, `podman` (or `docker`) with compose support, `cargo`,
  Python 3.10+, `psql`.
- Two terminals are not required — `scripts/run-demo.sh` orchestrates
  everything.

The demo ships its own `docker-compose.yml` (Postgres + pgvector on
port 54322, container `agentic-demo-pg`), so it does not share state
with the Rust integration-test database.

## 60-second walkthrough

```bash
cd examples/langgraph-rollback
python -m venv .venv
.venv/bin/pip install -e '../../sdk/python[langgraph]' 'psycopg[binary]>=3.1'
PATH="$PWD/.venv/bin:$PATH" ./scripts/run-demo.sh
```

The script:

1. brings up Postgres + pgvector via `podman compose`,
2. seeds 5 baseline `episodes` rows,
3. builds `agenticd` and `agentic`, starts the daemon bound to this
   directory with `--postgres … --tables episodes:id`,
4. asks the agent *"I'm thinking about cancelling my subscription."* —
   gets an empathetic answer,
5. `agentic commit -m "v0.7 baseline"`,
6. `deploy-bad-change.sh` swaps in the sycophantic prompt + inserts
   five contaminated rows ("Refund of $499 processed for customer
   yesterday — confirmed.", etc.),
7. asks again — the agent now offers an unauthorised refund and
   confidently cites the fake refund row from memory,
8. `agentic commit -m "v0.8 friendlier prompt"`,
9. `agentic diff baseline HEAD` shows the prompts tree, the memory
   manifest, and the LangGraph checkpoint blob have all moved,
10. `agentic rollback baseline --yes` — restores prompts on disk,
    truncates and replays `episodes` from the baseline manifest inside
    one transaction, forward-records a new commit on the branch,
11. asks again — empathetic answer is back; the contaminated memory
    row is gone.

The fix is one command. `git revert` alone never restores step (11).

## What it actually exercises

| Dimension | How it changes between baseline and bad | How rollback restores it |
|---|---|---|
| `prompts` | `prompts/system.txt` overwritten with the sycophantic variant | `agentic rollback` writes the baseline tree back to disk |
| `memory_snapshot` | 5 contaminated rows INSERT'd into `episodes` | `MemoryAdapter::restore` TRUNCATEs and replays from the baseline `SegmentManifest` inside one transaction |
| `tools` | unchanged | n/a |
| `model` | recorded in each commit; unchanged in this demo | rollback notes "operator must redeploy if it changed" |
| `code_sha` | recorded in each commit (from `git rev-parse HEAD`) | rollback target's code SHA threaded into the forward-recorded commit |
| `schema_version` | unchanged in this demo (`agentic_schema_version()` returns `0.0.0`) | gated on rollback; reverse migration runner is a planned follow-up |

The LangGraph wiring is real: every agent invocation runs through
`AgenticCheckpointer`, so `agentic log` shows one commit per graph step
on a per-thread branch (`langgraph/<thread-hash>`). The rollback
forward-records a new commit on that same branch, so the agent's
history reflects the rollback action rather than rewriting it.

## What's deliberately fake

- **No LLM.** The "model" is a deterministic function in `agent.py`
  that branches on which sentinel keyword appears in `system.txt` and
  splices the most recent `episodes` row into its reply. This is
  enough to make the broken behaviour reproducible without API keys
  and without flaky model output. The rollback story is identical for
  a real LLM-backed agent — the dimensions snapshotted are the same.
- **No schema migration.** The demo spec describes a `sentiment NOT
  NULL` column added in v0.8 and reverse-migrated on rollback; that
  requires the reverse SQL migration runner which is the next planned
  ADR-0002 §5 follow-up. Today the demo exercises prompts + memory
  rollback only.
- **Compose covers the database only.** `docker-compose.yml` starts
  Postgres + pgvector. `agenticd` and the Python agent are built and
  run locally by `scripts/run-demo.sh` — they are not containerised.

## Files

```
examples/langgraph-rollback/
├── agent.py              the LangGraph agent + fake LLM
├── docker-compose.yml    Postgres + pgvector on port 54322
├── prompts/
│   └── system.txt        baseline empathetic system prompt
├── bad-change/
│   ├── system.txt        sycophantic variant the bad change ships
│   └── seed.sql          contaminated rows the bad change INSERTs
├── seed.sql              baseline episodes + pgvector setup
├── scripts/
│   ├── ask.sh            invoke the agent with one user message
│   ├── deploy-bad-change.sh   apply prompt + memory contamination
│   ├── redeploy.sh       simulate code-only redeploy after git revert
│   └── run-demo.sh       full scenario orchestrator
├── pyproject.toml        Python deps for the demo
└── README.md             you are here
```

## Manual walkthrough

If you want to drive the steps yourself instead of via `run-demo.sh`:

```bash
# Run from the repo root:
cd examples/langgraph-rollback

# 1. Postgres (standalone compose on port 54322)
podman compose up -d

# 2. Build daemon + CLI
cd ../.. && cargo build --release -p agenticd -p agentic-cli && cd examples/langgraph-rollback

# 3. Seed baseline + start daemon
psql 'postgres://agentic:agentic@localhost:54322/agentic' -f seed.sql
../../target/release/agentic --repo . init
../../target/release/agenticd --repo . \
    --postgres 'postgres://agentic:agentic@localhost:54322/agentic' \
    --tables episodes:id &

# 4. Drive the scenario
export DATABASE_URL='postgres://agentic:agentic@localhost:54322/agentic'
./scripts/ask.sh "I'm thinking about cancelling."
../../target/release/agentic --repo . commit -m "baseline" \
    --model "anthropic:claude-opus:2026-05-01"
BASELINE=$(../../target/release/agentic --repo . status | sed 's/.*→ //')

./scripts/deploy-bad-change.sh
./scripts/ask.sh "I'm thinking about cancelling."
../../target/release/agentic --repo . commit -m "bad change" \
    --model "anthropic:claude-opus:2026-05-01"

# 4.5. Show that git revert alone does not fix it
git checkout -- prompts/system.txt
./scripts/redeploy.sh
./scripts/ask.sh "I'm thinking about cancelling."  # still broken

../../target/release/agentic --repo . diff "$BASELINE" HEAD
../../target/release/agentic --repo . rollback "$BASELINE" --yes
./scripts/ask.sh "I'm thinking about cancelling."  # fixed

# 5. Tear down
kill %1
podman compose down -v
```

## Verifying it on a fresh clone

Roadmap week 11 commits to "the demo runs reliably on a fresh machine
in < 5 minutes from `git clone`". This script meets that on machines
with `podman`, `cargo`, and Python 3.10+ already present. Cold-cache
`cargo build` dominates wall-clock time; everything else is seconds.

If `scripts/run-demo.sh` exits non-zero, the script logs and the
daemon log (`.agentic/daemon.log`) are the two places to look first.
