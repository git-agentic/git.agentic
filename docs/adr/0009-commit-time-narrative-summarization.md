# ADR-0009: Commit-Time Narrative Summarization

**Status:** Proposed
**Date:** 2026-05-21
**Deciders:** Toni
**Extends:** [ADR-0002](./0002-substrate-and-supercommit.md) Decision 2 (extended Commit object: `intent`, `plan`, `transcript`, `evals`, `cost_cents`, `signatures`)
**Relates to:** [ADR-0008](./0008-secondary-objectstore-for-agent-state.md) (where the summary blob lives), [ADR-0005](./0005-sessionstore-amendment-to-adr-0004.md) (where transcript frames originate)

## Context

[ADR-0002 Decision 2](./0002-substrate-and-supercommit.md) extends the Commit object with five new content-addressed blob references — `intent`, `plan`, `transcript`, `evals`, plus `cost_cents` and `signatures`. The `transcript` field is being populated by `AgenticSessionStore` (per [ADR-0005](./0005-sessionstore-amendment-to-adr-0004.md)) and by the LangGraph checkpointer. The `intent` and `plan` fields exist in the schema but **are not being written**. As of 2026-05-21, every Commit ships with `intent = Hash::ZERO` and `plan = Hash::ZERO`, which means the agent-PR review primitive ([memory: `project_agent_pr_primitive.md`](../../.. /agent-pr-primitive)) cannot render the question it is centrally designed to answer: *what was this Commit supposed to do, and did it do it?*

