# Issue #124 — demo green from a clean state — design

**Date:** 2026-07-10
**Issue:** [#124](https://github.com/git-agentic/git.agentic/issues/124)
**Branch:** `issue-124-demo-green`
**Status:** Approved in brainstorming (Toni, 2026-07-10)

## Problem

`run-demo.sh` does not run green from a clean state. Issue #124 enumerates five gaps.
Gaps 1–4 are portability/setup defects; gap 5 ("baseline ask hangs") was diagnosed in this
session and is neither a hang nor environmental:

**Diagnosed root cause of gap 5.** Every `AgenticCheckpointer` commit is rejected by the
ADR-0013 secret scanner. `_serialise_envelope` (`sdk/python/agentic/langgraph.py`)
base64-wraps msgpack checkpoints, and the entropy detector
(`crates/agentic-core/src/scanner.rs`: Shannon entropy > 4.5 over base64-alphabet runs
≥ 20 bytes) fires on base64 by construction. Daemon log (RUST_LOG=debug) shows every
checkpointer `commit` rejected with
`2PC staging on branch "langgraph/…": blob rejected by secret scanner: [Hit { kind: HighEntropy, … }]`.
The CLI is poisoned the same way: `read_prompt_dir` (`crates/agentic-cli/src/main.rs`)
recursively sweeps `prompts/__langgraph__/checkpoint.json` into `agentic commit`, so demo
steps 5 and 8 also fail once an ask has run. Timeline: `demo.cast` was recorded 2026-05-20;
the scanner landed 2026-05-21 — the demo has never run green since ADR-0013 shipped, and
CI (debug builds only, no demo run) could not see it.

The originally-reported 30 s silent hang did not reproduce from a clean state
(deterministic exit 1 in ~2 s, Python 3.14.6, langgraph 1.2.9). The SDK's missing socket
timeouts (`client.py` / `_framing.py` never call `settimeout`) are what turn any stall
into a silent hang; they are fixed here regardless.

The blob-hash allowlist (ADR-0013 Decision 4) cannot address gap 5: checkpoint content
changes every run, so there is no stable hash to allowlist. The scanner runs at `put_raw`
with no path context (paths live in the Tree), so a path-scoped policy must be decided
above `put_raw`, in commit staging.

## Design

### 1. ADR-0017 — entropy-exemption for declared checkpoint paths (gap 5)

New ADR amending ADR-0013.

- **Decision.** The entropy heuristic yields 100 % false positives on encoded checkpoint
  payloads, so it carries zero signal for them. Commit staging — which knows each blob's
  tree path — skips **only the entropy heuristic** for blobs whose path matches an exempt
  prefix. Pattern rules (AWS keys, PEM blocks, …) still run on every blob, exempt or not.
- **Default exempt prefix:** `__langgraph__/`. Configurable via a repeatable daemon flag
  `--scanner-exempt-entropy-prefix` (precedent: `--scanner-allowlist`).
- **Observability:** each applied exemption emits a structured tracing event under the
  daemon's existing tracing-only discipline (issue #118 pattern).
- **Residual risk (documented in the ADR):** a secret embedded inside serialized agent
  state is no longer entropy-caught. Accepted: the scanner is a guardrail against
  accidental commits by a trusted, peer-authenticated client (ADR-0012), not an
  adversarial control; an adversarial same-UID client could already bypass it by other
  means.
- **Contract neutrality:** no wire change; prompt-tree paths remain opaque strings, so the
  Commit object stays the platform API contract (ADR-0002) and framework-neutral
  (ADR-0003 Decision 3). The `__langgraph__/` default is daemon configuration, not schema.

### 2. SDK socket timeouts

`AgenticClient` sets socket timeouts: connect ~5 s, per-request read ~30 s, both
overridable (constructor args; env override acceptable). On expiry, raise a retryable
`AgenticProtocolError` naming the socket path and elapsed time. No wire change. This
converts any future daemon-side stall from a silent hang into an actionable error.

### 3. Gap 1 — release build broken by `strip = "symbols"`

`Cargo.toml` `[profile.release]`: `strip = "symbols"` → `strip = "debuginfo"`. Must be
verified by an actual `cargo build --release` on this machine (macOS ld 27031) during
implementation; if `debuginfo` also corrupts the `sqlx-macros` proc-macro dylib, drop
`strip` entirely. (bug-132)

### 4. Gap 2 — socket path exceeds `SUN_LEN`

`run-demo.sh` binds the daemon socket at a short path from `mktemp -d` under `/tmp`
(e.g. `/tmp/agentic-demo.XXXXXX/d.sock`), exported as `AGENTIC_SOCKET`, removed in the
existing cleanup trap. The demo then works from any checkout depth, including worktrees.

### 5. Gap 3 — `ask.sh` invokes `python`

`ask.sh` uses `${PYTHON:-python3}`; `run-demo.sh` exports `PYTHON` pointing at the demo
venv's interpreter.

### 6. Gap 4 — Python deps not set up

`run-demo.sh` gains an idempotent venv bootstrap at `examples/langgraph-rollback/.venv`:
`python3 -m venv` + `pip install -e "sdk/python[langgraph]" "psycopg[binary]"`, skipped
when the venv already exists and imports succeed. The seed step also removes stale
`prompts/__langgraph__/` left by prior failed runs, alongside the existing
`git checkout -- prompts/system.txt` restore.

### 7. CI additions (both approved)

- **macOS release-build job:** `cargo build --release -p agenticd -p agentic-cli` on
  `macos-latest`, build-only. Catches the strip/linker breakage class of gap 1.
- **Demo-smoke job:** Linux job running `run-demo.sh` end-to-end against dockerized
  Postgres. Would have caught the scanner regression the day it landed — the demo is the
  discipline.

## Error handling

- Scanner exemption is fail-closed: a path that does not match an exempt prefix gets the
  full scan, unchanged. Malformed `--scanner-exempt-entropy-prefix` values are rejected at
  daemon startup (startup-validation precedent from #118/ADR-0012).
- SDK timeout errors are retryable `AgenticProtocolError`s; existing callers that catch
  `AgenticError` keep working.
- `run-demo.sh` venv bootstrap failures abort with an actionable message (which command
  failed, how to retry).

## Testing

- **Rust:** staging-level tests — high-entropy blob under an exempt path commits; the same
  blob under a non-exempt path is rejected; a pattern hit (e.g. AWS key) under an exempt
  path is still rejected; flag parsing/startup validation.
- **Python:** timeout behavior against a stub socket server (accepts, never replies →
  retryable `AgenticProtocolError` within the deadline); existing SDK tests stay green.
- **End-to-end (the acceptance criterion):** `run-demo.sh` green from a clean state
  (compose down, fresh `.agentic`, fresh venv), run from both the repo root and a
  worktree. Confirms baseline ask, both commits, diff, rollback, recovered ask.

## Out of scope

- The `AgenticSessionStore` (ADR-0005) and other SDK demo-path risks in cerebrum
  (ghost-state on transient commit failure, `TRUNCATE` identity rewind, serializer
  divergence) — pre-existing, tracked separately.
- Explaining the original 30 s hang observation: not reproducible from a clean state;
  the SDK timeouts make any recurrence loud and diagnosable.
- Fresh-machine < 5 min timing (sprint item A1) — this work is a prerequisite, not the
  measurement.
