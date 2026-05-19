# Cerebrum

> OpenWolf's learning memory. Updated automatically as the AI learns from interactions.
> Do not edit manually unless correcting an error.
> Last updated: 2026-05-19

## User Preferences

<!-- How the user likes things done. Code style, tools, patterns, communication. -->

## Key Learnings

- **Project:** git.agentic
- **Description:** > **Git for agent behavior.** Atomic, reversible snapshots of the full system state that determines how an AI agent acts.
- **Wire protocol:** length-prefixed JSON (`agentic_proto::framing`) over Unix domain socket. 4-byte BE u32 length + JSON payload. Max frame: 16 MiB. Same `Envelope<T>` with `correlation_id` reused by daemon and CLI/SDK.
- **Object store layout:** `.agentic/objects/<ab>/<62-hex>.zst` (BLAKE3, zstd-compressed JSON of an `Object` enum). Refs: `.agentic/HEAD` + `.agentic/refs/heads/<name>` (newline-terminated 64-hex). All ref writes go through tmp-file + `rename(2)` for atomicity.
- **2PC staging order** (ADR-0002 Decision 3, implemented in `agentic_core::commit::stage_and_commit`): stage blobs → build Commit → write Commit blob → (Git push, Chunk C) → update branch ref. For Chunk A the single commit point degrades to the Commit-blob write.

## Do-Not-Repeat

<!-- Mistakes made and corrected. Each entry prevents the same mistake recurring. -->
<!-- Format: [YYYY-MM-DD] Description of what went wrong and what to do instead. -->

- **[2026-05-19] Don't use tuple variants wrapping `Vec<T>` (or other non-struct types) inside a `#[serde(tag = "kind")]` internally-tagged enum.** serde-json silently fails to serialize the value; `write_frame` errors before any bytes hit the socket, and the peer sees a clean EOF — the symptom is "daemon closed connection without reply." Always use struct variants (`Variant { entries: Vec<T> }`) for tagged-enum payloads that aren't already maps. Discovered while wiring `Response::Log(Vec<LogEntry>)`. See bug-005.
- **[2026-05-19] After extending `Commit` per ADR-0002, also `Box` it inside `Object::Commit`.** Clippy's `large_enum_variant` lint blocks CI otherwise — the extended Commit is ~440 bytes vs other variants ≤24 bytes. Wire format is unchanged (`Box<T>` and `T` produce identical serialized JSON).

## Decision Log

<!-- Significant technical decisions with rationale. Why X was chosen over Y. -->

- **2026-05-19 — Substrate is Approach C (ADR-0002, Accepted).** Git core for code, content-addressed blob store for non-code, coordinator (`agenticd`) on top. Rejected Option A (Git-native with refs/notes sidecar) due to refs explosion, blob unsuitability, and fake interop through GitHub mirroring. Rejected Option B (fully post-Git Merkle DAG with Git shim) due to adoption cost killing platform-led GTM funnel. Approach C preserves the structural moat at the *data model* layer (extended Commit object with intent/plan/transcript/evals/cost/signatures) while keeping Git-native interop at the *storage* layer at the code dimension. Storage layer is swappable in v2+ to a fully post-Git substrate without breaking the platform API contract.
- **2026-05-19 — The Commit object IS the platform API (ADR-0002 Decision 2).** No separate API surface. Platforms produce Commit objects with extended fields; everything downstream reads from them. This is the structural choice that makes integration tractable in an afternoon and keeps the storage layer swappable later.
- **2026-05-19 — 2PC staging order is fixed (ADR-0002 Decision 3).** Blobs to object store first → collect content hashes → build Commit blob → Git push as single commit point → update branch ref. Failure-injection tests required at each boundary. This is the plumbing that makes "atomic rollback" honest rather than aspirational.
- **2026-05-19 — Production deployments require filesystem snapshot-capable storage (ADR-0002 Decision 4).** ZFS / Btrfs / EBS or equivalent. Logical export acceptable for MVP demos only. Design `agenticd`'s storage interface around snapshot capabilities from day one to avoid an architectural rip-out at the first production deployment.
- **2026-05-19 — Rollback for destructive migrations is explicitly bounded (ADR-0002 Decision 5).** Atomic for non-destructive migrations. For destructive migrations, rollback restores from the last snapshot taken before the migration; activity between snapshot and rollback is lost. Must be in design-partner pilot conversations, not discovered after.