[Entire CLI](../product/competitive-brief-entire.md) ships a feature that fills exactly this gap. At commit time, when `strategy_options.summarize.enabled = true`, they invoke the `claude` CLI to generate a structured five-section summary — **intent / outcome / learnings / friction / open items** — from the in-flight session transcript, and attach it to the checkpoint. It is opt-in, non-blocking (failures log but don't prevent commits), and runs against the user's local agent CLI.

Three things are worth separating:

1. **The shape of the summary.** Five sections is a good shape. It maps cleanly to fields we already have: *intent* ↔ `Commit.intent`, *outcome + learnings* ↔ part of `Commit.evals` (or a sixth blob if we want to keep `evals` machine-readable only), *friction + open items* ↔ a new "narrative" addendum.
2. **When and how to invoke a summarizer.** Synchronous-blocking-on-commit is wrong; the commit path's latency budget per `snapshot-model.md` §9 is `< 2s` and an LLM call doesn't fit. Async-fire-and-forget is what Entire chose and is the right default.
3. **Whether the summary is a first-class CLI surface.** Entire makes it one (`entire checkpoint explain`). We should not — per [ADR-0002 Decision 2](./0002-substrate-and-supercommit.md), the summary is *content of the Commit object*, retrievable via existing `agentic show` machinery, not a new top-level verb. This is the discipline that keeps the surface narrow.

This ADR specifies how summarization fits into the existing Commit object, how the summarizer is invoked, and how privacy / cost concerns are managed without bolting on a new product surface.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **The summary is a single content-addressed blob referenced from a new `Commit.narrative` field. Five sections: intent, outcome, learnings, friction, open items.** | Slots into the extended-Commit schema (ADR-0002 Decision 2); one field is the minimum surface for the five-section payload. |
| 2 | **`Commit.intent` is also populated when summarization is enabled** — it gets a synthesized intent statement derived from the first user message and tool-call prefix of the session. | Closes the `intent = ZERO` gap; lets the agent-PR primitive render the question it was designed for. |
| 3 | **Invocation is asynchronous post-commit, never blocking.** `agenticd` enqueues a summarization job after the commit ack is sent; the resulting blob is written and a follow-up `Commit.amend_narrative(hash)` updates the Commit's `narrative` field via a synthesized successor commit. | Keeps the commit-path latency budget intact; preserves "commits are immutable" by amending via successor rather than mutation. |
| 4 | **Opt-in per-repo via `[summarize] enabled = true` in `agenticd.toml`. Default is off.** | Honest about the LLM call cost and the transcript-egress privacy implication. Matches Entire's default-off. |
| 5 | **Provider is configurable; default is the Anthropic API via `anthropic-sdk`. `provider = "claude_cli"` invokes the local Claude CLI per Entire's pattern. `provider = "none"` disables.** | API as default makes the daemon self-contained; CLI fallback honors users who already have Claude CLI authenticated and want to reuse it. |
| 6 | **The summary blob and the synthesized successor commit route to the secondary `ObjectStore`** per [ADR-0008](./0008-secondary-objectstore-for-agent-state.md), when one is configured. | Summaries contain transcript-derived content; access-control posture follows transcripts. |
| 7 | **No new top-level CLI verb.** `agentic show <commit>` already renders blob fields; it gains a `--narrative` flag that pretty-prints the five-section summary. | Resist `agentic explain`. The summary is data, not a workflow. |

---

## Decision 1 — `Commit.narrative` blob, five sections

The summary is a single JSON blob with this shape:

```json
{
  "schema_version": "1.0",
  "intent": "Add CSAT escalation routing to the support agent.",
  "outcome": "Routed escalation triggers added; one regression in the …",
  "learnings": [
    "The `escalation_threshold` config defaults differ between staging and prod …",
    "…"
  ],
  "friction": [
    "Spent ~20 min on a confusing pgvector index error that turned out to be …",
    "…"
  ],
  "open_items": [
    "Add an integration test for the escalation flow under load.",
    "…"
  ],
  "generated_at": "2026-05-21T14:32:11Z",
  "generator": { "provider": "anthropic_api", "model": "claude-sonnet-4-6", "input_tokens": 8431, "output_tokens": 412 }
}
```

The blob is stored under the same content-addressed scheme as every other blob (BLAKE3, zstd-compressed) and referenced from a new `narrative: Hash` field on the `Commit` struct in `crates/agentic-core/src/object.rs`. `narrative = Hash::ZERO` when no summary exists (analogous to existing zero-valued fields).

Five sections is what we ship. Not four, not six. The shape is borrowed from Entire and from PR-template conventions because it matches how engineers actually think about a unit of work. Adding sections requires a `schema_version` bump and a follow-up ADR; this is the explicit discipline.

## Decision 2 — `Commit.intent` is populated alongside

Today `Commit.intent` is always `Hash::ZERO`. With summarization enabled, the summarizer also writes a separate `intent` blob — just the first section, not the whole narrative — to the existing `intent` field. This is the cheapest way to start populating the agent-PR primitive's central question without re-architecting.

When summarization is disabled, `intent` stays `Hash::ZERO` and the agent-PR primitive renders a placeholder. We do not synthesize an intent without an LLM call; heuristic "intent" extraction from session metadata is worse than nothing.

(`Commit.plan` is similarly empty today and is *not* populated by this ADR. Plan extraction is a harder problem — the plan is usually implicit in the agent's tool-call trace — and the eval primitives in `Commit.evals` are more load-bearing for the demo. Leave `plan` for ADR-0010+ when there's a concrete consumer.)

## Decision 3 — Async post-commit, amend via successor

The commit-path latency budget is `< 2s` per [`snapshot-model.md` §9](../architecture/snapshot-model.md). An Anthropic API call typically takes 3–8s for a session summary of typical transcript size; Claude CLI invocation has cold-start overhead of 1–2s on top. Neither fits in the budget.

The flow:

```
agenticd receives commit envelope
  → 2PC staging (per ADR-0002 Decision 3)
  → ref update
  → ack sent to SDK (latency budget met)
  → [async] enqueue summarization job for commit <hash>
  → [async, later] summarizer reads transcript blob, calls provider, writes narrative blob
  → [async, later] synthesize a successor commit on the same ref with parent=<hash>,
                   narrative=<new-hash>, intent=<new-hash>, all other fields copied
  → [async, later] update branch ref to the successor commit
```

The successor-commit mechanism is the discipline. Commits are immutable; "amending" a commit means writing a new commit that points at the same content but with the narrative field populated, then advancing the ref. This is the same shape as Git's `commit --amend` semantics. The successor commit carries a synthesized `signatures` entry of kind `NarrativeAmendment { source: <hash>, generator: {...} }` per [ADR-0002 Decision 2](./0002-substrate-and-supercommit.md), so the audit trail is intact.

Two consequences worth naming:

- **The branch ref briefly points at a narrative-less commit, then advances.** Anyone reading the ref between the ack and the amendment sees the unamended state. This is correct — the narrative is informational, not load-bearing for rollback or for the tuple guarantee.
- **If the summarizer fails (network error, provider down, transcript blob unreadable), the ref stays at the original commit.** The failure is logged with a stable error code; `agentic doctor narrative` lists commits with `narrative = ZERO` that were eligible for amendment but never got it, so a user can retry. Add to action items.

## Decision 4 — Opt-in, default off

`agenticd.toml` gains:

```toml
[summarize]
enabled       = false           # default
provider      = "anthropic_api" # or "claude_cli" or "none"
model         = "claude-sonnet-4-6"
max_input_tokens  = 32000
max_output_tokens = 512
retry_on_failure = true
```

Default off is the honest choice. Enabling summarization:

- Sends the transcript blob to an external LLM provider. The transcript may contain customer data, secret material the redactor missed (best-effort per `CLAUDE.md`), or proprietary prompts. Users must opt in to that egress consciously.
- Costs money per commit. Typical session transcripts are 5–15k tokens of input; at Sonnet pricing that's small but non-zero, and at scale (a busy repo making 100s of commits per day) it adds up.
- Requires a credential. `ANTHROPIC_API_KEY` for `anthropic_api`, an authenticated `claude` CLI for `claude_cli`. The daemon refuses to start with `summarize.enabled = true` and no credential.

Documentation at `docs/integration/summarization.md` (action item below) lists exactly what's sent, to whom, and how to verify the redactor before enabling.

## Decision 5 — Provider matrix

Three options:

| Provider | Use case | Auth | Notes |
|---|---|---|---|
| `anthropic_api` (default) | Daemon-self-contained; production deployments where the daemon is a service, not a per-user process. | `ANTHROPIC_API_KEY` env var. | First-class. Uses `anthropic-sdk` Rust crate. |
| `claude_cli` | Developer machines where `claude` CLI is already authenticated and the user wants to reuse that auth. Matches Entire's behavior. | Inherits from local `claude` CLI auth. | Subprocess invocation; CLI must be in `PATH`. Cold-start cost noted in Decision 3. |
| `none` | Explicit disable for tests, for compliance environments that forbid external LLM calls, or for users who want the field present-but-empty pending a manual amendment. | n/a | `enabled = true` + `provider = "none"` is a valid combination; it means "amend with externally-supplied narrative via `agentic commit amend-narrative <hash> <blob>`." |

Other providers (OpenAI, local llama.cpp, etc.) are explicitly out of scope for v1.1. Adding one is a 50-line PR behind a `Provider` trait; we don't preempt without a design-partner ask.

## Decision 6 — Narrative blob routes to secondary store

The narrative blob is transcript-derived; it inherits the transcript's access-control posture. Per [ADR-0008](./0008-secondary-objectstore-for-agent-state.md) Decision 2, all agent-state blobs route to the secondary store when configured. The narrative blob and the synthesized successor commit follow that rule with no additional logic.

If a user has `[object_store.secondary]` configured but does **not** want narrative to leave the primary (e.g., they're using secondary for `Segment` audit and don't want intent narratives in the audit store), that's a per-object-kind routing case. Deferred per ADR-0008 Decision 2's "no policy DSL in v1.1" rule. Users who need this configure two `agenticd` instances or disable summarization.

## Decision 7 — No `agentic explain`

Entire ships `entire checkpoint explain` as a first-class verb. We do not.

The summary is a field on the Commit object. The existing `agentic show <commit>` already renders blob fields. It gains:

```bash
$ agentic show <commit> --narrative
# prints the five-section summary, pretty-formatted

$ agentic log --narrative
# log view, each commit annotated with first-line intent

$ agentic diff <a> <b> --narrative
# diff view, narrative field included in the per-dimension diff
```

No new top-level verb. No new product surface. The summary is data, retrievable through the existing read-path machinery, and that is the whole pattern. If a user habitually wants `agentic show <commit> --narrative`, they alias it in their shell; we do not bake a verb for them.

This is the discipline that keeps the surface narrow. Resist the urge.

## Consequences

**Positive**

- Populates `Commit.intent` and adds `Commit.narrative` — the two fields the agent-PR review primitive needs to answer its central question. Today both are zero; this fixes that without re-architecting.
- Async amendment via successor commit preserves both the commit-path latency budget and the immutability guarantee. Same shape as Git's amend semantics; reusable for future field-amendment cases (e.g., delayed eval results).
- Default-off, opt-in, with provider choice is the honest framing for transcript egress. Doesn't surprise users with bills or with prompts-on-Anthropic-servers.
- Borrows a well-shaped feature from a shipping competitor (Entire) without copying their product framing (which would dilute our wedge per [`competitive-brief-entire.md`](../product/competitive-brief-entire.md)).

**Negative**

- Successor-commit-as-amendment is a new pattern in the codebase. It's the right shape, but it requires the ref-update layer to handle "advance ref to a commit that didn't exist when the previous ref-update was issued." Existing `refs::write_branch` is atomic at the ref level but does not enforce a parent-of-current check today. Add an `expect_parent: Hash` parameter; gate the amend ref-write on it.
- Two extra writes per commit (narrative blob + successor commit), plus one extra ref update. At p99 these don't matter; at high write-throughput they're additional pressure on the `ObjectStore`. Benchmark separately.
- The `narrative = ZERO` → eventually `narrative = <hash>` window is observable to anyone reading the ref between ack and amendment. Documented; tooling treats `ZERO` as "not-yet-summarized," not as "intentionally-empty." This is a UX wart we accept for not blocking commits.

**Risks to revisit**

- If providers other than Anthropic become design-partner-pulled (an OpenAI-API customer who refuses to add `ANTHROPIC_API_KEY`), the `Provider` trait expansion is forced. Specify the trait shape now in the implementation comments even if only one impl ships.
- If summarization failures accumulate (provider outage, repeated network errors), the `agentic doctor narrative` queue can grow unboundedly. Cap it; document the cap. Failures older than a TTL get dropped with a structured log line, not retried forever.
- The five-section schema is shipped at `schema_version: "1.0"`. The next time we want to add a section (e.g., "test-coverage delta"), we need a migration story. Don't ship without `schema_version` already in the blob (it is, per Decision 1) — bumping is then a code change, not a data-migration change.
- Generating a `Commit.intent` from the first user message risks confidently misrepresenting the agent's actual goal when the user reformulates mid-session. Mitigate: generate `intent` from the *full* transcript with an explicit "what did the user ultimately ask for" prompt, not from the first message alone. Spec'd in the action items.

## Prior art

- **Entire CLI's `strategy_options.summarize`** — direct shape inspiration; five sections (intent/outcome/learnings/friction/open items) borrowed verbatim. Default-off, non-blocking, local `claude` CLI invocation are all borrowed.
- **Git's `commit --amend`** — pattern for "amend via new commit pointing at same content with one field changed." Our successor-commit approach generalizes this.
- **GitHub Actions / CI summary annotations** — established UX pattern for "narrative content attached to an artifact post-creation, not blocking creation."
- **Open-source PR templates with intent / context / testing sections** — the convention five-section summaries lean on.
- **Internal: ADR-0002 Decision 2 (extended Commit object) + ADR-0008 (secondary store) + ADR-0005 (transcript origin)** — the substrate this ADR composes onto. We are not introducing new substrate, only populating existing fields and adding one (`narrative`).

## Action items

1. [ ] Add `narrative: Hash` field to `crates/agentic-core/src/object.rs` `Commit` struct. Default `Hash::ZERO` for backward compatibility. v1.1 milestone.
2. [ ] Implement `agenticd::summarize` worker in `crates/agenticd/src/summarize.rs`: queue, provider dispatch, blob write, successor-commit synthesis, ref advance with `expect_parent` gating. v1.1 milestone.
3. [ ] Add `expect_parent: Option<Hash>` parameter to `refs::write_branch` and use it for narrative-amendment ref writes. v1.1 milestone; load-bearing for race-free amendment.
4. [ ] Implement `Provider` trait with `AnthropicApiProvider`, `ClaudeCliProvider`, `NoneProvider` impls. v1.1 milestone.
5. [ ] Implement `agentic commit amend-narrative <commit-hash> <blob-path>` for the `provider = "none"` workflow. v1.1 milestone.
6. [ ] Extend `agentic show` with `--narrative` flag; extend `agentic log` and `agentic diff` similarly. v1.1 milestone.
7. [ ] Implement `agentic doctor narrative` — list commits where `narrative = ZERO` but amendment was enqueued + age in queue. v1.1 milestone.
8. [ ] Document `docs/integration/summarization.md`: what's sent, to whom, how to redact, how to verify before enabling. v1.1 milestone.
9. [ ] Specify the prompt used to generate `intent` and `narrative` blobs in `crates/agenticd/src/summarize/prompts.rs`. Pin the prompt with a version string; treat changes as a `schema_version` consideration. v1.1 milestone.
10. [ ] Benchmark the extra writes (narrative blob + successor commit + ref update) against the `snapshot-model.md` §9 targets. Confirm async path does not steal from sync commit-path budget. v1.1 milestone.

See [ADR-0002](./0002-substrate-and-supercommit.md), [ADR-0005](./0005-sessionstore-amendment-to-adr-0004.md), [ADR-0008](./0008-secondary-objectstore-for-agent-state.md), [`competitive-brief-entire.md`](../product/competitive-brief-entire.md), and the v1.1 plan at [`v1.1-plan.md`](../product/v1.1-plan.md) §Workstream 4.
