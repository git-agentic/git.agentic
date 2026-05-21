# ADR-0007: Ephemeral Branches as an Agent-Run Primitive

**Status:** Proposed
**Date:** 2026-05-21
**Deciders:** Toni
**Extends:** [ADR-0002](./0002-substrate-and-supercommit.md) Decision 2 (Commit object as platform API contract)
**Relates to:** [ADR-0005](./0005-sessionstore-amendment-to-adr-0004.md) (`executor/<session_id>` per-session branches for the Claude Agent SDK integration)

## Context

ADR-0005 introduced per-session branches `executor/<session_id>` to mirror Claude Agent SDK transcript frames into the agentic Commit graph in real time. Each `SessionStore.append` becomes a Commit on that branch; the session's full transcript history is the parent chain. This shape works for v1.0 but has rough edges that will compound as more agent-run kinds (LangGraph runs, CrewAI runs, planner→worker fan-out) need similar treatment:

1. **No semantic distinction between durable and ephemeral history.** `executor/<session_id>` lives in the same `refs/heads/` namespace as `main`. Long-running sessions accumulate hundreds to thousands of commits per branch; abandoned sessions never get cleaned up; `agentic log` and `agentic branch --list` interleave production refs with disposable session history.
2. **No "promote" gesture.** When an agent run produces something the team wants to keep — a fixed prompt, a memory edit, an evals run worth comparing against `main` — the only path is `git cherry-pick` semantics implemented by hand. There is no first-class primitive for "this ephemeral run produced a Commit worth promoting to a real branch."
3. **GC story is missing.** Per-session branches grow unboundedly. We have no documented retention window, no abandon-detection, and no namespace-based pruning rule. ADR-0005 acknowledged this implicitly ("retention window unspecified") and deferred it.
4. **The pattern will recur.** Multi-agent runs where a planner fans out to several worker agents need exactly this shape: each worker writes to its own ephemeral branch parented to the same point; the planner promotes the winning one. Building that without a primitive will mean re-inventing namespace conventions per integration.

Pierre Computer Company's Code.Storage exposes an explicit "ephemeral branches" primitive — temporary branches isolated from the default namespace, with retention semantics built in. We don't adopt their implementation (per [ADR-0006](./0006-objectstore-backend-trait.md) Decision 3, managed-Git backends are wrapped behind the `ObjectStore` trait, not adopted as our primitives), but the design idea is worth borrowing because it cleanly factors the durable-vs-disposable distinction at the ref layer.

This ADR introduces ephemeral branches as a first-class primitive in our ref model, defines their lifecycle, promotion gesture, and GC story, and reframes ADR-0005's `executor/<session_id>` branches as an instance of it.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **Ephemeral branches live under `refs/ephemeral/<namespace>/<id>`, distinct from `refs/heads/`.** | Namespace isolation by construction; one rule for GC; no interleaving with production refs in `agentic log` / `agentic branch`. |
| 2 | **Lifecycle: created on first commit, sealed by promote / discard / TTL expiry.** No mid-life rename to `refs/heads/`; promotion creates a new durable ref pointing at the same Commit hash. | Promotion is a value-level act, not a ref-rename. Disposable refs stay disposable. |
| 3 | **`agentic promote <ephemeral-ref> [<dest-branch>]` is the gesture.** Default destination is the requested commit's content-addressed `commit/<hash-prefix>` durable ref. | Surface a single, obvious CLI gesture; resist letting promotion turn into a merge-strategy menu. |
| 4 | **GC: TTL-based (default 14 days since last write), with explicit `agentic ephemeral retain <ref>` to extend.** Promotion bumps TTL to "indefinite" on the destination ref only; the ephemeral ref itself still expires. | Storage cost grows with agent activity; retention must be bounded. |
| 5 | **ADR-0005's `executor/<session_id>` becomes `refs/ephemeral/executor/<session_id>`.** No SDK surface change; the AgenticSessionStore continues to talk to `agenticd` over the same wire. | Drop-in refactor; ADR-0005's semantics are preserved. |
| 6 | **The Commit-object schema does not change.** Ephemerality is a ref-layer property, not a Commit-layer property. | Preserves the ADR-0002 Decision 6 storage / API split: backends and ref models can change without breaking the platform contract. |

