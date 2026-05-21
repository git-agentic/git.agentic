# ADR-0006: ObjectStore Backend Trait — Formalising the v1.1 Storage-Layer Swap

**Status:** Proposed
**Date:** 2026-05-21
**Deciders:** Toni
**Extends:** [ADR-0002](./0002-substrate-and-supercommit.md) Decision 6 (post-Git substrate as v2+ option behind a stable platform API contract)
**Relates to:** [ADR-0004](./0004-realtime-agenticd-for-executor.md) (GCS-backed `ObjectStore` pulled forward to v1.0 for the Executor sidecar)

## Context

[ADR-0002 Decision 6](./0002-substrate-and-supercommit.md) committed to making the storage layer swappable behind a stable platform API contract — platforms produce and consume `Commit` objects; "never expose Git ref names, object store paths, or storage-layer concepts to platform integrators." The implementation seam for that decision is the `ObjectStore` trait in [`crates/agentic-core/src/store.rs`](../../crates/agentic-core/src/store.rs):

```rust
pub trait ObjectStore: Send + Sync {
    fn put(&self, object: &Object) -> Result<Hash>;
    fn put_raw(&self, kind: ObjectKind, bytes: &[u8]) -> Result<Hash>;
    fn get(&self, hash: &Hash) -> Result<Object>;
    fn get_raw(&self, hash: &Hash) -> Result<Vec<u8>>;
    fn has(&self, hash: &Hash) -> bool;
}
```

