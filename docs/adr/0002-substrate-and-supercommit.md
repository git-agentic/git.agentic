# ADR-0002: Substrate Approach — Git Core, Content-Addressed Manifest, Coordinator-Mediated Two-Phase Commit

**Status:** Accepted
**Date:** 2026-05-19
**Deciders:** Toni
**Extends:** [ADR-0001](./0001-architecture-foundations.md)

## Context

[ADR-0001](./0001-architecture-foundations.md) established the tuple, the content-addressed object store, and the delegation of code versioning to Git. Since ADR-0001 was written, the company's external positioning has sharpened to **"the git host built for when most commits are written by agents,"** with a **platform-led GTM** that targets the ~15–30 agent platforms (Cursor, Cognition, Factory, Magic, Replit Agent, Devin, etc.) that originate most agent commits.

This positioning forces a substrate decision the prior ADR left implicit:

- How do we store the (code + prompts + tools + model + memory + schema + intent + transcript + evals + cost) tuple such that integration is an afternoon's work for an agent platform?
- How do we make atomic rollback genuinely atomic rather than aspirational, given that we cross a Git boundary and an opaque-blob boundary in every commit?
- How does the data model differentiate structurally from GitHub's commit-and-PR substrate, so that GitHub cannot trivially catch up by adding a few sidecar fields to their existing model?

This ADR locks in **Approach C** as the substrate architecture, extends the Commit object to make it the platform API contract, fixes the two-phase commit staging order that makes atomic rollback honest, and surfaces two product constraints we must communicate to design partners before they pilot.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **Approach C: Git core for code, content-addressed blob store for non-code, coordinator on top.** | Preserves a fully-post-Git substrate's structural moat at the *data model* layer while keeping Git-native interop at the *storage* layer. |
| 2 | **Extend the Commit object with `intent`, `plan`, `transcript`, `evals`, `cost_cents`, and `signatures`.** | The Commit object IS the platform API contract; there is no separate API surface. |
| 3 | **Two-phase commit staging order: blobs first → content hashes → Git push as single commit point.** | The 2PC plumbing is the load-bearing technical risk for atomic rollback; getting it wrong destroys the company's central promise. |
| 4 | **Production deployments require filesystem snapshot-capable storage (ZFS / Btrfs / EBS or equivalent).** | Logical export is acceptable for MVP demos but breaks at production scale; design for it from day one or rip it out at the worst possible moment. |
| 5 | **Rollback semantics for destructive migrations are explicitly bounded.** | Every demo in the industry hand-waves this. We tell design partners during pilot, not after. |
| 6 | **A fully post-Git Merkle DAG remains the v2+ option behind a stable platform API contract.** | The API contract is independent of the storage layer; if per-hunk attestation or multi-agent commit graphs force the swap later, platforms don't break. |

---

## Decision 1 — Approach C

We evaluated three substrate architectures.

### Option A — Git-native with sidecar data

Use Git for code as-is. Store prompts, tools-manifest, model-refs, memory snapshots, and schema as Git refs (`refs/agentic/...`) or Git notes. Snapshot is a "supercommit" that pins the tuple of refs together.

### Option B — Fully post-Git Merkle DAG with Git compatibility shim

Build on a content-addressed Merkle DAG (Sapling/JJ/Pijul flavor) where the unit is the state tuple itself. A Git compatibility shim exposes `git clone` / `git push` for the code-tree portion only. Atomic rollback is native — flipping the tuple pointer is one operation.

### Option C — Git core + content-addressed blob store + coordinator (chosen)

Code lives in real Git with full compatibility. Prompts, tools-manifest, and model-refs live alongside in Git as text (they fit). Memory snapshots, schema dumps, transcripts, evals, intent, plan, and the manifest itself live in our content-addressed blob store (`agentic-core/store.rs`, established by ADR-0001 Decision 2). The Rust coordinator (`agenticd`) stages blobs, collects content hashes, and pushes to Git with the Commit hash referenced from a Git note as the single commit point.

### Trade-off table

| Dimension                          | A: Git-native sidecar | B: Post-Git Merkle DAG | C: Coordinator (chosen) |
|------------------------------------|-----------------------|------------------------|-------------------------|
| Code-layer interop (`git clone`)   | Native                | Via shim               | Native                  |
| Suitable storage for large blobs   | Poor                  | Native                 | Native                  |
| Atomic rollback                    | Bolted on             | Native                 | Via coordinator         |
| Refs explosion at agent scale      | Real risk             | Not an issue           | Not an issue            |
| GitHub mirroring preserves sidecar | Drops silently        | N/A                    | Manifest in notes       |
| Integration cost for platforms     | Low                   | High                   | Low                     |
| Structural moat vs. GitHub         | Partial               | Strongest              | Strong (data model)     |
| Build cost for us                  | Low                   | High                   | Moderate                |
| Door to (B) later                  | —                     | —                      | Storage swappable       |