---

## Decision 1 — Namespace and ref layout

Ephemeral branches live under their own ref prefix:

```text
.agentic/
  refs/
    heads/
      main                          # durable
      release/2026-05-26            # durable
    ephemeral/
      executor/
        sess-7f3a9c…                # ADR-0005 session branch
      langgraph/
        run-2026-05-21T10:00:00Z    # LangGraph compile-and-invoke run
      planner/
        attempt-0/worker-a          # multi-agent fan-out worker A
        attempt-0/worker-b          # multi-agent fan-out worker B
```

`<namespace>` partitions ephemeral refs by source — one per framework integration, plus a default `local/` for human-driven ad-hoc work. `<id>` is opaque to the ref system; integrations choose the format (session ID, run ID, ULID, timestamp).

What this buys:

- `agentic log` and `agentic branch --list` exclude `refs/ephemeral/**` by default. `--all` or `--ephemeral` opts in.
- `agentic gc` (v1.1) walks `refs/ephemeral/**` and reaps under the TTL rule in Decision 4 without touching durable refs.
- Future namespaces (CrewAI, AutoGen, in-house planners) add their own subdirectory without coordinating naming conventions.

## Decision 2 — Lifecycle

```
                         ┌──────────────────┐
                         │ promote          │──▶ durable ref in refs/heads/
                         │ (Decision 3)     │
                         └──────────────────┘
                                  ▲
                                  │
[ create ]──▶ [ active ]──▶ [ sealed ]
   ↑              │              │
   │              │              ├──▶ discard (immediate GC)
   │              │              └──▶ TTL expiry ──▶ GC
   │              ▼
   │         [ failed ]──▶ TTL expiry ──▶ GC
   └─ implicit: first commit to a previously-unknown ephemeral ref
```

Active is the default; an ephemeral ref accepts commits whose `parent` resolves into its chain. Sealed (no more writes accepted) is the terminal state and is set by `agentic ephemeral seal <ref>`, by promotion, or by discard. Once sealed, an ephemeral ref is read-only until GC reaps it.

There is no mid-life rename to `refs/heads/`. Promotion is a value-level operation that creates a new durable ref pointing at the same Commit hash. The ephemeral ref continues to live until TTL expiry; it can co-exist with the promoted durable ref for the entire retention window. This is the discipline that prevents promotion from leaking into the operational model — a promoted Commit's history can still be GC'd; only the durable ref pinning it is permanent.

## Decision 3 — Promotion gesture

```bash
$ agentic promote refs/ephemeral/executor/sess-7f3a9c
# default: creates refs/heads/commit/7f3a9c… pointing at the tip Commit hash

$ agentic promote refs/ephemeral/langgraph/run-2026-05-21T10:00:00Z fix-llm-hallucination
# explicit destination: refs/heads/fix-llm-hallucination
```

`promote` is the single CLI surface. Behaviour:

- Reads the tip Commit hash of the ephemeral ref.
- Creates the destination durable ref (`refs/heads/<dest>`) pointing at that hash. Fails if the durable ref already exists; the caller must pass `--force` to overwrite.
- Does NOT seal the ephemeral ref. Promotion is idempotent and non-destructive on the ephemeral side; the caller may later seal or discard.
- Records the promotion as a synthesised Commit on the destination ref with a `promote_of` field referencing the ephemeral ref name and tip hash. (This is the ADR-0002 `signatures` array territory — promotions are attestations.)

Out of scope for v1.1:

- Merging multiple ephemeral refs into one durable ref. That's the planner-fan-out problem and deserves its own ADR if a design partner pulls.
- Cherry-pick semantics across ephemeral chains. Use `git cherry-pick` on the code dimension; the other dimensions follow the rollback machinery from ADR-0002 Decision 3.
- A web UI for promotion. CLI-only per [ADR-0001 Decision 9](./0001-architecture-foundations.md).