Two implementations exist on `main` today: `FsObjectStore` (the MVP local-filesystem backend) and `GcsObjectStore` (pulled forward to v1.0 by ADR-0004 to satisfy the Executor sidecar's GCS write-through requirement). The trait header still describes remote backends as "v1.1 plug-in territory." With v1.0 ship in sight (2026-05-26, pulled forward from the originally-planned 2026-08-11) and an external infra category emerging — Pierre Computer Company's Code.Storage being one example of a managed Git-infra layer with programmatic commits, ephemeral branches, and warm/cold tiering — we need to:

1. **Decide what stays in the trait and what stays out.** The current surface (put / put_raw / get / get_raw / has) was designed against `FsObjectStore`. `GcsObjectStore` already strains it (no streaming, no batched writes, no lifecycle policy hooks). A managed-Git backend like Code.Storage strains it differently — it owns its own ref model, expects commits constructed via its SDK rather than blobs uploaded then pushed, and has its own auth/tiering primitives.
2. **State the v1.1 backend matrix explicitly.** Which backends do we commit to supporting, which are "behind the trait if someone writes the impl," and which are explicit non-goals.
3. **Re-affirm the no-leak rule from ADR-0002 Decision 6.** This is where it can erode quietly: a feature added to expose Code.Storage's ephemeral branches all the way to the SDK would re-introduce a storage-layer concept the platform integration contract has to know about. ADR-0007 handles the "ephemeral branches as a primitive" question on its own terms; this ADR keeps the storage trait clean.

This ADR does NOT adopt any specific managed-Git backend for v1.0 — that would conflict with [ADR-0001](./0001-architecture-foundations.md) Decision 9 (self-hosted Docker compose) and the broken-prompt demo's "`git clone` + `docker-compose up` in <5 min on a fresh machine" constraint. It locks in the trait shape that lets v1.1 add managed backends without re-litigating ADR-0002.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **`ObjectStore` trait stays the v1.1 backend seam.** Self-hosted FS remains the default; GCS is the production reference impl; S3 and Azure Blob land in v1.1; managed-Git backends (Code.Storage class) plug in behind the same trait but require an adapter layer. | One seam, multiple impls. Don't fragment the API contract by backend. |
| 2 | **Extend the trait minimally for v1.1: `delete`, `list_prefix`, and an async variant.** Required by GC and by remote backends that batch over network round-trips. | Today's blocking-only surface is unworkable for remote backends at production scale. |
| 3 | **Managed-Git backends are wrapped, not adopted.** A `ManagedGitStore` adapter implements `ObjectStore` by translating put/get/has into the backend's SDK calls. The adapter is the only place that knows about repo IDs, JWT signing, or vendor-specific ref models. | Keeps `Commit`-object-as-platform-API-contract honest (ADR-0002 Decision 6); avoids polluting downstream code with backend-specific concepts. |
| 4 | **Self-hosted remains the default and the demo path.** No managed-backend dependency is allowed on the broken-prompt demo path. Managed backends are opt-in for v1.1 production deployments. | The demo discipline (`docs/product/demo-scenario.md`) requires offline / air-gapped capability. |
| 5 | **Backend selection is `agenticd` config, not SDK API.** The SDK and CLI take `Commit` inputs and ref names; they never see backend identity. | If the SDK had to know about backends, ADR-0002 Decision 6 would have already failed. |

---

## Decision 1 — The trait as the v1.1 backend seam

We commit to the following backend matrix:

| Backend | Status | v1.1 milestone | Notes |
|---|---|---|---|
| `FsObjectStore` | Shipped (v1.0) | n/a | Default. Required for the broken-prompt demo. |
| `GcsObjectStore` | Shipped (v1.0) | n/a | Production reference impl. Pulled forward by ADR-0004 for the Executor sidecar. |
| `S3ObjectStore` | Behind the trait | v1.1-α | Same `Bucket`/`Object` shape as GCS; ~1 week of port + tests. |
| `AzureBlobObjectStore` | Behind the trait | v1.1-β | Add after S3 if a design partner pulls; speculative otherwise. |
| `ManagedGitStore` (Code.Storage and class) | Behind an adapter | v1.1-γ (opt-in) | Adapter required; see Decision 3. Treated as one candidate among several. |
| In-process / fully post-Git Merkle DAG | v2+ | n/a | Per ADR-0002 Decision 6 — preserved option, not v1.1 work. |

Rationale for the cluster:

- **S3 next**: GCS and S3 have nearly isomorphic surfaces. Adding S3 unlocks AWS-resident design partners and is the lowest-risk v1.1 storage win.
- **Azure speculative**: we have no design partner asking. Land it only if one does.
- **Managed-Git "γ" tier**: Pierre's Code.Storage and any equivalent that emerges are interesting because they solve adjacent problems (warm/cold tiering, ephemeral branches as a first-class primitive, GitHub bidirectional sync) — but adopting one for the core object store would conflict with ADR-0001 Decision 9. They go behind the same trait as opt-in production backends, *and* require the adapter layer in Decision 3.

We do not commit to shipping more than one managed-Git adapter in v1.1. If multiple emerge as commercially viable backends for design partners, that's a follow-up ADR's problem.

## Decision 2 — Minimal v1.1 surface extension

The MVP trait was designed for `FsObjectStore`. It is missing three capabilities that remote and managed backends need:

```rust
pub trait ObjectStore: Send + Sync {
    // Existing v1.0 surface (unchanged):
    fn put(&self, object: &Object) -> Result<Hash>;
    fn put_raw(&self, kind: ObjectKind, bytes: &[u8]) -> Result<Hash>;
    fn get(&self, hash: &Hash) -> Result<Object>;
    fn get_raw(&self, hash: &Hash) -> Result<Vec<u8>>;
    fn has(&self, hash: &Hash) -> bool;

    // New in v1.1:
    fn delete(&self, hash: &Hash) -> Result<()>;
    fn list_prefix(&self, shard_prefix: &str) -> Result<Vec<Hash>>;
}
```

`delete` is required by GC, which we cannot defer forever (MVP "never GC" per [`mvp-spec.md`](../product/mvp-spec.md) Q4 is fine for design-partner pilots and untenable past that). `list_prefix` is required for orphan detection (objects written by failed step-1-through-3 of the 2PC staging order per [ADR-0002 Decision 3](./0002-substrate-and-supercommit.md)) and for any operation that needs to enumerate the store without a manifest in hand.

We add an async variant in v1.1:

```rust
#[async_trait]
pub trait AsyncObjectStore: Send + Sync {
    async fn put(&self, object: &Object) -> Result<Hash>;
    // ... mirrors the blocking surface
}
```

The blocking trait stays. `FsObjectStore` stays blocking; `GcsObjectStore` already wraps tokio under a blocking facade and exposes an async-native impl via the new trait for callers that want to pipeline. The daemon picks per backend at config time.

What we deliberately do NOT add to the trait:

- Streaming put/get. Backends that need it (large segment manifests against GCS) expose it as a backend-specific method; daemon code that needs streaming asks for the concrete type via a downcast helper. Adding streaming generically forces every backend to implement it; we'd rather absorb a downcast for the one or two callers.
- Backend-native ref operations. See Decision 3.
- Auth/lifecycle/quota hooks. These are construction-time config, not runtime trait methods.

## Decision 3 — Managed-Git backends are wrapped, not adopted

Managed-Git providers like Code.Storage expose programmatic-commit APIs that *look* close to what `agenticd` already does. The temptation is to lean on their commit-creation primitive directly. We don't, for three reasons:

1. **Their commit model is code-only.** Code.Storage's `createCommit` writes files and emits a Git commit SHA. Our `Commit` object captures six tuple dimensions plus the ADR-0002 platform-API extensions (`intent`, `plan`, `transcript`, `evals`, `cost_cents`, `signatures`). The two object models do not nest cleanly — adopting their `createCommit` as our "commit primitive" would either truncate our Commit or pile our extended fields into Git notes the way [ADR-0002 Option A](./0002-substrate-and-supercommit.md) was rejected for.
2. **Their ref model is theirs.** Repo IDs, ephemeral branches, sandboxes — all backend-specific names. Surfacing them through our trait would break the no-leak rule from ADR-0002 Decision 6.
3. **Hard SaaS dependency on the substrate path is incompatible with our demo discipline.** The broken-prompt demo must run from `git clone` + `docker-compose up` on a fresh laptop in under 5 minutes. We do not add an external auth dance and a network dependency to the substrate of the canonical demo.

The adapter pattern:

```rust
pub struct ManagedGitStore<C: ManagedGitClient> {
    client: C,
    repo_id: String,
    blob_cache: BlobCache,
}

impl<C: ManagedGitClient> ObjectStore for ManagedGitStore<C> {
    fn put(&self, object: &Object) -> Result<Hash> {
        let bytes = serde_json::to_vec(object)?;
        let hash = object.hash();
        // Mirror the FsObjectStore content-address layout inside the
        // managed-Git repo: .agentic/objects/<ab>/<62-hex>.zst
        let path = format!(".agentic/objects/{}/{}.zst",
            &hash.to_hex()[..2], &hash.to_hex()[2..]);
        let compressed = zstd::stream::encode_all(&bytes[..], 3)?;
        self.client.put_blob(&self.repo_id, &path, &compressed)?;
        Ok(hash)
    }
    // ... get / has / etc. mirror the same path scheme
}

pub trait ManagedGitClient: Send + Sync {
    fn put_blob(&self, repo: &str, path: &str, bytes: &[u8]) -> Result<()>;
    fn get_blob(&self, repo: &str, path: &str) -> Result<Vec<u8>>;
    fn head_blob(&self, repo: &str, path: &str) -> Result<bool>;
}
```

The adapter treats the managed-Git provider as a content-addressed blob store with the same `<ab>/<62-hex>.zst` layout `FsObjectStore` uses. Their "commits" and "branches" are not touched — our refs continue to live in our own ref model (Decision 5 below). What we get from the managed provider is durable blob storage with their tiering and access semantics; what we do NOT get is their commit / branch / ephemeral-branch / sandbox primitives leaking into our trait.

Anything more native — emitting actual Git commits via Code.Storage's `createCommit`, using their ephemeral branches as our ephemeral-run primitive (see ADR-0007), pushing through their GitHub bidirectional sync — is a separate per-backend integration feature, NOT a trait extension. It lives on the adapter type and is exposed via downcast or via a side-channel CLI subcommand. The `ObjectStore` trait stays clean.

## Decision 4 — Self-hosted is the default and the demo path

[`docs/product/demo-scenario.md`](../product/demo-scenario.md) requires the broken-prompt demo to run with no external dependencies beyond what `docker-compose up` brings up. Managed backends are an opt-in production-tier configuration, not part of the default path. Specifically:

- `examples/langgraph-rollback/docker-compose.yml` MUST continue to compose only Postgres + pgvector + agenticd locally, with `FsObjectStore` as the backend.
- A managed backend is selected only by explicit config (`AGENTICD_OBJECT_STORE=managed-git`, with provider-specific env vars) and only documented in v1.1's production deployment guide.
- CI exercises `FsObjectStore` and `GcsObjectStore` (against `fsouza/fake-gcs-server` per the sprint Week A close-out). Managed-backend CI is opt-in nightly, against vendor sandboxes where available.

This is the same discipline ADR-0004 used to bring GCS forward: GCS is real in v1.0 because the Executor sidecar genuinely needs it, but the LangGraph broken-prompt demo continues to run against `FsObjectStore`.

## Decision 5 — Backend selection lives in `agenticd` config

The SDK does not know about backends. The CLI does not know about backends. Both speak `Commit` objects and ref names ([ADR-0002 Decision 2 and 6](./0002-substrate-and-supercommit.md)). Backend identity, credentials, tiering policy, and adapter wiring are `agenticd` configuration — read at startup, immutable for the lifetime of the daemon process.

```toml
# .agentic/config.toml — v1.1
[object_store]
backend = "fs"         # or "gcs", "s3", "managed-git"
root    = ".agentic/objects"

[object_store.gcs]
bucket = "agentic-prod-eu"
# auth via ambient GOOGLE_APPLICATION_CREDENTIALS

[object_store.managed_git]
provider = "code.storage"
repo_id  = "my-org/my-agent"
# auth via AGENTIC_MANAGED_GIT_PRIVATE_KEY env var
```

This is the same shape MVP already uses for `[memory.postgres]`. Adding backends does not change SDK or CLI surfaces, which is the point.

## Consequences

**Positive**

- v1.1 storage work has a single seam (the `ObjectStore` trait). New backends are weeks of focused work, not multi-week architectural threads.
- The platform-API-contract discipline from ADR-0002 Decision 6 is preserved against the specific temptation managed-Git providers create. Their primitives are interesting but their primitives are not our primitives.
- The broken-prompt demo stays portable. A design partner with no GCS account, no AWS account, and no Code.Storage account can still run the demo.
- The path to a fully post-Git Merkle DAG in v2+ is unaffected — that's a swap of backend behind the same trait, not a swap of API contract.

**Negative**

- Two trait additions (`delete`, `list_prefix`) plus the async variant are a non-trivial v1.1 work item before any new backend can ship. Adding them in v1.0 is tempting but conflicts with the hardening sprint focus; we hold the line.
- The managed-Git adapter pattern is real engineering, not glue. The first one (whichever backend wins the design-partner ask first) is realistically two weeks of work to get production-quality, including auth, error mapping, and integration tests against the vendor sandbox.
- The downcast escape hatch for backend-specific features (streaming, ephemeral branches, lifecycle hooks) is a discipline tax. Future contributors will be tempted to widen the trait instead. The ADR exists in part to make that argument explicit and to require a follow-up ADR to widen it.

**Risks to revisit**

- If two or more design partners ask for the same managed-Git provider, we may want a deeper integration than the adapter affords. Open a follow-up ADR rather than widening this one.
- If the GCS-backed ObjectStore turns out to need streaming put/get for production-scale segment manifests (the sprint Week A close-out work has not stressed this), revisit Decision 2's "no streaming in the trait" exclusion.
- If managed-Git backends evolve to expose multi-tuple-dimension primitives (e.g. a commit object with arbitrary metadata fields, not just code), the temptation to lean on that vs. our own Commit object grows. The discipline is documented in ADR-0002 Decision 6; this ADR reinforces it.

## Prior art

- **Pierre Computer Company's Code.Storage** ([docs](https://code.storage/docs/getting-started/introduction)) — managed Git-as-a-service with SDK-based programmatic commits, ephemeral branches, sandboxes, warm/cold tiering, and bidirectional GitHub sync. Useful as a candidate behind the adapter pattern in Decision 3, and as design-reference for the ephemeral-branches work in [ADR-0007](./0007-ephemeral-branches-agent-run-primitive.md). Not adopted for v1.0 per [ADR-0001 Decision 9](./0001-architecture-foundations.md) and the demo discipline in Decision 4 above.
- **Sapling and JJ** (already cited in [ADR-0002](./0002-substrate-and-supercommit.md) §"Prior art") — relevant when v2+ moves to a post-Git Merkle DAG behind the same trait.

## Action items

1. [ ] Land `delete` and `list_prefix` on the `ObjectStore` trait in `crates/agentic-core/src/store.rs`. Implement on `FsObjectStore` and `GcsObjectStore`. v1.1 milestone.
2. [ ] Add `AsyncObjectStore` trait with the async-native shape; wire `GcsObjectStore` to expose it natively. v1.1 milestone.
3. [ ] Write the `ManagedGitStore` adapter and a `ManagedGitClient` trait. First impl chosen by design-partner ask (track in `docs/product/v1.1-plan.md`). v1.1 milestone.
4. [ ] Update `agenticd` config schema to support per-backend sections; document selection rules. v1.1 milestone.
5. [ ] Add nightly opt-in CI job for managed-backend smoke tests once an adapter ships. v1.1 milestone.
6. [ ] Add a downcast helper (`fn as_streaming(&self) -> Option<&dyn StreamingObjectStore>`) for the streaming escape hatch in Decision 2's exclusion list. Only if a v1.1 caller actually needs it.

See [ADR-0001](./0001-architecture-foundations.md), [ADR-0002](./0002-substrate-and-supercommit.md), [ADR-0007](./0007-ephemeral-branches-agent-run-primitive.md), and [`docs/product/v1.1-plan.md`](../product/v1.1-plan.md).
