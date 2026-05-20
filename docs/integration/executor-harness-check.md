# Executor harness compatibility check — Claude Agent SDK (2026-05-20)

**Status:** Research memo. Read-only — no code change.
**Purpose:** verify (or contradict) ADR-0004 Decision 3's assumption that the Claude Agent SDK exposes `on_checkpoint`-style hooks at tool-call boundaries, plus pause/restore support, before we commit to the sidecar atomic-integration path in v1.0.

**TL;DR:** the SDK ships everything we need, but **not in the shape ADR-0004 Decision 3 described**. ADR-0003's escape hatch (revert to layered manifest-export) is **not** needed; ADR-0004 Decisions 3 and 4 **do** need amendment to match the SDK's actual primitives.

## Sources

- [Claude Agent SDK — Persist sessions to external storage](https://code.claude.com/docs/en/agent-sdk/session-storage) — the canonical `SessionStore` protocol.
- [Claude Agent SDK — Python overview](https://code.claude.com/docs/en/agent-sdk/python) — `ClaudeAgentOptions`, `interrupt()`, hook events.
- [`anthropics/claude-agent-sdk-python` README](https://github.com/anthropics/claude-agent-sdk-python/blob/main/README.md) — `PreToolUse` hook example.

## What the SDK actually exposes

### 1. Hooks (`PreToolUse`, others) — intervention, not snapshot

Hooks fire at specific points in the agent loop. `PreToolUse` is documented; `PostToolUse` is referenced. They are **intervention** primitives — the contract is `(input_data, tool_use_id, context) → dict` with a `permissionDecision` for `PreToolUse`. They are **not** designed to publish state to an external store; that is `SessionStore`'s job.

We can still attach to hooks if we want to synchronously block the agent on an `agenticd` ack — that's exactly what `PreToolUse` returning `permissionDecision: "ask"` would let us do, gated on whether the prior `append` flushed.

### 2. `SessionStore` — append-only mirror, this IS the checkpoint primitive

Two required methods (full protocol quoted from the docs):

```python
class SessionStore(Protocol):
    async def append(self, key: SessionKey, entries: list[SessionStoreEntry]) -> None: ...
    async def load(self,   key: SessionKey) -> list[SessionStoreEntry] | None: ...
```

`append` is called by the SDK **after each batch of transcript entries is written locally**. Granularity is controlled by `session_store_flush: "batched" | "eager"`:

- `batched` (default): one flush per turn, or when the buffer fills.
- `eager`: background flush after every frame.

For atomic rollback we want `eager`. Even then, the smallest unit is "after a frame", **not "before/after a specific tool call"**. There is no mid-tool-call instrumentation hook.

### 3. Pause / resume — supported, but coarse

- **Resume across processes/hosts** via `ClaudeAgentOptions(session_store=store, resume="<session_id>")`. The SDK calls `store.load(key)` once before the subprocess spawns and replays the transcript. This is the production pause/resume mechanism.
- **`ClaudeSDKClient.interrupt()`** — streaming-mode only, stops the current run. Does NOT preserve mid-run state for later resume from the same instruction.
- **There is no mid-execution pause primitive.** The docs are explicit: "The SDK does not provide a pause/resume primitive… there is no mechanism to pause and later resume from the exact same execution point."

### 4. `append` is best-effort by design — directly contradicts ADR-0004 Decision 4

The docs are explicit: "Mirror writes are best-effort. If `append()` rejects or times out, the error is logged, a `{ type: 'system', subtype: 'mirror_error' }` message is emitted into the iterator, and the query continues."

That is the opposite of "loud-fail the ticket if the sidecar is unreachable" (ADR-0004 Decision 4). The SDK keeps going; the agent does its work even when `agenticd` is down. Atomic rollback's contract breaks the moment that happens, because the in-memory state has moved past the last durable checkpoint.

## What this means for ADR-0003 / ADR-0004

### Do NOT trigger the escape hatch

ADR-0003 Decision 2's documented escape hatch fires if "the Claude Agent SDK doesn't expose checkpoint primitives we can attach to." It does — they are `SessionStore.append` + `resume`. The shape is different from what ADR-0004 Decision 3 described, but the capability is there. The atomic-integration path stays.

### ADR-0004 Decision 3 needs amendment

Current text: "Every Claude Agent SDK checkpoint as the harness fires them — typically at tool-call boundaries or per message-cycle. The exact firing pattern is the SDK's responsibility; we snapshot whatever it gives us."

Closer to reality: "Every `SessionStore.append` call, with `session_store_flush='eager'`. The append batch is one or more transcript frames; we commit each batch as one `agentic` Commit. Tool-call boundary alignment is **best-effort, not guaranteed** — an `append` batch may straddle a tool call." Pseudocode for the wrapper:

```python
class AgenticSessionStore:
    async def append(self, key, entries):
        # key includes project_key + session_id; encode as the agentic branch.
        await self._client.commit(
            branch=f"executor/{key['session_id']}",
            entries=entries,                     # opaque JSON
            parent=self._last.get(key['session_id']),
        )
        self._last[key['session_id']] = ...      # remember commit hash

    async def load(self, key):
        return await self._client.read_entries(branch=f"executor/{key['session_id']}")
```

### ADR-0004 Decision 4 needs amendment OR a synchronising hook

ADR-0004 Decision 4 ("loud-fail if sidecar unreachable") is incompatible with `SessionStore`'s best-effort design. Two options:

1. **Wrap the iterator.** The worker observes the message stream for `{ type: 'system', subtype: 'mirror_error' }` system messages and tears down on the first one. Simple, but the agent has already produced output by the time the error surfaces — atomic rollback fidelity is degraded.
2. **Synchronising `PreToolUse` hook.** A hook that returns `permissionDecision: "deny"` if the prior `append` hasn't been ack'd by `agenticd`. This blocks the next tool call until the checkpoint is durable, restoring the "no progress without durable state" invariant. Requires the hook to share state with the SessionStore (single-process, in-memory).

Recommendation: pick (2). It maps cleanly to ADR-0002 Decision 3's 2PC staging order — no tool call advances unless its predecessor is in the store. Document this in the amended ADR-0004.

### What needs to actually change in code

None of this is changing the SDK. The changes are entirely on our side:

- `sdk/python/agentic/claude_agent.py` (new) — implements `SessionStore` and a paired `PreToolUse` hook wired through `AgenticClient`. ~80 lines.
- `crates/agentic-proto/src/lib.rs` — confirm the existing `Commit` op accepts an opaque `entries: Vec<serde_json::Value>` payload (or extend it). No new wire op should be needed; we are just persisting transcript frames as Commit payload.
- ADR-0004 amendment — Decisions 3 and 4 rewritten to match the SessionStore + synchronising-hook shape; ADR-0003 Decision 2 stays unchanged.

## Open questions for the Executor team

These are the things this memo cannot answer; surface them on the next sync.

1. **Compaction semantics for rollback.** `getSessionMessages` returns the post-compaction chain. After a compaction event, raw pre-compaction entries are still in the store (`store.load(key)` returns the full 503), but the agent's view is the summary (18 messages). Rolling back to a pre-compaction commit and replaying through compaction is non-trivial; do we want to commit *also* on compaction boundaries to make rollback granular below the summary?
2. **Subagent transcripts.** `listSubkeys` returns paths like `subagents/agent-<id>`. Rolling back the main session without rolling back its subagents leaves orphaned subagent state. Either (a) commit subagents as part of the parent commit, or (b) ban subagents in v1.0 and document the limitation.
3. **`forkSession` semantics.** Forking rewrites every `sessionId`, so the commit DAG must not assume session IDs are stable across forks. Worth a short rule in the integration doc.

## Verification

This memo is read-only. The follow-up is two things:

- An amendment to ADR-0004 covering Decisions 3 and 4 with the actual primitives. **Owner: Toni.**
- A spike: implement `AgenticSessionStore + synchronising hook` against the in-memory `claude_agent_sdk.testing.run_session_store_conformance` suite. ~half a day. **Owner: TBD.** If conformance passes, the integration shape is proven before Cloud Run packaging starts.
