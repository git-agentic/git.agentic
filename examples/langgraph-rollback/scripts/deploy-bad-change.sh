#!/usr/bin/env bash
# Apply the bad change — the simulated "small prompt tweak" that
# triggers the broken-prompt scenario.
#
# Three things land at once, the same way they would in a real deploy:
#
#   1. The sycophantic system prompt overwrites the empathetic one.
#   2. A schema migration adds a NOT NULL `sentiment` column to the
#      agent's `episodes` memory table and records itself in
#      `agentic_migrations`. Its `.down.sql` is registered under
#      `.agentic/schema/` so `agentic rollback` can reverse it.
#   3. Contaminated rows are INSERTed into `episodes` — together with
#      (2) these are what make a code-only `git revert` insufficient.
#
# Neither change is committed to agenticd here; run-demo.sh does that
# right after, so the bad state lands as a single commit.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

: "${DATABASE_URL:?DATABASE_URL must point at the demo Postgres}"

echo "→ swapping prompts/system.txt for the sycophantic variant"
cp "${here}/bad-change/system.txt" "${here}/prompts/system.txt"

echo "→ applying migration 001_add_sentiment (episodes.sentiment NOT NULL)"
psql "${DATABASE_URL}" -v ON_ERROR_STOP=1 -1 -f "${here}/bad-change/001_add_sentiment.up.sql" >/dev/null
# Operator-owned reverse migration. .agentic is 0700 and already exists
# (run-demo.sh runs `agentic init` first); the daemon reads the file at
# rollback time, so no restart is needed.
mkdir -p "${here}/.agentic/schema"
cp "${here}/bad-change/001_add_sentiment.down.sql" "${here}/.agentic/schema/001_add_sentiment.down.sql"

echo "→ INSERTing 5 contaminated rows into episodes"
psql "${DATABASE_URL}" -v ON_ERROR_STOP=1 -f "${here}/bad-change/seed.sql" >/dev/null

echo "✓ bad change deployed"
