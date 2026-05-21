# Cerebrum

> OpenWolf's learning memory. Updated automatically as the AI learns from interactions.
> Do not edit manually unless correcting an error.
> Last updated: 2026-05-21

## User Preferences

<!-- How the user likes things done. Code style, tools, patterns, communication. -->

## Key Learnings

- **Project:** git.agentic — Git for agent behavior. Atomic, reversible snapshots of the full `(code + prompts + tools + model + memory + schema)` tuple.
- **Wire protocol:** length-prefixed JSON (`agentic_proto::framing`) over Unix domain socket. 4-byte BE u32 length + JSON payload. Max frame: 16 MiB. `Envelope<T>` with `correlation_id` reused by daemon and CLI/SDK.
- **Object store layout:** `.agentic/objects/<ab>/<62-hex>.zst` (BLAKE3, zstd-compressed JSON of an `Object` enum). Refs: `.agentic/HEAD` + `.agentic/refs/heads/<name>` (newline-terminated 64-hex). All ref writes go through tmp-file + `rename(2)` for atomicity.

## Do-Not-Repeat

<!-- Format: [YYYY-MM-DD] Description of what went wrong and what to do instead. -->

- **[2026-05-19] Don't use tuple variants wrapping `Vec<T>` inside a `#[serde(tag = "kind")]` internally-tagged enum.** serde-json silently fails; `write_frame` errors before any bytes hit the socket, peer sees clean EOF ("daemon closed connection without reply"). Use struct variants (`Variant { entries: Vec<T> }`) instead. See bug-005.
- **[2026-05-19] After extending `Commit` per ADR-0002, `Box` it inside `Object::Commit`.** Clippy's `large_enum_variant` lint blocks CI — extended Commit is ~440 bytes vs other variants ≤24 bytes. Wire format unchanged (`Box<T>` serializes identically to `T`).

## Decision Log

- **2026-05-19 — ADR-0002 key decisions (all Accepted).** Substrate is Approach C: Git core for code, content-addressed blob store for non-code, `agenticd` coordinator. Commit object IS the platform API (no separate surface). 2PC staging order is fixed: blobs → Commit blob → Git push → branch ref update; failure-injection tests required at each boundary. Production requires snapshot-capable storage (ZFS/Btrfs/EBS). Rollback for destructive migrations is bounded: restores from last pre-migration snapshot; activity since snapshot is lost.

- **2026-05-20 — Pivoted to hardening sprint.** Roadmap weeks 1–11 all landed. Remaining risk is verification: GCS integration tests `#[ignore]`d, cold-start never timed on fresh machine, no published benchmarks, no screencast, no Executor sidecar image. Plan: `docs/product/sprint-2026-05-20.md`. `CLAUDE.md` and `roadmap.md` "Pre-MVP scaffolding" text is stale — update after Week A.

- **2026-05-20 — Claude Agent SDK integrates via `SessionStore`, NOT `on_checkpoint` hooks.** SDK primitive is `append(key, entries)` + `load(key)`, called once per turn (batched) or per frame (eager). `append` is best-effort — loud-fail requires a synchronising `PreToolUse` hook gating next tool call on agenticd ack. Amend ADR-0004 Decisions 3+4; do NOT trigger ADR-0003 escape hatch. Memo: `docs/integration/executor-harness-check.md`.

- **2026-05-21 — Entire CLI (4.3k★, MIT/Go) is closest competitor; one-sentence wedge: *"Entire indexes prompt history; git.agentic rewinds agent state."*** They capture why code changed (session metadata on `entire/checkpoints/v1` branch, file-level rewind) but explicitly don't restore prompts/tools/model/memory/schema. Three v1.1 patterns borrowed: (1) `checkpoint_remote` → ADR-0008 secondary `ObjectStore` for private-agent-state / compliance; (2) commit-time narrative summarization → ADR-0009 `Commit.narrative` field, default-off, async; (3) multi-agent hook-installer verb + file-location convention only. NOT borrowed: their "searchable record" positioning, `explain` verb, on-by-default summarization. Watch: Entire release cadence (calendar 2026-07-15).

- **2026-05-21 — v1.1 ObjectStore backend matrix + ephemeral branches (ADR-0006, ADR-0007, both Proposed).** Formalise `ObjectStore` trait as v1.1 backend seam with `delete` + `list_prefix` + `AsyncObjectStore`; managed-Git providers wrap behind `ManagedGitStore` adapter, never adopted as substrate (ADR-0002 Decision 6 no-leak rule holds). Promote `executor/<session_id>` (ADR-0005) to first-class `refs/ephemeral/<namespace>/<id>` with TTL GC and `agentic promote` — shape was going to recur on second integration. Code.Storage (Pierre Computer Company) is watchlist-only for v1.0; not adopted.