### Rationale

The decisive argument is adoption. Platform-led GTM requires that integration is an afternoon's work for Cursor / Cognition. Approach A's fragility (refs explosion, fake interop, blob unsuitability) compromises the wedge. Approach B's adoption cost — customers must push to a new substrate, not just add a remote — kills the early funnel. Approach C preserves the structural moat at the *data model* layer (the supercommit manifest is the differentiator that GitHub cannot bolt onto their existing substrate) while keeping the *storage* layer Git-compatible at the code dimension. The moat is in what we record about agent behavior and how we coordinate the rollback transaction, not in how the bytes are laid out on disk.

## Decision 2 — Extending the Commit object

ADR-0001 and [snapshot-model.md §1.4](../architecture/snapshot-model.md) defined the Commit object with the six tuple dimensions plus author / timestamp / message / parent. We extend it with six new fields that make the Commit object usable as the platform API contract:

```rust
pub struct Commit {
    // From ADR-0001
    pub parent:          Option<Hash>,
    pub author:          String,
    pub timestamp:       u64,
    pub message:         String,
    pub code_sha:        GitSha,
    pub prompts:         Hash,
    pub tools:           Hash,
    pub model:           Hash,
    pub memory_snapshot: Hash,
    pub schema_version:  SemVer,

    // New in ADR-0002
    pub intent:          Hash,            // Blob: what the agent was asked to do
    pub plan:            Hash,            // Blob: what the agent decided to do
    pub transcript:      Hash,            // Blob: tool transcript (reads/edits/errors/retries)
    pub evals:           Hash,            // Blob: standardized eval results
    pub cost_cents:      u32,             // Compute cost producing this commit
    pub signatures:      Vec<Attestation>,// Platform + reviewer attestations
}
```

This is the artifact every product surface composes against:

- The agent-PR review primitive renders intent / plan / transcript / evals / cost from the manifest.
- Rollback flips a branch ref to a prior Commit hash; the coordinator restores the dimensions the prior Commit references.
- Attestation chains are the `signatures` array.
- The platform integration is "produce a Commit object with these fields populated and ship it to `agenticd` via the SDK."

The structural choice this encodes: **there is no separate "platform API" — the API IS the Commit-object schema.** Platforms produce Commits; everything downstream reads from them. This is what makes the integration tractable in an afternoon and what keeps the storage layer swappable later (Decision 6).

The five new blob references (`intent`, `plan`, `transcript`, `evals`) follow the same canonical-serialization + BLAKE3 hashing model as the existing dimensions ([snapshot-model.md §1.1](../architecture/snapshot-model.md)). The `evals` blob carries a standardized schema (TBD; see Action 7) so the agent-PR primitive can render eval deltas without bespoke per-platform parsing.

## Decision 3 — Two-phase commit staging order

The single largest operational risk in Approach C is naive two-phase commit between Git and the blob store. If we push to Git first and the blob write subsequently fails, we have a manifest reference pointing at nothing. If we write blobs first and the Git push fails, we have orphan blobs (cheap) but no public state change — recoverable. The order is not interchangeable. Therefore:

### Required staging order

1. **Stage all non-Git blobs** to the content-addressed object store: memory snapshot segments, schema dump, prompts tree, tools manifest, transcript, evals, intent, plan. Collect their content hashes.
2. **Construct the Commit object** referencing those hashes.
3. **Write the Commit blob** to the object store. Capture its hash.
4. **Push to Git** with the Commit hash referenced from a Git note attached to `code_sha`, OR via a `refs/agentic/manifests/<commit-hash>` ref. **This Git push is the single commit point.**
5. **Update the branch ref** (`refs/heads/<branch>` in our object store) to point at the new Commit hash.

### Failure modes

- **Steps 1–3 fail:** orphan blobs in the object store. GC reclaims them on schedule. No public state has changed.
- **Step 4 fails** (Git push rejected, network error): retry idempotently. Same blobs, same content hashes, no duplicates.
- **Step 5 fails after Step 4 succeeds:** the manifest is durable in Git as an orphan ref; the branch can be advanced on retry. No data lost.

