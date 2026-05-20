#!/usr/bin/env bash
# Invoke the demo agent with one user message.
#
# Usage: ./scripts/ask.sh "<message>"
#
# Reads DATABASE_URL + AGENTIC_SOCKET from the environment (set by
# run-demo.sh) and routes through the LangGraph + AgenticCheckpointer
# pipeline so every ask becomes a Commit.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

: "${DATABASE_URL:?DATABASE_URL must point at the demo Postgres}"
: "${AGENTIC_SOCKET:=${here}/.agentic/agenticd.sock}"
export DATABASE_URL AGENTIC_SOCKET

python "${here}/agent.py" "$@"
