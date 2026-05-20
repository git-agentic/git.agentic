# ADR-0005: SessionStore-based amendment to ADR-0004 Decisions 3 and 4

**Status:** Proposed
**Date:** 2026-05-20
**Deciders:** Toni
**Amends:** [ADR-0004](0004-realtime-agenticd-for-executor.md) Decisions 3 and 4
**Relates to:** [`docs/integration/executor-harness-check.md`](../integration/executor-harness-check.md) (research memo this ADR codifies)

## Context

[ADR-0004](0004-realtime-agenticd-for-executor.md) was Accepted on 2026-05-20 and commits the Executor integration to a sidecar `agenticd` with **atomic** rollback semantics. Two of its Decisions assumed specific primitives in the Claude Agent SDK:

- **Decision 3** specified snapshot triggers "as the harness fires them — typically at tool-call boundaries or per message-cycle. The exact firing pattern is the SDK's responsibility; we snapshot whatever it gives us." The implicit model was an `on_checkpoint`-style hook.
- **Decision 4** specified loud-fail of the Coding worker ticket if the sidecar is unreachable: "If the sidecar process dies mid-session, the worker receives an IPC error on the next checkpoint write … Marks the ticket as failed … Exits non-zero."

A read-only audit of the Claude Agent SDK's actual public surface (`docs/integration/executor-harness-check.md`, citing the SDK's `session-storage` and `python` reference pages) found:

1. The SDK does have hooks (`PreToolUse`, `PostToolUse`, …), but they are **intervention** primitives that return a `permissionDecision` for the imminent tool call. They are not designed to publish state to external storage; they cannot be the snapshot primitive on their own.
2. The SDK's actual external-storage primitive is the **`SessionStore` protocol** — two required async methods, `append(key, entries)` and `load(key)`, with an `InMemorySessionStore` reference implementation and a conformance test suite shipped at `claude_agent_sdk.testing.run_session_store_conformance`. Verified to exist in `anthropics/claude-agent-sdk-python` at `src/claude_agent_sdk/testing/session_store_conformance.py`.
3. `append` is invoked **per turn (`session_store_flush="batched"`, default) or per frame (`session_store_flush="eager"`)**. The smallest available granularity is one transcript frame; mid-tool-call instrumentation is not exposed.
4. `append` is **best-effort by design** — the SDK docs are explicit: "If `append()` rejects or times out, the error is logged, a `{ type: 'system', subtype: 'mirror_error' }` message is emitted into the iterator, and the query continues. … Batches that fail are not retried." This directly contradicts ADR-0004 Decision 4 as written.
5. Pause/resume across processes/hosts is supported via `ClaudeAgentOptions(session_store=store, resume="<session_id>")`; the SDK calls `store.load` once before the subprocess spawns. There is no mid-execution pause primitive, but for our purposes pause-and-resume-on-another-instance is what matters and that works.

ADR-0004's *capability claim* (atomic rollback is implementable) survives the audit. Its *mechanism description* in Decisions 3 and 4 does not. This ADR codifies the corrected mechanism. ADR-0004's Decisions 1, 2, and 5 are untouched.

This amendment does NOT trigger the ADR-0003 Decision 2 escape hatch. That escape hatch fires if "the SDK doesn't expose checkpoint primitives we can attach to." `SessionStore.append` plus `resume` are such primitives — they have a different shape than ADR-0004 first described, but they are present, documented, and shipped.

## Decision

### Decision 1 — Snapshot primitive is `SessionStore.append`, not a hypothetical `on_checkpoint` hook (replaces ADR-0004 Decision 3)

The sidecar implements the Claude Agent SDK's `SessionStore` protocol. The Coding worker passes the resulting adapter via `ClaudeAgentOptions(session_store=store, session_store_flush="eager")` when constructing each query.

The sidecar's `append` implementation translates each transcript-frame batch into one `agentic` Commit on a per-session branch `executor/<session_id>`, parented to the previous commit on that branch.

