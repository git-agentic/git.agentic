# ADR-0008: Secondary `ObjectStore` for Agent-State Objects

**Status:** Proposed
**Date:** 2026-05-21
**Deciders:** Toni
**Extends:** [ADR-0006](./0006-objectstore-backend-trait.md) Decision 5 (backend selection is config, not API)
**Relates to:** [ADR-0001](./0001-architecture-foundations.md) Decision 5 (don't replace Git for code), [ADR-0002](./0002-substrate-and-supercommit.md) Decision 1 (Git substrate for code dimension only)

## Context

The blob store (`.agentic/objects/` and its eventual GCS / S3 / `ManagedGitStore` siblings per [ADR-0006](./0006-objectstore-backend-trait.md)) holds every non-code object git.agentic produces: Segment manifests, Tree objects, Commit objects, signatures, eventually attestations. The Git repository holds the code dimension. Today these are co-located in the user's repository on disk and pushed together to the user's `origin` remote.

For self-hosted private repositories this is fine. For two adjacent scenarios it is not:

1. **OSS code repository, proprietary agent state.** A public OSS project uses git.agentic during development. The code is permissively licensed and lives on a public GitHub repo. The agent's prompts, transcript frames, evals history, and memory snapshots contain proprietary system instructions, customer-derived test fixtures, and pgvector content the project does not want public — and per [ADR-0013](0013-secret-scanner.md) the v1.0 secret scanner (PR-3 of the hardening sprint) will be best-effort, not a substitute for not publishing the data at all. Today the user has to choose: push agent state and leak, or don't push and lose the cross-machine workflow.
2. **Code repo and agent-state repo have different access-control populations.** A company's monorepo is readable by the whole engineering org. Agent transcripts may include customer-tier data subject to tighter access control (CSAT escalation transcripts, security-team prompts, regulated-domain memory). The blast radius of "anyone with monorepo read access can see every agent transcript" is too wide.

Both scenarios share a shape: the code dimension and the non-code dimensions want to live in **different storage locations with different access-control posture**. The architectural seam already exists — code goes to Git, everything else goes through the `ObjectStore` trait — so the question is whether we expose that seam to config and how.

[Entire CLI](../product/competitive-brief-entire.md) ships exactly this pattern (`entire enable --checkpoint-remote github:org/repo` + `ENTIRE_CHECKPOINT_TOKEN` env var). Their shape is small and well-judged: structured `{provider, repo}` config, HTTPS coercion when a dedicated token is present (because SSH keys are tied to the user, not the project), graceful skip with a warning if the secondary remote is unreachable so the primary push isn't blocked, fork-detection to avoid accidentally pushing a fork's checkpoints upstream. Stealing this shape is cheaper than designing one.

This ADR formalises the secondary-`ObjectStore` config and the failure semantics. It does **not** introduce a new storage backend — it composes existing `ObjectStore` impls per [ADR-0006](./0006-objectstore-backend-trait.md) with a per-dimension routing rule.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **`agenticd` accepts two `ObjectStore` configs: a primary (the default for all objects) and an optional secondary, with a per-object-kind routing rule.** | Compose existing backends; no new trait surface. |
| 2 | **Default routing rule: code-dimension references stay with Git on `origin`; all `Commit` / `Segment` / `Tree` / signature objects route to the secondary `ObjectStore` when one is configured.** | Matches the OSS + private-agent-state use case directly without a per-object policy file. |
| 3 | **Auth for the secondary store is independent of the primary.** `AGENTICD_SECONDARY_OBJECT_STORE_TOKEN` env var; per-backend auth conventions inherit from the underlying `ObjectStore` impl. | Decouples blast radius; matches Entire's `ENTIRE_CHECKPOINT_TOKEN`. |
| 4 | **Failure semantics: secondary-store writes are best-effort by default; the primary commit succeeds and the user sees a warning. A `secondary_required = true` mode promotes secondary failure to a commit failure.** | OSS use case wants graceful degradation; regulated use case wants loud-fail. Both have to be available. |
| 5 | **No change to the public SDK surface.** Routing is `agenticd` config; the `Commit` object's wire shape is unchanged. The SDK continues to speak `Commit` objects per [ADR-0002 Decision 6](./0002-substrate-and-supercommit.md). | Storage-layer concept; must not leak into the platform API. |
| 6 | **The rollback path reads from whichever store currently has the object — primary first, secondary on miss.** Rollback is read-only against the stores; it does not care which one served the bytes. | Keeps rollback as the simple gesture it is. |

---

## Decision 1 — Two configured stores, one composed router

`agenticd.toml` gains a `[object_store.secondary]` table that mirrors the existing `[object_store]` (primary) shape introduced in [ADR-0006](./0006-objectstore-backend-trait.md) Workstream 1 milestone W1.1.6. Example:

```toml
[object_store]
backend = "fs"
path    = ".agentic/objects"

[object_store.secondary]
backend  = "gcs"
bucket   = "acme-agent-state"
prefix   = "git.agentic/proj-x/"
required = false
```

Or, for the OSS-using-managed-Git-checkpoint-repo pattern:

```toml
[object_store]
backend = "fs"
path    = ".agentic/objects"

[object_store.secondary]
backend  = "managed_git"
provider = "github"
repo     = "acme-private/proj-x-checkpoints"
required = false
```

At daemon start, `agenticd` constructs a `RoutedObjectStore { primary, secondary, routing_rule }` rather than a single `Arc<dyn ObjectStore>`. `RoutedObjectStore` implements the same `ObjectStore` trait, so call sites in `crates/agentic-core/src/commit.rs` and `crates/agenticd/src/rollback.rs` need no changes beyond the constructor.

If `[object_store.secondary]` is absent, `RoutedObjectStore` collapses to a thin pass-through over the primary — zero-cost, no behavior change. This is the default for the v1.0 demo path.

## Decision 2 — Routing rule: agent-state objects to secondary

The routing rule is a single function on object kind:

```rust
fn route(kind: ObjectKind, ctx: &RoutingCtx) -> StoreSelector {
    use ObjectKind::*;
    match kind {
        // Code-dimension references are addressed via Git SHAs in the
        // Commit object's code field. The code itself is pushed to Git's
        // `origin`, not through ObjectStore. Nothing to route here.

        // Agent-state objects route to secondary when configured.
        Commit | Segment | Tree | Blob | Signature => ctx.secondary_selector(),
    }
}
```

`Blob` covers prompt blobs, MCP manifests, and any user-supplied tool fingerprint payload. All of these are agent-state, not code (per [ADR-0001 Decision 1](./0001-architecture-foundations.md) and ADR-0002 Decision 2). Nothing about the routing rule treats the code dimension — code stays in Git, Git remotes are independent of `ObjectStore`.

The routing rule is **not** policy-configurable in v1.1. We are not building a per-object-kind config language; we are building a default that satisfies the two named scenarios. If a design partner pulls a third scenario that needs per-kind routing (e.g., "keep `Commit` objects on origin but route `Segment` to secondary because the segments contain customer data and the commits don't"), open a follow-up ADR — don't expand the routing surface preemptively.

## Decision 3 — Auth boundary

Each `ObjectStore` impl has its own auth convention:

- `FsObjectStore` — no auth (local filesystem).
- `GcsObjectStore` — service account from `GOOGLE_APPLICATION_CREDENTIALS` per [ADR-0006](./0006-objectstore-backend-trait.md).
- `S3ObjectStore` — AWS SDK default credential chain.
- `ManagedGitStore` — provider-specific.

For the secondary store, two new env vars decouple primary and secondary auth without changing per-backend conventions:

- `AGENTICD_SECONDARY_OBJECT_STORE_TOKEN` — when set, the secondary store impl receives this as its credential, overriding the default credential chain. Honoured by `GcsObjectStore`, `S3ObjectStore`, and `ManagedGitStore` (where it injects an HTTPS token).
- `AGENTICD_SECONDARY_OBJECT_STORE_TOKEN_FILE` — path-to-file variant for environments that mount tokens as files (Kubernetes service-account tokens).

For `ManagedGitStore` specifically, if a token is present and the configured remote is an SSH URL, the adapter coerces to HTTPS so the token can be used (same trick Entire pulls). This is the documented behavior; the override is in `agenticd.toml` as `[object_store.secondary] force_ssh = true` for users who want loud-fail rather than coercion.

The blast-radius split that this buys is the whole point: the OSS scenario can publish the code repo with a `GITHUB_TOKEN` for `origin` push that has only the OSS repo's permissions, and a separate `AGENTICD_SECONDARY_OBJECT_STORE_TOKEN` scoped to the private checkpoints repo. Leaking the OSS token does not leak agent state.

## Decision 4 — Failure semantics

Two modes, selected by `[object_store.secondary] required = <bool>` (default `false`):

**`required = false` (best-effort, default)**

- On commit: write to primary, then asynchronously enqueue the secondary write. The commit ack is sent to the SDK as soon as the primary succeeds. A failing secondary write logs a structured warning and increments a metric, but does not propagate failure to the user.
- On rollback: read from primary first; on miss, read from secondary. If the secondary is unreachable, rollback proceeds with whatever primary contains — degraded but not blocked.
- Use case: OSS / convenience tier. Matches Entire's default behavior.

**`required = true` (loud-fail)**

- On commit: write to both stores; the commit ack waits for both. A failing secondary write fails the commit, with the same error model as a failed primary write. The SDK sees a `CommitError::SecondaryStoreUnreachable` — distinct error kind so callers can retry the secondary specifically.
- On rollback: a missing object in the secondary store fails rollback if the routing rule says that object should live there. No fallback to primary.
- Use case: regulated / compliance tier. The transcript MUST land in the audit-grade store before the commit is considered durable.

This is the same shape as [ADR-0004](./0004-realtime-agenticd-for-executor.md) Decision 4's loud-fail (preserved in [ADR-0005](./0005-sessionstore-amendment-to-adr-0004.md) via a synchronising `PreToolUse` hook): we don't pretend best-effort is sufficient for everyone, but we don't force loud-fail by default.

`required = true` is incompatible with `secondary` being absent — the config schema rejects the combination at daemon start with a clear error, not at first commit.

## Decision 5 — No SDK surface change

The Python SDK (`agentic.client.Client`) continues to call `commit(envelope)` and receive `CommitId` back. The Commit object's wire shape per [ADR-0002 Decision 2](./0002-substrate-and-supercommit.md) is unchanged. Nothing about routing is visible to the SDK or to platform integrators.

This is non-negotiable per [ADR-0002 Decision 6](./0002-substrate-and-supercommit.md): the SDK's public surface trades in `Commit` objects only. No store paths, no routing flags, no per-call backend selection. If a platform needs different routing per commit, they configure a different `agenticd` instance — not a different call shape.

The one observable that does leak is in `CommitAck`: a new `secondary_status: Option<SecondaryStatus>` field reports `{Synced, Pending, Failed}` when a secondary store is configured. SDK clients can opt-in to read it; default is to ignore. This is the minimum surface the audit-tier use case requires (callers need to know whether to retry), and it's additive so it doesn't break existing callers.

## Decision 6 — Rollback reads from either store

`rollback.rs` already iterates over object hashes resolved from the target `Commit`. With `RoutedObjectStore`, each `get(hash)` call tries primary first, then secondary on miss. The fall-through is fast for the default (`required = false`) and skipped for `required = true` (which fails rather than falls through to wrong-store).

This matters because of one specific scenario: a developer who has committed against `[object_store.secondary]` enabled, then disabled it (or moved to a machine without secondary credentials), should still be able to roll back to objects that were never written to primary. The fall-through covers that case for the best-effort mode. For loud-fail mode, the operator's choice to require secondary durability is the operator's choice to require secondary availability for rollback.

## Consequences

**Positive**

- Closes the OSS-public-code, private-agent-state use case cleanly. Today this is a hard wall.
- Decouples blast radius between code-repo access and agent-state access. Real compliance ask once the first regulated-domain pilot lands.
- Composes existing `ObjectStore` impls; no new backend code, no new trait, no SDK change.
- Borrows Entire's well-trodden shape (structured config, dedicated token, graceful skip) without taking on their implementation as a dependency.

**Negative**

- Two-store config is a new operational surface. Sync drift between primary and secondary in `required = false` mode is a real failure class — we need a `agentic doctor secondary` command to diff hash sets and report drift. Add to action items.
- The `secondary_status` field on `CommitAck` is the one place this leaks to the SDK. Once added, removing it is a breaking change. Worth its cost; flagging the lock-in.
- Rollback fall-through (Decision 6) means a missing object in primary that's present in secondary will succeed silently. This is the right behavior for best-effort mode but masks a real primary-store integrity issue. Mitigation: `agenticd` logs at WARN every primary-miss-secondary-hit during rollback; metric counter for ops dashboards.

**Risks to revisit**

- If a design partner pulls per-object-kind routing (e.g., "Segment to one bucket, Commit to another"), we will be tempted to add a routing-policy DSL. Resist; open a follow-up ADR and only build it if a second partner pulls.
- The `force_ssh` escape hatch in `ManagedGitStore` is there for "don't silently swap my SSH auth for an HTTPS token." If we find users hitting it for surprising reasons (e.g., SSO-gated SSH but no HTTPS access at all), reconsider the default coercion.
- Cross-store consistency in failure injection: the `[object_store.secondary] required = true` mode must be exercised by the same failure-injection harness that covers the 2PC staging order from [ADR-0002 Decision 3](./0002-substrate-and-supercommit.md). The staging order extends: blobs to primary → blobs to secondary (if required) → build Commit → write Commit to primary → write Commit to secondary (if required) → ref update. This is more surface to fuzz; budget for it.

## Prior art

- **Entire CLI's `checkpoint_remote` + `ENTIRE_CHECKPOINT_TOKEN`** — the direct shape inspiration. Their `provider:owner/repo` structured config, dedicated token, HTTPS coercion when token is present, and graceful-skip-on-unreachable behavior are all borrowed verbatim or near-verbatim. See [competitive-brief-entire.md](../product/competitive-brief-entire.md).
- **Git's `insteadOf` and dual-remote patterns** — well-understood operational shape for "code here, metadata there." Different protocol, same intent.
- **Buildkite Artifacts / GitHub Actions Artifact retention with separate storage** — CI-world precedent for "metadata about a build lives in different storage than the build inputs."
- **GCS/S3 cross-account write patterns with KMS-scoped service accounts** — pattern for compliance-tier secondary storage.

## Action items

1. [ ] Implement `RoutedObjectStore` in `crates/agentic-core/src/store.rs` (or a new `routed.rs`). Pass-through-when-absent semantics. v1.1 milestone, gated on [ADR-0006](./0006-objectstore-backend-trait.md) W1.1.1 (trait `delete` + `list_prefix` landed).
2. [ ] Extend `agenticd.toml` schema in `crates/agenticd/src/main.rs` with `[object_store.secondary]` + `required` + env-var conventions. v1.1 milestone.
3. [ ] Add `AGENTICD_SECONDARY_OBJECT_STORE_TOKEN(_FILE)` env-var plumbing to each `ObjectStore` impl. v1.1 milestone, parallel with backend-specific work.
4. [ ] Implement `CommitAck::secondary_status` and propagate through SDK types. v1.1 milestone; coordinate with `sdk/python/agentic/types.py` types update.
5. [ ] Failure-injection tests for the extended 2PC staging order (primary → secondary blobs → Commit → primary → secondary → ref). v1.1 milestone; same harness as ADR-0002 Decision 3 coverage.
6. [ ] `agentic doctor secondary` — list-prefix diff between primary and secondary, report drift, propose reconciliation actions. v1.1 milestone; depends on Action 1.
7. [ ] Document the OSS-with-private-checkpoints workflow at `docs/integration/secondary-object-store.md`. v1.1 milestone.
8. [ ] Decide whether `ManagedGitStore`'s HTTPS coercion default is correct (Risks to revisit). Re-evaluate after one design partner uses it.

See [ADR-0001](./0001-architecture-foundations.md), [ADR-0002](./0002-substrate-and-supercommit.md), [ADR-0006](./0006-objectstore-backend-trait.md), [`competitive-brief-entire.md`](../product/competitive-brief-entire.md), and the v1.1 plan at [`v1.1-plan.md`](../product/v1.1-plan.md) §Workstream 4.