## Decision 4 — GC and retention

Default retention is **14 days since the last write** to the ephemeral ref. `agenticd` records last-write timestamps in `.agentic/ephemeral-meta/<namespace>/<id>.toml`:

```toml
created_at  = "2026-05-21T10:00:00Z"
last_write  = "2026-05-21T10:42:13Z"
ttl_days    = 14
sealed      = false
seal_reason = ""
```

`agentic gc` (v1.1) reaps any ephemeral ref whose `last_write + ttl_days < now`. Sealed refs use `seal_at` instead of `last_write`. The reap is two-phase: (1) delete the ref file; (2) leave the Commit objects in the object store for the next blob-level GC pass. This preserves the existing 2PC discipline from ADR-0002 Decision 3 — ref operations are separable from object-store operations.

Extending retention:

```bash
$ agentic ephemeral retain refs/ephemeral/executor/sess-7f3a9c --days 90
$ agentic ephemeral retain refs/ephemeral/executor/sess-7f3a9c --forever
```

`--forever` sets `ttl_days = 0` (sentinel "no GC"). Forever-retain is the escape hatch; we expect it to be rare. If a design partner sets `--forever` on more than a handful of refs, that's a signal they want a durable namespace, not an ephemeral one — log it to the v1.1 plan as a follow-up.

Promotion does NOT set `--forever` on the ephemeral ref. The promoted destination ref is permanent (per Decision 2); the ephemeral ref still expires. This is intentional: it prevents promotion-as-pinning from quietly inflating storage.

## Decision 5 — ADR-0005's `executor/<session_id>` is rehomed

ADR-0005 Decision 1 specified per-session branches at `executor/<session_id>`. With this ADR, those branches live at `refs/ephemeral/executor/<session_id>`. No SDK surface change is required:

- `AgenticSessionStore` already speaks `Commit` objects to `agenticd` over the wire — it never references the ref path directly.
- `agenticd`'s session-mapping layer is the only place that translates `<session_id>` to a ref path; that mapping moves from `refs/heads/executor/<session_id>` to `refs/ephemeral/executor/<session_id>`.
- `agentic log refs/ephemeral/executor/<session_id>` continues to work; `agentic log` without arguments stops showing per-session traffic by default (improvement for users; no breaking change for tooling).

This refactor is a one-day v1.1 task and should land alongside the ephemeral-branches primitive itself, not as a separate sprint.

## Decision 6 — Commit-object schema is unchanged

Ephemerality is a property of the ref, not of the Commit. A Commit on `refs/ephemeral/executor/sess-7f3a9c` has the same on-disk shape as a Commit on `refs/heads/main`. This matters for three reasons:

- **ADR-0002 Decision 2 (Commit-as-platform-API) is preserved.** Platforms produce Commits with the standard schema; the ref they target is configuration, not part of the Commit object.
- **ADR-0006 Decision 5 (backend selection is config, not API) is preserved.** Different backends store ephemeral and durable Commits identically; only the ref-layer GC rule differs.
- **Promotion is cheap.** Creating `refs/heads/<dest>` pointing at an ephemeral Commit hash is a single atomic ref write — same machinery as the existing `refs::write_branch` in `crates/agentic-core/src/refs.rs`. No object copy, no rewrite.

If a future Commit needs to carry "I came from an ephemeral run" provenance, that's expressed via the existing `signatures` array (ADR-0002 Decision 2) — an attestation type like `EphemeralOrigin { namespace, id, tip_hash }`. We don't add a new top-level Commit field.

## Consequences

**Positive**

- A single, named primitive replaces what was about to become per-framework ad-hoc conventions. LangGraph integration, Executor integration, future CrewAI integration all use the same ref namespace and the same retention rule.
- The promotion gesture gives design partners a clean answer to "an agent run produced something worth keeping — what's the workflow?" Today the answer is "we'll figure that out"; with this ADR it's `agentic promote <ref>`.
- GC has a documented rule, which removes a known v1.0 → v1.1 risk (unbounded ref growth in the Executor sidecar deployment).
- The Code.Storage design idea is borrowed without taking on its implementation as a dependency, consistent with [ADR-0006 Decision 3](./0006-objectstore-backend-trait.md).