```python
class AgenticSessionStore:
    """SessionStore adapter that mirrors transcript frames into agenticd."""
    def __init__(self, client: AgenticClient):
        self._client = client
        self._last: dict[str, Hash] = {}   # session_id → last commit hash

    async def append(self, key: SessionKey, entries: list[SessionStoreEntry]) -> None:
        sid = key["session_id"]
        commit = await self._client.commit(
            branch=f"executor/{sid}",
            entries=entries,                # opaque JSON; preserved verbatim
            parent=self._last.get(sid),
        )
        self._last[sid] = commit.hash

    async def load(self, key: SessionKey) -> list[SessionStoreEntry] | None:
        sid = key["session_id"]
        return await self._client.read_entries(branch=f"executor/{sid}")
```

Snapshot triggers therefore become:

- **Session start.** Captured implicitly: the worker calls `client.session_start(...)` before constructing the SessionStore-backed query; the daemon writes an initial Commit with the tuple state per ADR-0004's original Decision 3 framing (model identifier, system prompt hash, MCP manifest hashes, working-copy SHA, empty memory).
- **Every `SessionStore.append`** the SDK fires. With `session_store_flush="eager"` this is "after every frame" — typically one per agent message-cycle, sometimes more.
- **Session end.** Captured explicitly: the worker calls `client.session_end(...)` after the SDK's query iterator drains, with the final tuple state (PR SHA on success, structured failure record on failure).

Tool-call boundary alignment is **best-effort, not guaranteed**: an `append` batch may straddle a tool call. This is a tighter promise than ADR-0004 Decision 3 implied and the team should be honest with design partners about it.

Higher granularity than this (true mid-tool-call snapshots) is rejected for the same reasons ADR-0004 Decision 3 originally rejected it: per-write cost (ADR-0004 Decision 5) doesn't justify the marginal rollback fidelity. Lower granularity (session-end only) remains the manifest-export shape ADR-0003 rejected.

### Decision 2 — Loud-fail is preserved via a synchronising `PreToolUse` hook (replaces ADR-0004 Decision 4)

ADR-0004 Decision 4 ("loud-fail the ticket if the sidecar is unreachable") is incompatible with `SessionStore.append` being best-effort. The SDK keeps the agent loop running through `append` failures and only surfaces them as `{ type: 'system', subtype: 'mirror_error' }` messages in the iterator. If the worker only watches `append` errors, the agent has already advanced past the last durable checkpoint by the time the error reaches user code — atomic rollback fidelity is degraded.

To preserve ADR-0004 Decision 4's invariant ("no progress without durable state"), the Coding worker installs a **synchronising `PreToolUse` hook**:

```python
async def gate_on_durability(input_data, tool_use_id, context):
    """Block the next tool call until the prior append has been ack'd by agenticd."""
    if not store.last_append_acked():            # store is the AgenticSessionStore above
        return {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": (
                    "agenticd has not ack'd the prior checkpoint; refusing to "
                    "advance to preserve atomic rollback contract"
                ),
            },
        }
    return {}
```

The hook gates each tool call on the durability of the preceding `append`. The `AgenticSessionStore` and the hook share per-session state in the same process (single-process worker; no cross-process synchronisation needed for `containerConcurrency: 1`). When the sidecar is unreachable, `last_append_acked()` stays False, the next `PreToolUse` returns `deny`, the agent surfaces a tool-permission error, and the worker treats that as a hard failure of the ticket — same observable contract as ADR-0004 Decision 4 in writing.

This is loud-fail by design. It is also slightly weaker than ADR-0004 Decision 4 originally promised — it can let one *agent message* without a tool call slip through before the next gate fires. For the Executor's coding-task workload (tool-call-heavy), this is acceptable. For pure-conversation workloads (none in v1.0), it isn't, and a future ADR would need to add a `PostToolUse`/message-boundary gate.

If the SDK adds a `PostMessage`-style synchronous hook in a future release, prefer that over the `PreToolUse` gate: it closes the one-message gap above without changing the contract elsewhere.