This is the discipline that turns "atomic rollback" from marketing copy into truth. It is also the single piece of plumbing every contributor must understand. It belongs in `crates/agentic-core/src/commit.rs` (new) with an explanatory comment block, not as tribal knowledge.

[snapshot-model.md §4](../architecture/snapshot-model.md) describes the in-process atomicity of capturing the tuple's six dimensions. This ADR adds the durability-boundary atomicity that turns an in-memory Commit object into a durable, recoverable, atomically-rollback-able record.

## Decision 4 — Storage capability as part of the contract

[snapshot-model.md §3](../architecture/snapshot-model.md) establishes the segment-based snapshot model that lets us snapshot pgvector in <2s, using either logical decoding or trigger-based row streaming. That model relies on the segment writer keeping up with the agent's write rate.

For MVP demos and small-scale design-partner pilots, segments on commodity SSD storage with periodic logical export are acceptable.

For any production deployment, the daemon must run on **filesystem snapshot-capable storage**: ZFS, Btrfs, EBS, or equivalent. The failure mode that drives this requirement is concrete: when the segment writer falls behind under sustained heavy write load, the only way to take a coherent snapshot in bounded time is a filesystem-level snapshot rather than a logical read of an out-of-date segment stream. Designing `agenticd`'s storage interface around filesystem-snapshot capabilities from day one — even when MVP demos don't exercise the path — prevents an architectural rip-out at the worst possible moment (the first production deployment under real load).

`agentic init` will detect storage capability and warn if running on non-snapshot-capable filesystems. The warning is informational at MVP scale and becomes a hard guard for production deployments in v1.1.

## Decision 5 — Bounded rollback for destructive migrations

[snapshot-model.md §3.5](../architecture/snapshot-model.md) specifies the schema-migration story: every change ships forward (`up`) and reverse (`down`) migrations; missing reverses fail rollback loudly rather than silently. This is the right discipline but it papers over a real product limit.

For schemas whose forward migration is **non-destructive** (add column with default, add table, add index, widen a type), rollback is genuinely atomic: reverse-migrate, restore memory, done.

For schemas whose forward migration is **destructive** (drop column with data, transform-and-replace, narrow a type, merge rows), rollback restores from the last snapshot taken before the migration. **Data written between that snapshot and the rollback moment is lost.** No coordinator design can change this; the data has been transformed in ways that cannot be inverted from the post-migration state alone.

This is the limit every "atomic rollback" demo in the industry hand-waves over. We name it explicitly in design-partner conversations:

> Rollback is atomic for non-destructive migrations. For destructive migrations, rollback restores from the most recent snapshot taken before the migration, meaning agent activity between that snapshot and the rollback is lost. If your agent's memory schema is mostly append-only, this is almost never an issue in practice. If your agent regularly destructively migrates its own memory, we should talk about snapshot frequency and what the realistic recovery point looks like for your workload.

This wording belongs in design-partner pilot agreements and onboarding docs, not in marketing copy.

## Decision 6 — Fully post-Git substrate remains the v2+ option

We do not foreclose a future move to a fully post-Git Merkle DAG. The mechanism that keeps that door open is **the platform API contract being storage-independent**. Platforms produce Commit objects; the Commit object schema is stable; the storage layer can swap from "Git for code + object store for blobs + coordinator" to "single Merkle DAG with Git compat shim" without breaking platform integrations.

Two motivations could force the swap later:

- **Per-hunk attestation.** Multi-agent collaboration where Cursor wrote half of a commit and Cognition's agent wrote the other half eventually wants attestation finer than per-commit. Per-hunk attestation is awkward in Git (notes are per-commit, refs are per-commit) and natural in a Merkle DAG (a tree of hunks with per-node metadata).
- **Multi-agent commit graphs.** Workflows where multiple agents fork from a shared state, each take an action, and the system merges results, are easier to model as content-addressed DAGs than as Git branches with linear histories.

Neither is required for MVP. The discipline that preserves the option is: **never expose Git ref names, object store paths, or storage-layer concepts to platform integrators.** They produce and consume Commit objects; everything else is implementation detail.

## Prior art

Two systems are worth studying before finalizing the code-layer interface in `agentic-core/store.rs`:

- **Sapling (Meta)** — exposes Git wire-protocol compatibility on top of a content-addressed substrate. Their compat shim handles ref translation, partial clones, and push semantics in ways directly applicable to Approach C.
- **Jujutsu (JJ)** — the same general pattern with a different working-copy model. Strong precedent for "post-Git data model, Git wire-protocol compatibility."

