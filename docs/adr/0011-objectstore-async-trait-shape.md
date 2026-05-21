# ADR-0011: `ObjectStore` Async-Trait Shape

**Status:** Proposed
**Date:** 2026-05-21
**Deciders:** Toni
**Extends:** [ADR-0006](./0006-objectstore-backend-trait.md) Decision 2 (extend the trait minimally for v1.1: `delete`, `list_prefix`, and an async variant)
**Relates to:** [`docs/ops/2026-05-21-agenticd-architectural-analysis.md`](../ops/2026-05-21-agenticd-architectural-analysis.md) §A5 / §C1 / §B2 / §B3 / §R3 (the recommendations and risk this ADR unblocks at the trait level; the tactical [A5](../ops/2026-05-21-agenticd-architectural-analysis.md#a5) `spawn_blocking` patch lands without waiting on this ADR)

## Context

[ADR-0006](./0006-objectstore-backend-trait.md) Decision 2 committed v1.1 to an async variant of the `ObjectStore` trait, with `delete` and `list_prefix` as the other two additions required for GC and remote backends. It did not pin down the *shape* of the async variant — which crate, what `Send` bounds, how the existing `FsObjectStore` and `GcsObjectStore` impls migrate, or how the daemon's `LocalSet` execution model interacts with `Send`-bounded futures.

The 2026-05-21 architectural analysis surfaced exactly why that shape matters:

- **C1 / B2 / R3** — `GcsObjectStore` uses `reqwest::blocking::Client` because the trait is sync (`gcs_store.rs:83–86`, `store.rs:50`). On the daemon's single-threaded `LocalSet` (`main.rs:129–141`), a blocking HTTP call freezes every connection task on the thread for up to the 30-second `REQUEST_TIMEOUT`, including read-path requests (`ReadObject`, `Log`, `Diff`) that don't hold `commit_lock`. The freeze is not a daemon bug — it's a direct consequence of the sync-trait + LocalSet pairing.
- **B3** — Because the put path holds the memory mutex across the blocking call, the freeze window extends through database advisory-lock scope, compounding the impact.

The tactical fix is [A5](../ops/2026-05-21-agenticd-architectural-analysis.md#a5): wrap the existing blocking calls in `tokio::task::spawn_blocking` inside `gcs_store.rs`. That patch is shippable today, requires no trait change, and addresses the demo-day risk for any v1.0 deployment that uses GCS. It is **not** what this ADR is for.

This ADR is for the **v1.1 trait redesign**: how the `ObjectStore` trait itself becomes async without compromising the swappability commitment in [ADR-0002](./0002-substrate-and-supercommit.md) Decision 6 (storage layer must stay swappable), the daemon-as-sidecar shape in [ADR-0004](./0004-realtime-agenticd-for-executor.md), or the managed-Git adapter pattern in [ADR-0006](./0006-objectstore-backend-trait.md) Decision 3.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **Use Rust's native `async fn in trait`** (stable since Rust 1.75) rather than the `async-trait` crate or `Box<dyn Future>` boilerplate. | Stable language feature, no dyn-compat penalty, no extra dependency. The features the `async-trait` crate adds (object-safety, type-erasure) we get explicitly via a small `dyn`-safe wrapper trait. |
| 2 | **The trait splits into `ObjectStore` (object-safe, type-erased)** wrapping a concrete async impl, and `AsyncObjectStore: Send + Sync` (the native-async surface for impls). The daemon and SDK consume `Arc<dyn ObjectStore>` exactly as today. | `async fn in trait` is not yet dyn-compatible. The wrapper trait gives us object safety; the native surface gives implementers the cleanest write experience. |
| 3 | **All `ObjectStore` futures are `Send`.** The `LocalSet`-vs-`spawn` distinction is settled at the daemon level (per ADR-0004 Decision 1 sidecar topology), not at the trait. Implementers that need non-Send work (e.g. a hypothetical `IoUringObjectStore` in v2+) get their own non-Send trait. | A single Send-bounded trait is the simplest contract; the LocalSet specialisation that exists today is a daemon implementation choice, not a trait property. |
| 4 | **Method set: `put`, `put_raw`, `get`, `get_raw`, `has`, `delete`, `list_prefix`** — five existing + two from ADR-0006 D2. No streaming variants in v1.1. | Streaming `put_stream` / `get_stream` is a v2+ concern (large blobs > 100 MiB). v1.0/v1.1 commit shapes don't approach that size. |
| 5 | **`ObjectKind` parameter on `put_raw` is removed** ([B5 from the audit](../ops/2026-05-21-agenticd-architectural-analysis.md#b5)). It was silently discarded by both implementations; content addressing makes it irrelevant. | A parameter with no runtime effect is a lie. Drop it in the v1.1 trait. |
| 6 | **Existing `FsObjectStore` and `GcsObjectStore` migrate by writing native-async impls.** `FsObjectStore` uses `tokio::fs`; `GcsObjectStore` switches from `reqwest::blocking` to `reqwest`'s default async client. The tactical `spawn_blocking` patch from [A5](../ops/2026-05-21-agenticd-architectural-analysis.md#a5) is removed at this point. | One v1.1 release migrates the whole impl matrix; no per-backend deprecation window. |

## Decision details

### Decision 1 — Native `async fn in trait`

```rust
// crates/agentic-core/src/store.rs

pub trait AsyncObjectStore: Send + Sync {
    async fn put(&self, object: &Object) -> Result<Hash>;
    async fn put_raw(&self, bytes: &[u8]) -> Result<Hash>;
    async fn get(&self, hash: &Hash) -> Result<Object>;
    async fn get_raw(&self, hash: &Hash) -> Result<Vec<u8>>;
    async fn has(&self, hash: &Hash) -> Result<bool>;
    async fn delete(&self, hash: &Hash) -> Result<()>;
    async fn list_prefix(&self, prefix: &str) -> Result<Vec<Hash>>;
}
```

Rust 1.75+ supports `async fn` in trait definitions natively. The auto-generated future for each method captures `&self` and the parameters; the compiler computes its concrete type per impl. No `Box<dyn Future>` allocation per call, no `async_trait` macro boilerplate, no extra dependency.

The current `rust-toolchain.toml` pins `1.95.0` per [CLAUDE.md](../../CLAUDE.md) "Build, test, run". We're already past the stabilisation point.

### Decision 2 — Object-safe wrapper + native surface split

`async fn in trait` is not yet dyn-compatible (object-safe) as of Rust 1.95. The standard fix is a thin wrapper:

```rust
// Object-safe surface the daemon and SDK consume.
pub trait ObjectStore: Send + Sync {
    fn put<'a>(&'a self, object: &'a Object) -> BoxFuture<'a, Result<Hash>>;
    fn put_raw<'a>(&'a self, bytes: &'a [u8]) -> BoxFuture<'a, Result<Hash>>;
    fn get<'a>(&'a self, hash: &'a Hash) -> BoxFuture<'a, Result<Object>>;
    fn get_raw<'a>(&'a self, hash: &'a Hash) -> BoxFuture<'a, Result<Vec<u8>>>;
    fn has<'a>(&'a self, hash: &'a Hash) -> BoxFuture<'a, Result<bool>>;
    fn delete<'a>(&'a self, hash: &'a Hash) -> BoxFuture<'a, Result<()>>;
    fn list_prefix<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<Hash>>>;
}

// Blanket impl: anything that's AsyncObjectStore is automatically ObjectStore.
impl<T: AsyncObjectStore + ?Sized> ObjectStore for T {
    fn put<'a>(&'a self, object: &'a Object) -> BoxFuture<'a, Result<Hash>> {
        Box::pin(AsyncObjectStore::put(self, object))
    }
    // ... one method per pair, mechanical.
}
```

Implementers write `impl AsyncObjectStore for FsObjectStore { ... }` with native `async fn` and inherit `ObjectStore` for free via the blanket. Consumers (daemon, SDK) take `Arc<dyn ObjectStore>` exactly as today (`server.rs:35`). The double-trait shape is a known idiom; when `async fn in trait` becomes dyn-safe in a future Rust release, the wrapper is removed and the blanket impl deleted, with no API change for either side.

If `async-trait` ever ships a stable `#[async_trait(dyn_safe)]` variant or the language adds an attribute, this ADR is reopened to pick the cleanest of the three.

### Decision 3 — All futures `Send`

The daemon's LocalSet (`main.rs:129–141`) is single-threaded by deliberate design — it avoids sqlx 0.7's HRTB issues per the comment in `main.rs`. Inside the LocalSet, futures don't *need* to be Send. But:

- The audit (C1 / B2) is precisely about the cost of running blocking work on the LocalSet. The v1.1 fix is to offload to `spawn_blocking` (for unavoidable sync work) or run native async on a multi-threaded runtime.
- ADR-0004 Decision 1's sidecar topology has `containerConcurrency=1`, so the daemon could theoretically be `current_thread` everywhere. But the trait shouldn't bake that in — a future deployment that wants multi-threaded executor work shouldn't have to redesign the trait.
- Send-bounded futures cost nothing for impls whose state is already Send (`FsObjectStore` holds a `PathBuf`; `GcsObjectStore` holds a `reqwest::Client` which is Send). They cost real ergonomics for impls that want to hold `Rc` or other non-Send state, but no current or planned impl does.

Therefore: `AsyncObjectStore: Send + Sync`. Implementers that genuinely need non-Send work (a hypothetical `IoUringObjectStore` in v2+, an in-memory `LocalFsCache`) get their own trait, not a relaxation of this one.

### Decision 4 — Method set frozen at seven

```
put           — write a typed Object, return its hash
put_raw       — write raw bytes, return the hash
get           — read by hash, deserialise to Object
get_raw       — read by hash, return raw bytes
has           — does this hash exist
delete        — remove (for GC)
list_prefix   — enumerate (for GC, for adapter implementations)
```

No streaming `put_stream` / `get_stream`. Object sizes in v1.0/v1.1 are bounded by:
- The 16 MiB framing limit on the wire (per [ADR-0010](./0010-wire-protocol-error-model.md) — the *Envelope* limit, not a blob limit).
- The 10 MiB per-blob limit currently enforced by the daemon (`ReadObject` 10 MiB guard).

Streaming becomes relevant when blob sizes exceed ~100 MiB (think segmented memory snapshots of multi-GB pgvector tables, or compiled-model artefacts as commit content). When v2+ surfaces that workload, a `StreamingObjectStore: AsyncObjectStore` extension trait can add `put_stream` / `get_stream` without breaking the seven-method core. Not in v1.1.

No `put_many` / `get_many` batch variants either. Backends that benefit from batching (S3 multipart, GCS batch endpoints) handle batching internally per call; the trait stays single-object.

### Decision 5 — Drop `ObjectKind` from `put_raw`

```rust
// Before (v0):
async fn put_raw(&self, kind: ObjectKind, bytes: &[u8]) -> Result<Hash>;
// After (v1.1):
async fn put_raw(&self, bytes: &[u8]) -> Result<Hash>;
```

Both existing impls used `_kind` (silently discarded — [B5](../ops/2026-05-21-agenticd-architectural-analysis.md#b5)). Content addressing makes the kind irrelevant: `get_raw` returns the bytes; the caller deserialises the kind from the content itself.

If a future backend wants to optimise by kind (e.g., separate hot/cold tiering for `Commit` vs `Segment`), it can re-introduce a kind hint, but as a separate optional method (`put_raw_with_hint`), not by reviving a parameter the implementation can ignore.

### Decision 6 — Single-release migration for existing impls

Both existing implementations migrate in one v1.1 release:

**`FsObjectStore` (`agentic-core/src/store.rs`):**
- Replaces `std::fs::write` / `std::fs::read` with `tokio::fs::write` / `tokio::fs::read`. The atomic-rename pattern (`store.rs:54–58`) becomes `tokio::fs::rename`. The content-addressed TOCTOU ([C7](../ops/2026-05-21-agenticd-architectural-analysis.md#c7)) remains benign.

**`GcsObjectStore` (`agentic-core/src/gcs_store.rs`):**
- Removes `reqwest::blocking::Client`; uses `reqwest::Client` (default async).
- The `spawn_blocking` patch from the tactical [A5](../ops/2026-05-21-agenticd-architectural-analysis.md#a5) work is deleted; the async client makes it unnecessary.
- The "Threading" module doc (`gcs_store.rs:30–37`) is rewritten to drop the LocalSet caveat.

No `#[deprecated]` markers, no parallel sync trait kept around. The sync trait is replaced wholesale — internal API, two implementers, both migrating together.

## What does not change

- The on-disk layout (`agentic-core/src/store.rs` `path_for` scheme: `.agentic/objects/<ab>/<rest>.zst`) is unchanged. Wire-format compatibility for objects already in stores.
- `Object` / `Commit` / `Tree` / `Blob` schemas are unchanged.
- `ObjectStoreSpec` parser surface (`agenticd/src/objstore.rs`) is unchanged — the URL grammar (`fs:`, `gcs://bucket/prefix`) stays the same; only the trait the parser returns differs.
- The relationship between [ADR-0006](./0006-objectstore-backend-trait.md) Decisions 3 (managed-Git wrapped not adopted) and 4 (self-hosted is default + demo path) is unchanged. The `ManagedGitStore` adapter migrates to the same trait shape.
- The daemon's `LocalSet`-on-a-single-thread execution model is unchanged in v1.1. The trait being async means the *runtime* gets to choose how to schedule the futures; the daemon continues to do so via `LocalSet`. The change is that now native-async backends don't *force* a thread freeze.

## Consequences

**Positive:**

- The C1 / B2 / R3 LocalSet-freeze risk vanishes for native-async backends. Native async work yields between I/O points; other connections continue serving.
- The `delete` and `list_prefix` methods unblock the GC work in v1.1 hardening (per [`docs/product/v1.1-plan.md`](../product/v1.1-plan.md) W3).
- `Send`-bounded futures keep the door open for a future multi-threaded daemon mode without trait surgery.
- Dropping `ObjectKind` from `put_raw` removes a lie from the API.

**Negative:**

- The double-trait shape (object-safe `ObjectStore` + native-async `AsyncObjectStore`) is a known but real complexity. New backend authors have to understand which one to implement (answer: always `AsyncObjectStore`).
- One release migrates all impls; there's no per-backend opt-in window. For a project with two impls this is the right tradeoff; if the impl count grew to ten before v1.1 ships, this ADR would need to grow a deprecation period.
- The `Send`-bound forecloses some future impl shapes (Rc-holding, single-threaded-only backends). The decision section above argues this is acceptable; v2+ may revisit.

**Risks to revisit:**

- If Rust stabilises dyn-safe `async fn in trait` before v1.1 ships, the wrapper trait collapses into `ObjectStore` alone. Land the simpler shape if that happens; this ADR's decisions other than 2 are unaffected.
- If the GC story in v1.1 W3 reveals that `list_prefix` is insufficient (e.g., it needs filtering by object kind or tiered store hints), add the methods as a `GarbageCollectableObjectStore: AsyncObjectStore` extension trait rather than growing the core. Don't bloat the seven-method base.

## Implementation plan

1. **`agentic-core/src/store.rs`** — define both traits, blanket-impl `ObjectStore` for any `AsyncObjectStore`. Move `FsObjectStore` to the new trait, using `tokio::fs`.
2. **`agentic-core/src/gcs_store.rs`** — rewrite `GcsObjectStore` against `reqwest::Client` (async). Remove the `reqwest::blocking` dependency. Delete the LocalSet caveat from the module doc.
3. **`agentic-core/tests/gcs_integration.rs`** — same external behaviour; integration tests pass without changes once made `#[tokio::test]`-shaped.
4. **`agenticd/src/server.rs`** — `state.store: Arc<dyn ObjectStore>` is unchanged; existing call sites (`store.put_raw(...)`, etc.) get `.await` added. Most one-line changes.
5. **`agenticd/src/main.rs`** — the LocalSet stays. The note about "blocking GCS on LocalSet" in the architectural-analysis audit gets struck out.
6. **Tactical [A5](../ops/2026-05-21-agenticd-architectural-analysis.md#a5) `spawn_blocking` patch** — if it landed in the hardening sprint, delete it here. Its v1.0 purpose was to bound the freeze; v1.1 removes the freeze.
7. **Update the [`agenticd` architectural-analysis audit](../ops/2026-05-21-agenticd-architectural-analysis.md)** §A5 / §C1 / §B2 / §R3 to reference this ADR as "blocked by → unblocked by". Close the corresponding follow-up issue.

Owner: TBD.