## What does not change

- **ADR-0004 Decision 1** (sidecar deployment in the same Cloud Run instance) — unchanged.
- **ADR-0004 Decision 2** (no network auth in v1.0) — unchanged.
- **ADR-0004 Decision 5** (GCS-backed `ObjectStore`) — unchanged.
- **ADR-0003 Decisions 1–3** — unchanged. Escape hatch in ADR-0003 Decision 2 is NOT triggered.
- **`agentic-proto` wire types** — unchanged. The existing `Commit` op already accepts an opaque JSON payload sufficient for `entries`.

## Consequences

**Positive:**

- The Executor integration uses a documented, supported, conformance-tested SDK surface (`SessionStore`) instead of a hook contract the SDK does not advertise. Adapter authors can validate their work with `claude_agent_sdk.testing.run_session_store_conformance`.
- Pause-and-resume on a different Cloud Run instance is supported out of the box via the SDK's existing `resume="<session_id>"` mechanism.
- The synchronising `PreToolUse` hook composes cleanly with ADR-0002 Decision 3's 2PC staging: a checkpoint not durable in the object store means the next tool call is blocked, exactly as the 2PC ordering already requires.

**Negative:**

- Tool-call boundary alignment is best-effort. A pause-and-rollback that's intended to land "at tool-call boundary X" may actually replay one extra frame. Document this for design partners.
- The `PreToolUse` gate adds a per-tool-call sync point; if `agenticd` `append` latency rises, every tool call waits for the prior `append` to complete. Bounded by ADR-0004 Decision 5's "~50–200 ms per GCS write" budget; not blocking, but visible in worker wall-clock time.
- Conversation-only sessions (no tool calls) have no checkpoint-gating mechanism in v1.0. Document the limitation; not an issue for the Executor's coding workload.

**Risks to revisit:**

- If the Claude Agent SDK changes the `SessionStore` protocol (e.g., adds required methods, changes flush semantics), this ADR's `AgenticSessionStore` pseudocode and the conformance assumption need re-verification. Pin the SDK version in `sdk/python/pyproject.toml` to a known-good range.
- `forkSession` in the SDK rewrites every `sessionId` and remaps message UUIDs (per the SDK docs). Our commit DAG keys branches off the session_id — forking creates a new branch with no shared history. Document this for users who might expect forked sessions to share a rollback target with their parent.
- Compaction: the SDK runs auto-compaction at length thresholds; `getSessionMessages` returns the post-compaction chain while `load` returns the raw history. Rolling back across a compaction boundary needs explicit handling — out of scope for this ADR; track separately.

## Implementation plan

1. **Spike (≈ half a day).** Implement `AgenticSessionStore` + the synchronising hook in `sdk/python/agentic/claude_agent.py` (new file). Validate against `claude_agent_sdk.testing.run_session_store_conformance`. If conformance fails, this ADR is reopened.
2. **Wire the worker.** The Coding worker constructs `AgenticSessionStore(client)` and passes it to `ClaudeAgentOptions(session_store=..., session_store_flush="eager", hooks={"PreToolUse": [HookMatcher(matcher="*", hooks=[gate_on_durability])]})`.
3. **End-to-end smoke against the sidecar.** With the GCS-backed `ObjectStore` (already merged via PR #21's Dockerfile + PR #20's CI), prove one cycle: session start → several `append`s → rollback → resume on a "different" instance (same image, fresh container).
4. **Amend `docs/integration/executor-sidecar.md`** with the worker-side code snippet from this ADR's Decision 2.

Owner for the spike: TBD. Owner for the integration-doc amendment: whoever lands the spike.

## Pseudocode caveats

The `AgenticSessionStore` and `gate_on_durability` snippets in this ADR are illustrative. They assume an `AgenticClient.commit(branch=, entries=, parent=)` method shape; the actual SDK surface in `sdk/python/agentic/client.py` may need extension to accept `entries` as an opaque payload. Final API shape is the spike's responsibility, not this ADR's.
