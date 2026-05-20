# Agent SDK schema verification — Week 6 checkpoint

Status: PASS
Date: 2026-05-20
Owner: toni
ADR refs: [ADR-0002](../adr/0002-substrate-and-supercommit.md), [ADR-0003](../adr/0003-<partner>-executor-integration.md), [ADR-0004](../adr/0004-realtime-agenticd-for-executor.md)

## Why this exists

[`docs/product/roadmap.md`](../product/roadmap.md) Week 6 schedules a verification checkpoint: with the Commit object capturing all six tuple dimensions for LangGraph, can it also express a Claude Agent SDK session **without framework-specific fields**, and do the SDK's checkpoint primitives match what [ADR-0004 Decision 3](../adr/0004-realtime-agenticd-for-executor.md) assumes?

This doc is the receipt. If either check failed, the trigger was either ADR-0005 (amend the Commit schema) or the [ADR-0003 Decision 2 escape hatch](../adr/0003-<partner>-executor-integration.md) (revert to layered manifest-export for v1.0 and defer atomic to v1.1).

Neither was triggered. One WARNING is recorded for future work.

## What we verified against

Authoritative source: [`anthropics/claude-agent-sdk-python`](https://github.com/anthropics/claude-agent-sdk-python) (fetched via Context7, 2026-05-20). The SDK presents:

- **Message types** — `AssistantMessage`, `UserMessage`, `SystemMessage`, `ResultMessage`; content blocks `TextBlock`, `ToolUseBlock`, `ToolResultBlock`.
- **Session transcript** — append-only JSONL, one entry per turn, entries linked through a `parentUuid` chain and identified by a `session_id` UUID. Public reader: `get_session_messages(session_id, ...)`.
- **SessionStore** — pluggable persistence interface. Reference Postgres schema is `(project_key, session_id, subpath, seq, entry jsonb, mtime)`, primary key on `(project_key, session_id, subpath, seq)`. The `subpath` field is the SDK's affordance for sub-agent transcripts.
- **Resume** — `ClaudeAgentOptions(session_store=..., resume=session_id)` resumes from any pause point in the store.
- **Hooks** — `PreToolUse`, `PostToolUse`, `Stop`, `SubagentStop`, `UserPromptSubmit`, `SessionStart`, `PreCompact`. Each fires at a deterministic boundary with structured input data and a `tool_use_id`.
- **MCP** — in-process SDK servers (`create_sdk_mcp_server`) and external stdio servers configured under `ClaudeAgentOptions.mcp_servers`.

## The Commit schema we are verifying

From `crates/agentic-core/src/object.rs`:

| Field | Origin |
|---|---|
| `parent`, `author`, `timestamp`, `message` | bookkeeping |
| `code_sha`, `prompts`, `tools`, `model`, `memory_snapshot`, `schema_version` | original six tuple dimensions (ADR-0001) |
| `intent`, `plan`, `transcript`, `evals` | ADR-0002 extensions — content-addressed Hash each |
| `cost_cents` | ADR-0002 |
| `signatures` | ADR-0002 attestations |

The discriminating question: walk an Agent-SDK artifact through these and try to land it on a dimension without inventing `agent_sdk_*` or `tool_call_*` or `mcp_*` fields.

## Probe walk-through

### Probe 1 — Multi-turn session with interleaved tool calls

A Coding-worker run is a JSONL transcript of (`UserMessage`, `AssistantMessage` containing one or more `ToolUseBlock`, `UserMessage` carrying the corresponding `ToolResultBlock`, ...) tied together by `parentUuid` and rooted at `session_id`.

| Artifact | Lands on | Notes |
|---|---|---|
| Full JSONL transcript | `transcript: Option<Hash>` | Hash of a `Blob` (or `Tree` if we split per-turn). Wire-format is content of the SessionStore `entry jsonb` column joined in `seq` order — no SDK-specific re-encoding required. |
| Tool catalog at commit time | `tools: Option<Hash>` | `Tree` of `<server>: <BLAKE3(canonical tools/list)>`; ADR-0004 sidecar already produces this. |
| System prompt | `prompts: Option<Hash>` | `Tree` of prompt files including the system message. |
| Model identifier | `model: Option<Hash>` | `Blob` of `{model, version, parameters}`. |
| User goal that opened the turn | `intent: Option<Hash>` | The opening `UserMessage` content (free text or structured). Already neutral. |

PASS — no field invented.

### Probe 2 — Sub-agent invocation

The SDK exposes sub-agents via the SessionStore `subpath` field: a sub-agent run is a separate JSONL stream namespaced under the parent `session_id`. The reference Postgres schema literally has `subpath text NOT NULL DEFAULT ''` for exactly this reason.

In Commit terms, a parent run's `transcript` is not a single Blob — it's a `Tree`:

```
transcript: Tree {
  "main.jsonl":           Blob(parent session JSONL),
  "subagents/<sub-id>/0": Blob(sub-agent JSONL),
  "subagents/<sub-id>/1": Blob(another sub),
}
```

This composes cleanly with the existing `Tree` type. Sub-agent commits **do not need** a Commit-level `children: Vec<Hash>` field because the agent run is one logical unit of work — one commit. Internal structure lives inside the transcript Tree.

Open design question (not blocking v1.0): if we ever want sub-agents to produce *independent commits* with their own rollback semantics (e.g. for very long-running sub-tasks), that is a follow-up — Merkle parent-of-many. But that's a feature beyond what ADR-0003 asks for here, and the SDK does not produce independent sub-agent sessions today.

PASS — no field invented.

### Probe 3 — Paused-then-resumed session

Pause: the agent stops mid-run after some `PostToolUse` hook returned, but before the next `query()` call completes. SessionStore has every entry written so far at `(project_key, session_id, subpath, seq < N)`.

Resume: `ClaudeAgentOptions(session_store=store, resume=session_id)` — the SDK reads from `seq` 0 to the latest entry and continues from there.

In Commit terms a pause-point commit is just a normal commit with a `transcript` hash that ends mid-run. Restore reverses it: write the transcript Blob/Tree back into the SessionStore, then the next `query(resume=session_id)` call picks up where it left off.

| Concern | Outcome |
|---|---|
| Does the schema express "this is a resumable pause point"? | Yes — every commit is. No special flag needed. |
| Does the schema record the originating session id? | Implicitly via the transcript content. If we ever want O(1) lookup, the transcript Tree can carry a `session_id` key; still no Commit-level field needed. |
| Does the runtime fire at the right boundary? | Yes — `PostToolUse` is exactly the granularity ADR-0004 Decision 3 assumes; see §"Checkpoint primitives" below. |

PASS — no field invented.

### Probe 4 — MCP `tools/list` changes mid-session

Setup: the agent uses MCP server `X`. Between tool calls 3 and 4, server `X` ships a new version that changes the signature of one tool. The session continues. At commit time, the daemon's `fingerprint_all` walks the configured MCP servers and computes the `tools` Hash from `tools/list` **as-of-commit-time** — which is the new version.

The transcript records every `ToolUseBlock` (name, input) and `ToolResultBlock` (output) verbatim. So replay can *prove what was called*, but the `tools` dimension does **not** record what each tool's schema was *at the moment of the call*. A rollback to this commit would restore the new schema, not the schema that was actually in effect for calls 1–3.

Is this a schema gap or an implementation gap?

- The schema is expressive enough: a future enhancement could replace the daemon's single-shot `fingerprint_all` with per-`PostToolUse`-fired captures whose hashes are recorded inside the transcript entry. That keeps `tools` framework-neutral and uses the existing extension points.
- It is an **implementation gap** in `crates/agenticd/src/mcp.rs` (fingerprints once per commit) and an **opportunity gap** in the daemon's hook integration (does not yet wire `PostToolUse` to capture per-call provenance).

Recommendation: WARNING. Logged below. Does not invalidate the schema verification. Practically mitigated for v1.0 by assuming MCP servers are stable within a single agent run — the Coding worker case ADR-0003 targets is exactly this shape.

PASS (with WARNING noted) — no field invented.

## Per-dimension PASS / GAP table

| Commit dimension | Used by which probe(s)? | Status | Notes |
|---|---|---|---|
| `code_sha` | All | PASS | Agent code identity. |
| `prompts` | 1, 3 | PASS | System / user prompt tree. |
| `tools` | 1, 4 | PASS¹ | ¹ See WARNING below: single fingerprint per commit, not per call. |
| `model` | All | PASS | Model identifier blob. |
| `memory_snapshot` | All | PASS | Already proven by Chunk B. |
| `schema_version` | All | PASS | Postgres migration version. |
| `intent` | 1, 3 | PASS | Opening user goal — neutral. |
| `plan` | (unused this round) | PASS | Reserved for plan-mode style artifacts. |
| `transcript` | 1, 2, 3 | PASS | JSONL → Blob/Tree. Sub-agents via Tree subpath. |
| `evals` | (unused this round) | PASS | Reserved for offline evals. |
| `cost_cents`, `signatures` | All | PASS | Operational metadata, not framework-coupled. |

## Checkpoint primitives runtime contract

ADR-0004 Decision 3 commits the sidecar to firing snapshot triggers at tool-call boundaries plus session start / end, and to supporting pause + restore at any captured boundary. Walk through the SDK's actual primitives:

| ADR-0004 assumption | SDK primitive | Match |
|---|---|---|
| Fire **before** each tool call | `PreToolUse` hook with `tool_name`, `tool_input`, `tool_use_id`, `context` | ✓ |
| Fire **after** each tool result | `PostToolUse` hook with the corresponding `tool_use_id` | ✓ |
| Fire at session end | `Stop` hook (main) and `SubagentStop` hook (sub-agents) | ✓ |
| Fire at session start | `SessionStart` hook | ✓ |
| Pause at any boundary | Boundaries are arbitrary — every `PostToolUse` is a candidate, the SessionStore receives entries as they are written. | ✓ |
| Restore from a boundary | `ClaudeAgentOptions(session_store=store, resume=session_id)` | ✓ |
| Identify the pause point unambiguously | `session_id` UUID + the SessionStore's `(seq, subpath)` ordering | ✓ |
| Resume work as a sub-agent | `SubagentStop` mirrors `Stop`; sub-agents reuse the same hook surface | ✓ |

No assumption fails. The sidecar's `on_checkpoint`-style firing is `PostToolUse` (and `Stop`/`SubagentStop` for the terminal cases), wired to call into agenticd's commit RPC via the same Unix socket the rest of the SDK contract uses.

## Open WARNING (does not block PASS)

- **Per-call MCP `tools/list` fingerprinting.** The daemon's `tools` dimension records the catalog state at commit time, not per tool call. If a server mutates its tool schemas within a session, replay will not preserve the call-time schema. Mitigation in v1.0: document that MCP servers used by the Coding worker are expected to be stable within a session — fits the ADR-0003 target use case. Real fix: in a follow-up, have agenticd subscribe to `PostToolUse` hooks and record per-call fingerprints inline in the transcript blob. This is a daemon enhancement and does not change the Commit schema.

## Decision

**PASS.** Proceed with ADR-0003 and ADR-0004 as written. Do not open ADR-0005. Do not trigger the manifest-export escape hatch.

Concrete consequences:

1. The SDK contract stays framework-neutral. No `agent_sdk_*` fields enter `Commit`.
2. The sidecar's snapshot triggers are: `PostToolUse` for incremental, `Stop` / `SubagentStop` for terminal, `SessionStart` for the initial commit. Wire this into `crates/agenticd/src/mcp.rs`'s hook integration when the sidecar work begins.
3. Carry forward the per-call MCP fingerprinting WARNING as a known v1.1 polish item. Document it in the v1.0 release notes so design partners are not surprised.
4. Coordinate with the platform-partner integration team that their Coding worker registers the hooks above against the SDK's hook system. This is ADR-0003 §"Coordination" territory.

## What would have failed this check

For the record, so we can recognise the failure shape if it happens later:

- A required artifact that does not land on any existing dimension and cannot be subsumed by `transcript`. Example would have been: a *streaming partial response* mid-tool-call that needs its own dimension. The SDK does not produce one — `AssistantMessage` blocks are atomic.
- A hook event the SDK does not fire that the sidecar critically depends on. Example would have been: no `PostToolUse` (we'd have only `Stop`-granularity snapshots, breaking ADR-0004's incremental promise). The SDK does fire it.
- A session-resume primitive that requires re-running tool calls from scratch (no pause-at-boundary). The SDK does not — `resume=session_id` picks up at the exact `seq` written.

None of these failed. The decision stands.