Neither is suitable to adopt as-is — both target human-authored workflows and neither encodes our tuple — but reading their internals saves us from re-inventing several edge cases (concurrent push, ref translation under rename, partial clone semantics).

## Relationship to ADR-0001

ADR-0001 Decision 5 ("Don't replace Git for code") is preserved in spirit and refined in detail. The original intent was to delegate code-VCS semantics to Git rather than rebuild them, and the offhand framing "We don't compete with GitHub at hosting Git" reflected the strategic posture as of that ADR. This ADR adjusts the strategic posture: we *do* host Git repos, as part of being **the git host built for when most commits are written by agents**. *Hosting* Git and *replacing Git as a VCS* are distinct. Code-VCS semantics remain Git's; the hosting layer plus the manifest layer plus the coordinator are ours.

ADR-0001 Decision 9 ("CLI-first; no web UI in MVP") is preserved. The agent-PR review primitive — the UI rendering of the extended Commit object — is a v1.1 product surface that sits on top of the substrate this ADR specifies. The MVP can produce and consume Commit objects via CLI and SDK alone; the UI is a distribution lever for later.

ADR-0001 Decision 2 (alternatives, on Git itself as the object store) is fully consistent: we already rejected Git itself as the blob store because it's bad at large opaque blobs and streaming snapshots. This ADR formalizes the corollary — we use Git for what it's good at (code, replication wire protocol) and our own content-addressed store for everything else, with the coordinator gluing them atomically.

## Consequences

**Positive**

- Integration cost for agent platforms remains "an afternoon's work" — they produce Commit objects with the new fields populated; everything else is downstream of that.
- The 2PC staging order is documented and enforced in the core, turning atomic rollback from aspiration into honest engineering.
- The data model is structurally differentiated from GitHub's substrate. The manifest extension is not bolt-on-able; GitHub would need to introduce a new first-class object that breaks their commit-and-PR model to match it.
- The path to a fully post-Git substrate in v2+ is preserved without committing us to it now.

**Negative**

- The coordinator (`agenticd`) is a single point of complexity. Bugs in the staging order have catastrophic blast radius and require disciplined testing — including failure-injection tests at each staging boundary.
- "Filesystem snapshot-capable storage required for production" raises the ops bar for self-hosted deployments. Acceptable for design partners; a friction point for broader adoption that we'll need to address with prescriptive deployment guides.
- The honesty item on destructive migrations is a genuine product limit, not a documentation choice. Some prospective customers' workloads will not fit, and we will lose those deals. That's correct positioning, not a problem to solve.

**Risks to revisit**

- Per-hunk attestation pressure could come earlier than v2+ if multi-platform commits become common quickly among design partners. Track requests; open a follow-up ADR if more than one design partner asks within a quarter.
- Git push as the single commit point assumes the Git remote is reliable. For self-hosted deployments running their own Gitaly, this is fine. For SaaS later, we own that reliability budget directly.
- Sapling / JJ might evolve to a point where adopting one of them outright is cheaper than maintaining our own coordinator. Worth a quarterly review.

## Action items

1. [ ] Update `crates/agentic-core/src/object.rs` `Commit` type to include `intent`, `plan`, `transcript`, `evals`, `cost_cents`, and `signatures` (Decision 2).
2. [ ] Create `crates/agentic-core/src/commit.rs` with the 2PC staging order implemented and an explanatory header comment block (Decision 3).
3. [ ] Add failure-injection tests at each of the five staging steps in `commit.rs` to verify orphan-blob, idempotent-retry, and orphan-ref recovery (Decision 3).
4. [ ] Add filesystem-snapshot-capability detection to `agentic init` with a warning when running on non-capable storage (Decision 4).
5. [ ] Add the "destructive migration limit" wording to the design-partner pilot agreement template (Decision 5).
6. [ ] Read Sapling and JJ source for ref-translation and push-semantics patterns before finalizing `crates/agentic-core/src/store.rs` Git-layer interface.
7. [ ] Specify the standardized `evals` blob schema in a follow-up doc (`docs/architecture/evals-schema.md`); coordinate with first two design-partner platforms on the field set.
8. [ ] Open a follow-up ADR (0003) on per-hunk attestation if and when the first design partner asks.

See [snapshot-model.md](../architecture/snapshot-model.md) for the underlying object model, [overview.md](../architecture/overview.md) for the system view, and [ADR-0001](./0001-architecture-foundations.md) for the foundational decisions.
