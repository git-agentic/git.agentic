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
#  8.5. simulate git revert  ← still broken (contaminated memory)
#   9. `agentic diff` baseline → bad   ← multi-dimensional regression
#  10. `agentic rollback baseline --yes`
#  11. agent ask again  ← empathetic answer is back
#  12. cleanup

set -euo pipefail

DEMO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "${DEMO_DIR}/../.." && pwd)"
COMPOSE_FILE="${DEMO_DIR}/docker-compose.yml"

DATABASE_URL_BASE="postgres://agentic:agentic@localhost:54322/agentic"
export DATABASE_URL="${DATABASE_URL_BASE}"
export AGENTIC_SOCKET="${DEMO_DIR}/.agentic/agenticd.sock"
AGENTICD_BIN="${REPO_ROOT}/target/release/agenticd"
AGENTIC_BIN="${REPO_ROOT}/target/release/agentic"

# Container runtime selection. The README advertises "podman OR docker";
# pick whichever is present. Both speak compose-file v3 well enough for
# the demo's needs.
if command -v podman >/dev/null 2>&1; then
    CONTAINER_RUNTIME=podman
elif command -v docker >/dev/null 2>&1; then
    CONTAINER_RUNTIME=docker
else
    echo "error: neither podman nor docker found on PATH" >&2
    exit 1
fi
compose() { "${CONTAINER_RUNTIME}" compose "$@"; }
container_run() { "${CONTAINER_RUNTIME}" "$@"; }

DAEMON_PID=""
cleanup() {
    if [[ -n "${DAEMON_PID}" ]]; then
        kill "${DAEMON_PID}" 2>/dev/null || true
    fi
    compose -f "${COMPOSE_FILE}" down -v >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

step() { printf "\n=== %s ===\n" "$*"; }

step "1. starting Postgres + pgvector (${CONTAINER_RUNTIME})"
compose -f "${COMPOSE_FILE}" up -d >/dev/null
pg_ready=false
for i in 1 2 3 4 5 6 7 8 9 10; do
    if container_run exec agentic-demo-pg pg_isready -U agentic -d agentic >/dev/null 2>&1; then
        echo "ready after ${i}s"
        pg_ready=true
        break
    fi
    sleep 1
done

if [[ "${pg_ready}" != "true" ]]; then
    echo "Postgres did not become ready after 10 seconds; aborting." >&2
    exit 1
fi

step "2. seeding episodes (baseline state)"
# Restore baseline prompt in case a prior run left the bad one in place.
git -C "${REPO_ROOT}" checkout -- "examples/langgraph-rollback/prompts/system.txt" 2>/dev/null || true
psql "${DATABASE_URL}" -v ON_ERROR_STOP=1 -f "${DEMO_DIR}/seed.sql" >/dev/null

step "3. building + starting agenticd"
( cd "${REPO_ROOT}" && cargo build --release -p agenticd -p agentic-cli >/dev/null )
rm -rf "${DEMO_DIR}/.agentic"
"${AGENTIC_BIN}" --repo "${DEMO_DIR}" init >/dev/null
"${AGENTICD_BIN}" --repo "${DEMO_DIR}" --postgres "${DATABASE_URL}" --tables episodes:id \
    > "${DEMO_DIR}/.agentic/daemon.log" 2>&1 &
DAEMON_PID=$!

# Wait for the daemon's Unix socket to appear before continuing. Fixed
# `sleep 1` was flaky on cold caches / slow systems; this also catches
# an early daemon crash and prints the log so the failure is
# actionable.
socket_ready=false
for i in 1 2 3 4 5 6 7 8 9 10; do
    if [[ -S "${AGENTIC_SOCKET}" ]]; then
        socket_ready=true
        break
    fi
    if ! kill -0 "${DAEMON_PID}" 2>/dev/null; then
        echo "error: agenticd exited before binding the socket. Log:" >&2
        cat "${DEMO_DIR}/.agentic/daemon.log" >&2
        exit 1
    fi
    sleep 0.5
done
if [[ "${socket_ready}" != "true" ]]; then
    echo "error: agenticd did not bind ${AGENTIC_SOCKET} within 5s. Log:" >&2
    cat "${DEMO_DIR}/.agentic/daemon.log" >&2
    exit 1
fi

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

step "8.5. simulate git revert — shows code-only revert is insufficient"
# Restore the prompt file to baseline (simulating what a code-only rollback, e.g.
# git revert, achieves: the file on disk goes back to its original content).
# This is a local working-tree restore, not a revert commit.
if ! git -C "${REPO_ROOT}" checkout -- "examples/langgraph-rollback/prompts/system.txt" 2>/dev/null; then
    echo "error: could not restore prompts/system.txt to baseline" >&2
    exit 1
fi
"${DEMO_DIR}/scripts/redeploy.sh"
"${DEMO_DIR}/scripts/ask.sh" "I'm thinking about cancelling my subscription."
# Contaminated memory rows are still in Postgres — the answer is still broken.

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