**Negative**

- The refactor of ADR-0005's `executor/<session_id>` is a behavioral change for any in-flight Executor deployment using the v1.0 ref path. We don't have any yet, but the migration discipline still has to be written (Action 4 below).
- `agentic log` semantics change for users who were inadvertently relying on per-session branches showing up in the default listing. We mitigate via release notes and an `--all` flag, but it's a behavioral break.
- TTL of 14 days is a guess. The real number is "however long a typical design partner takes to look at an agent run and decide whether to promote." We learn that during pilots; the 14-day default may move.

**Risks to revisit**

- If multi-agent planner→worker fan-out becomes a common pattern, we likely want a "session group" abstraction over multiple ephemeral refs that share a parent. Open a follow-up ADR when the first design partner asks.
- If managed-Git backends ([ADR-0006](./0006-objectstore-backend-trait.md) Decision 3) start exposing their own ephemeral-branch primitives natively, we have two ephemeral-branch models stacked. The `ManagedGitStore` adapter must not surface theirs through our `ObjectStore` trait; ours is the contract.
- TTL-based GC interacts with the "destructive migration loses post-snapshot data" caveat in [ADR-0002 Decision 5](./0002-substrate-and-supercommit.md). If a destructive migration is recorded on an ephemeral ref and the ref expires before someone notices, the migration's pre-state is gone. Mitigation: warn loudly when a Commit on an ephemeral ref includes a destructive schema migration; consider auto-promotion of such commits to a durable `audit/` namespace.

## Prior art

- **Pierre Computer Company's Code.Storage** — exposes ephemeral branches as a first-class primitive with built-in retention semantics. Inspiration for the namespace-isolation + TTL shape; not adopted as a dependency.
- **GitHub's pull-request refs** (`refs/pull/<n>/head`) — namespace isolation for non-default work. Different lifecycle (PR-tied, not TTL-tied), but the namespace pattern is the same.
- **Sapling's `pr/` namespace** — similar precedent.
- **Internal: ADR-0005's `executor/<session_id>`** — the proximate cause of this ADR. Generalising one integration's ad-hoc convention into a named primitive before we have a second integration imitating it.

## Action items

1. [ ] Implement `Refs::write_ephemeral(namespace, id, hash)`, `Refs::resolve_ephemeral(namespace, id)`, and `Refs::list_ephemeral()` in `crates/agentic-core/src/refs.rs`. v1.1 milestone.
2. [ ] Add `refs/ephemeral/` exclusion to `agentic log` and `agentic branch --list` defaults; add `--ephemeral` / `--all` opt-in flags. v1.1 milestone.
3. [ ] Implement `agentic promote` in `crates/agentic-cli`. Default-destination logic + `--force` flag. v1.1 milestone.
4. [ ] Implement `agentic ephemeral seal | discard | retain | list`. v1.1 milestone.
5. [ ] Implement TTL-based GC in `agenticd` (separable from blob-level GC). v1.1 milestone.
6. [ ] Refactor `AgenticSessionStore` (per ADR-0005) to write to `refs/ephemeral/executor/<session_id>`. Migration note for any v1.0 pilot deployment. v1.1 milestone.
7. [ ] Document the destructive-migration-on-ephemeral-ref warning rule (Risks to revisit). v1.1 milestone, possibly via follow-up ADR if it grows.
8. [ ] Add `EphemeralOrigin` attestation type to the ADR-0002 `signatures` schema. v1.1 milestone.

See [ADR-0001](./0001-architecture-foundations.md), [ADR-0002](./0002-substrate-and-supercommit.md), [ADR-0005](./0005-sessionstore-amendment-to-adr-0004.md), [ADR-0006](./0006-objectstore-backend-trait.md), and [`docs/product/v1.1-plan.md`](../product/v1.1-plan.md).
