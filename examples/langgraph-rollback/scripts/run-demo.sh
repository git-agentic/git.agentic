#!/usr/bin/env bash
# End-to-end "broken prompt" scenario.
#
# Brings up the demo from a clean slate, runs the full
# baseline → bad → rollback walkthrough, and tears down. Targets the
# week-11 roadmap promise: "runs reliably on a fresh machine in < 5 min
# from `git clone`".
#
# What it actually exercises:
#   1. podman compose up Postgres + pgvector
#   2. seed the agent's episodes table
#   3. build agenticd + start it bound to the demo repo
#   4. agent baseline ask
#   5. `agentic commit` baseline
#   6. deploy-bad-change.sh
#   7. agent bad ask  ← the hallucinated refund
#   8. `agentic commit` "bad change"
#   9. `agentic diff` baseline → bad   ← multi-dimensional regression
#  10. `agentic rollback baseline --yes`
#  11. agent ask again  ← empathetic answer is back
#  12. cleanup

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "${DEMO_DIR}/../.." && pwd)"
COMPOSE_FILE="${REPO_ROOT}/tests/fixtures/pg.yml"

DATABASE_URL_BASE="postgres://agentic:agentic@localhost:54321/agentic"
export DATABASE_URL="${DATABASE_URL_BASE}"
export AGENTIC_SOCKET="${DEMO_DIR}/.agentic/agenticd.sock"
AGENTICD_BIN="${REPO_ROOT}/target/release/agenticd"
AGENTIC_BIN="${REPO_ROOT}/target/release/agentic"

DAEMON_PID=""
cleanup() {
    if [[ -n "${DAEMON_PID}" ]]; then
        kill "${DAEMON_PID}" 2>/dev/null || true
    fi
    podman compose -f "${COMPOSE_FILE}" down -v >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

step() { printf "\n=== %s ===\n" "$*"; }

step "1. starting Postgres + pgvector"
podman compose -f "${COMPOSE_FILE}" up -d >/dev/null
for i in 1 2 3 4 5 6 7 8 9 10; do
    if podman exec agentic-test-pg pg_isready -U agentic -d agentic >/dev/null 2>&1; then
        echo "ready after ${i}s"
        break
    fi
    sleep 1
done

step "2. seeding episodes (baseline state)"
# Restore baseline prompt in case a prior run left the bad one in place.
git -C "${REPO_ROOT}" checkout -- "${DEMO_DIR}/prompts/system.txt" 2>/dev/null || true
psql "${DATABASE_URL}" -v ON_ERROR_STOP=1 -f "${DEMO_DIR}/seed.sql" >/dev/null

step "3. building + starting agenticd"
( cd "${REPO_ROOT}" && cargo build --release -p agenticd -p agentic-cli >/dev/null )
rm -rf "${DEMO_DIR}/.agentic"
"${AGENTIC_BIN}" --repo "${DEMO_DIR}" init >/dev/null
"${AGENTICD_BIN}" --repo "${DEMO_DIR}" --postgres "${DATABASE_URL}" --tables episodes:id \
    > "${DEMO_DIR}/.agentic/daemon.log" 2>&1 &
DAEMON_PID=$!
sleep 1

step "4. baseline ask"
"${DEMO_DIR}/scripts/ask.sh" "I'm thinking about cancelling my subscription."

step "5. commit baseline"
"${AGENTIC_BIN}" --repo "${DEMO_DIR}" commit -m "v0.7 baseline" \
    --model "anthropic:claude-opus:2026-05-01"
BASELINE=$("${AGENTIC_BIN}" --repo "${DEMO_DIR}" status | sed -n 's/.*→ //p')
echo "baseline = ${BASELINE}"

step "6. deploy bad change"
"${DEMO_DIR}/scripts/deploy-bad-change.sh"

step "7. bad ask  (hallucinated refund + contaminated memory)"
"${DEMO_DIR}/scripts/ask.sh" "I'm thinking about cancelling my subscription."

step "8. commit bad change"
"${AGENTIC_BIN}" --repo "${DEMO_DIR}" commit -m "v0.8 friendlier prompt" \
    --model "anthropic:claude-opus:2026-05-01"

step "9. diff baseline → bad"
"${AGENTIC_BIN}" --repo "${DEMO_DIR}" diff "${BASELINE}" HEAD

step "10. rollback to baseline"
"${AGENTIC_BIN}" --repo "${DEMO_DIR}" rollback "${BASELINE}" --yes

step "11. ask again — empathetic answer + clean memory"
"${DEMO_DIR}/scripts/ask.sh" "I'm thinking about cancelling my subscription."

step "12. log"
"${AGENTIC_BIN}" --repo "${DEMO_DIR}" log --oneline | head -10

echo
echo "✓ broken-prompt demo complete"
