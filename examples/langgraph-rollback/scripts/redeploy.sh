#!/usr/bin/env bash
# Simulate a code-only redeploy after `git revert`.
#
# In a real team this would restart the agent process after the prompt
# file on disk has been reverted by `git revert`. The daemon keeps
# running — only the agent is "restarted" (here that means the next
# `./scripts/ask.sh` call naturally re-reads the prompt from disk).
#
# This script intentionally does nothing beyond verifying the daemon is
# still alive and echoing what changed. The point is to show that even
# with the prompt back to baseline, the contaminated memory rows in
# Postgres persist — so the agent's answers are still broken.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

: "${AGENTIC_SOCKET:=${here}/.agentic/agenticd.sock}"
export AGENTIC_SOCKET

if [[ ! -S "${AGENTIC_SOCKET}" ]]; then
    echo "error: daemon socket not found at ${AGENTIC_SOCKET}" >&2
    exit 1
fi

AGENTIC_BIN="${here}/../../target/release/agentic"
if [[ ! -x "${AGENTIC_BIN}" ]]; then
    AGENTIC_BIN="$(command -v agentic 2>/dev/null || true)"
fi
if [[ -z "${AGENTIC_BIN}" || ! -x "${AGENTIC_BIN}" ]]; then
    echo "error: agentic binary not found; run \`cargo build --release -p agentic-cli\`" >&2
    exit 1
fi

# Verify the CLI can actually reach the daemon (not just that the socket exists).
"${AGENTIC_BIN}" --repo "${here}" ping >/dev/null

echo "→ daemon still running at ${AGENTIC_SOCKET}"
echo "→ prompts/system.txt after git revert:"
head -2 "${here}/prompts/system.txt" | sed 's/^/   /'
echo "→ (memory rows in Postgres are untouched by git revert)"
echo "✓ 'redeploy' complete — agent reads the reverted prompt on next ask"
