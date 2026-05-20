#!/usr/bin/env bash
# Apply the bad change — the simulated "small prompt tweak" that
# triggers the broken-prompt scenario.
#
# Two things land at once, the same way they would in a real deploy:
#
#   1. The sycophantic system prompt overwrites the empathetic one.
#   2. Contaminated rows are INSERTed into the agent's `episodes`
#      memory table — these are what makes a code-only `git revert`
#      insufficient.
#
# Neither change is committed to agenticd here; run-demo.sh does that
# right after, so the bad state lands as a single commit.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

: "${DATABASE_URL:?DATABASE_URL must point at the demo Postgres}"

echo "→ swapping prompts/system.txt for the sycophantic variant"
cp "${here}/bad-change/system.txt" "${here}/prompts/system.txt"

echo "→ INSERTing 5 contaminated rows into episodes"
psql "${DATABASE_URL}" -v ON_ERROR_STOP=1 -f "${here}/bad-change/seed.sql" >/dev/null

echo "✓ bad change deployed"
