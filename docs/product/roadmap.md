# 12-Week MVP Roadmap

**Status:** Historical record — weeks 1–11 shipped in full; only the post-launch design-partner track remains open
**Last updated:** 2026-07-10
**Public release:** 2026-05-22 — pulled in from the planned 2026-05-26, which itself was pulled forward from the originally-planned 2026-08-11.
**Design partners:** moved post-launch (was roadmap Week 12). Full v1.0 scope preserved.

This roadmap is organized around a single criterion: **at the end of each week, what works that didn't work before?** No invisible weeks. Each week ends with a checkpoint that is either visibly broken or visibly working.

> **Schedule change (2026-05-22).** With weeks 1–11 already landed and the broken-prompt demo working end-to-end, the public release went out 2026-05-22 — four days ahead of the planned 2026-05-26 ship target as the hardening sprint closed faster than expected. Design-partner onboarding (originally Week 12) is sequenced after the public release rather than gating it. The 12-week narrative below is preserved as the historical record of how v1.0 was built; the "Week 12" section now describes post-launch work.

## Phase guideposts

| Phase | Weeks | Outcome |
|---|---|---|
| **Phase 0 — Foundations** | 1–2 | Repo, daemon skeleton, object store can write/read a blob. |
| **Phase 1 — Snapshot primitive** | 3–6 | `agentic commit` works against a real Postgres+pgvector instance. |
| **Phase 2 — Rollback** | 7–9 | `agentic rollback` works including schema migrations. |
| **Phase 3 — Integration + demo** | 10–11 | LangGraph adapter, the broken-prompt demo runs end-to-end. |
| **Phase 4 — Sharpening + design partners** | post-launch (was week 12) | Three design partners running it. |

---

## Week-by-week

*All eleven build weeks shipped in full. The per-item checklists are collapsed
below; the original itemized plan is in git history
(`git log --follow docs/product/roadmap.md`, pre-2026-07-10 revisions).*

### Week 1 — Skeleton & object store ✅
**Goal:** the daemon starts, the CLI talks to it, blobs can be written and re-read.
✅ Shipped: workspace + CI (Linux/macOS matrix, fmt/clippy/test), Unix-socket daemon with `Ping`, BLAKE3 content-addressed blob store under `.agentic/objects/`.

### Week 2 — Tree, commit, refs ✅
**Goal:** the object model is real — commit a tuple of on-disk artifacts, read back history.
✅ Shipped: Tree + Commit objects, atomic `refs/heads/*` + `HEAD` updates, `agentic commit`/`log`, crash-safe commit staging.

### Week 3 — pgvector adapter, segment writer ✅
**Goal:** the daemon reads Postgres tables into content-addressed segments.
✅ Shipped: sqlx+pgvector adapter, tracked-table bootstrap, sealed segments + manifest object kind, 1M-row integration test.

### Week 4 — Logical decoding stream ✅
**Goal:** new Postgres writes land in segments in real time.
✅ Shipped: logical-decoding client with trigger-based fallback, batching streamer with disk-buffered backpressure.

### Week 5 — Atomic memory snapshot ✅
**Goal:** `agentic commit` captures a coherent memory snapshot.
✅ Shipped: advisory-lock snapshot algorithm, manifest in `Commit.memory_snapshot`, concurrent writes land in the next commit; < 2s target met (see [benchmarks](../architecture/benchmarks.md)).

### Week 6 — MCP fingerprinting, model + schema versioning ✅
**Goal:** every commit captures all six tuple dimensions.
✅ Shipped: MCP `tools/list` fingerprinter (URL policy per [ADR-0016](../adr/0016-mcp-url-policy.md)), model + schema version capture, migration directory convention.

### Week 7 — Diff ✅
**Goal:** `agentic diff` produces a useful behavioral diff.
✅ Shipped: per-dimension diff (prompts, tools, model, memory, schema) with `--json`.

### Week 8 — Rollback (memory + schema) ✅
**Goal:** memory and schema restore coherently.
✅ Shipped: rollback planning, ordered reverse migrations (destructive ones gated per [ADR-0014](../adr/0014-destructive-rollback-approval-gate.md)), transactional memory restore, forward-recorded rollback commits.

### Week 9 — Rollback (prompts + tools + model) ✅
**Goal:** the remaining dimensions roll back; failure modes are clean.
✅ Shipped: prompt write-back, tool pins, model-version surfacing, `--dry-run`/`--yes`, clear aborts on missing reverse migrations; < 5s end-to-end on the demo.

### Week 10 — Python SDK + LangGraph integration ✅
**Goal:** usable from a real LangGraph application.
✅ Shipped: `agentic-sdk` (TestPyPI + CI release job), typed client, `AgenticCheckpointer` drop-in.

### Week 11 — The demo, polished ✅
**Goal:** the broken-prompt demo runs reliably from a fresh machine in < 5 minutes.
✅ Shipped: scripted scenario (`run-demo.sh` — self-contained venv + container bring-up), asciinema recording, quickstart README. Since 2026-07-10 the `demo` CI job replays it end-to-end on every PR (under 2 minutes on a fresh runner with cached cargo deps).

### Post-launch — Design partners *(originally Week 12; moved post-ship 2026-05-21)*
**Goal:** three teams have run `agentic rollback` against their own staging environments.

- [ ] Design partner #1 onboarded (privately; in-person setup support)
- [ ] Design partner #2 onboarded
- [ ] Design partner #3 onboarded
- [ ] Three feedback documents captured
- [ ] One blog post drafted
- [ ] One investor-ready short deck (10 slides) drafted

**Demo at end of week:** a short feedback writeup from each partner, including at least one "would have saved hours" anecdote.

---

## Cross-cutting tracks (every week)

- **Tests.** Coverage target: 80% on `agentic-core`. Integration tests run on every PR.
- **Docs.** Quickstart and concepts updated as features land.
- **Benchmarks.** A simple `criterion`-based benchmark suite runs nightly; regressions block merge.
- **Daily kill-criteria check.** Are we still on the wedge? Anything we built this week that isn't load-bearing for the demo gets ripped out.

## Executor integration workstream (per ADR-0003 and ADR-0004)

ADR-0003 commits the first platform-partner integration as the first non-LangGraph integration target in v1.0, with **atomic real-time integration via a co-located sidecar `agenticd`** (ADR-0004). This is the largest MVP scope add since the foundational ADRs. The roadmap must absorb it; if it threatens the broken-prompt demo, the documented escape hatch is to revert to the originally-drafted manifest-export shape (ADR-0003 Decision 2's earlier framing).

Touch points across the existing weeks:

- **Week 6 — schema and harness verification.** When the Commit object captures all six tuple dimensions for LangGraph, verify (a) the schema can express a Claude Agent SDK session without framework-specific fields, and (b) the Claude Agent SDK's checkpoint primitives match what ADR-0004 Decision 3 assumes — `on_checkpoint`-style firing at tool-call boundaries, plus pause/restore support. If either fails, this is the cheapest week to discover it, and the fallback is to revert to manifest-export per the ADR-0003 escape hatch.
- **Weeks 7–9 — GCS-backed `ObjectStore`.** Alongside the rollback work, add a GCS-backed implementation of the `ObjectStore` trait in `crates/agentic-core/src/store.rs`. Write-through on every checkpoint; read-through local cache for diff/replay. Integration tests against a real GCS bucket (or `fake-gcs-server` in CI). This is real engineering on the substrate, not glue, and is the single biggest piece of new code introduced by the Executor workstream.
- **Week 10 — sidecar packaging.** Container image that runs `agenticd` as a sidecar with config for GCS-backed storage and Unix-socket IPC. Document how the Coding worker mounts the socket and what env vars wire the dependency. Alongside, ship the LangGraph SDK as planned for week 10 — these two tracks run in parallel.
- **Week 11 — second smoke demo.** End-to-end Executor session against the sidecar (using a stubbed Cloud Run worker calling the ticket dispatcher MCP, since the real Executor may not be ready). Demonstrate atomic rollback of an in-flight session: pause mid-tool-call, restore prior tuple, resume. This is a second demo alongside the LangGraph broken-prompt demo.

**Coordination with the platform-partner integration team.** The Coding worker must be written checkpoint-aware against the Claude Agent SDK. That is the Executor's responsibility, but our schedule depends on it. Weekly sync starting week 6 to verify the Executor's harness work isn't blocking our sidecar work and vice versa.

**Escape hatch — hard decision point at end of week 8.** If the GCS-backed `ObjectStore` is not passing **API-contract integration tests** (put / get / has / not-found roundtrips against `fsouza/fake-gcs-server` or a real GCS bucket) by end of week 8, revert ADR-0003 Decision 2 to its originally-drafted manifest-export shape, defer atomic Executor to v1.1, and ship the layered/offline path for v1.0. The demo is the discipline; atomic Executor is additive proof. This trade is non-negotiable.

*Status, 2026-05-20:* the API-contract bar is met against **both** fake-gcs and real GCS — `crates/agentic-core/tests/gcs_integration.rs` runs green against `fsouza/fake-gcs-server` in the `gcs` CI job (PR #20), and the same four tests have been run once against a real GCS bucket in `newcrm-493107` with bearer auth from `gcloud auth print-access-token` and all four passed (bucket + token ephemeral; teardown verified). **Production-readiness validation** — concurrent writers (multiple in-flight commits against the same bucket, per the ADR-0002 §3 2PC staging order), partial-upload failure injection (verify the 2PC boundary holds when GCS returns 5xx mid-stream), service-account auth (instead of user-account bearer), and large-blob streaming — is explicitly **not** gated by this kill-criterion. Those land as separate hardening work in v1.0 → v1.1 with their own milestones; see the archived [`docs/archive/sprint-2026-05-20.md`](../archive/sprint-2026-05-20.md) for the immediate follow-up framing at the time.

**Negative slack on this track.** Atomic in v1.0 means the plan no longer has zero slack — it has *negative* slack on the Executor track relative to the original 12-week budget. Closing the gap requires one of: additional engineering capacity dedicated to the Executor workstream, cutting another piece of MVP scope, or accepting that the ship date is at higher risk than ADR-0001 assumed. With the ship now 2026-05-22 (pulled forward from 2026-08-11), the Executor sidecar work that hasn't already landed is the most exposed item; treat any Executor-track slip as a candidate for the documented ADR-0003 escape hatch rather than a ship-date slip. Surface this to design partners up front; do not pretend the plan is unchanged.

## Slip budget

The plan has zero slack. Slip will happen. Slip strategy:

1. **Week 5 (atomic snapshot) is the most likely slip.** If it slips, push weeks 6–7 by the same amount; do not skip them.
2. **Week 10 (SDK + LangGraph) cannot slip past week 11.** If we're behind by week 9, drop one diff feature, not the LangGraph integration.
3. **Post-launch design partners must land even if MVP features are reduced.** Three users on a smaller MVP is better than zero users on a perfect MVP. (Originally a Week 12 gate; with the 2026-05-22 ship, partners come in the eight weeks after public release.)
4. **The Executor atomic-integration track (ADR-0003 Decision 2 + ADR-0004) is the highest slip risk in the plan.** If the GCS-backed `ObjectStore` + sidecar work cannot land with API-contract integration tests passing by **end of week 8** (see the §"Executor integration workstream" escape-hatch paragraph for what "passing" means here — roundtrip contract, NOT production-scale concurrent / failure-injection coverage), revert to the originally-drafted manifest-export shape and defer atomic Executor to v1.1. Do not let this track compromise the broken-prompt demo.

## Kill criteria

If at the **week 6 checkpoint** we cannot produce a commit object containing all six tuple dimensions for a non-trivial agent, we stop and reassess. The wedge depends on this working; if it can't be made to work in six weeks, the technical thesis is wrong.

If at the **week 11 checkpoint** the demo takes more than 15 minutes to set up from `git clone`, design partners will not adopt. We stop and fix the setup story before week 12.

If at **eight weeks post-launch (2026-07-17)** zero design partners are using the tool weekly in their own work, the wedge does not have product-market pull and we abandon the current scope. (Originally tied to Week 12; with the 2026-05-22 ship, the kill-criterion clock starts at the public release — eight weeks past 2026-05-22 is 2026-07-17.)

## Out of scope for the 12 weeks (will be asked about)

- Web UI / dashboard. → v1.1.
- Hosted SaaS. → after seed.
- Mem0 / Zep / Letta backends. → v1.1.
- CrewAI / AutoGen integrations. → v1.1.
- Remote or in-process `agenticd` deployments for the Executor. → v2+. v1.0 uses the sidecar shape per [ADR-0004](../adr/0004-realtime-agenticd-for-executor.md); the rejected alternatives (remote, in-process library) are not on the roadmap.
- Eval / CI/AE pipeline. → never (out of category).
- MCP server registry. → never (out of category).
- A2A protocol routing. → never (out of category).
- Sandbox / micro-VM execution. → never (out of category).
- Authentication, RBAC, audit log. → post-seed.

Everything outside this list is a distraction.

---

## v1.1 — what opens 2026-05-27

Detailed plan in [`docs/product/v1.1-plan.md`](./v1.1-plan.md). Governing ADRs: [ADR-0006](../adr/0006-objectstore-backend-trait.md) (`ObjectStore` backend trait), [ADR-0007](../adr/0007-ephemeral-branches-agent-run-primitive.md) (ephemeral branches as a primitive).

**This is the slice of v1.1 driven by the Code.Storage assessment and the hardening-sprint rollover items.** It is not the full v1.1; broader items (Mem0 / Zep / Letta memory backends, CrewAI / AutoGen / LlamaIndex framework integrations, web UI, hosted SaaS) live behind the same trait/contract surfaces and get their own plans when a design partner pulls.

**The hard scope rule from v1.0 carries over:** nothing in v1.1 is permitted to compromise the broken-prompt demo, and the demo continues to run against `FsObjectStore` with no environment-variable changes from the v1.0 quickstart.

### Three workstreams

| # | Workstream | Governing ADR | Estimated effort | Key gate |
|---|---|---|---|---|
| W1.1 | **Storage backend matrix** — `delete` + `list_prefix` + async trait variant; production-readiness validation for GCS; S3 or `ManagedGitStore` adapter (design-partner-driven). | [ADR-0006](../adr/0006-objectstore-backend-trait.md) | 4–5 weeks | Broken-prompt demo continues to pass with `FsObjectStore` default. |
| W1.2 | **Ephemeral branches as a primitive** — `refs/ephemeral/<namespace>/<id>` ref layout; `agentic promote`, `agentic ephemeral seal\|discard\|retain\|list`; TTL GC; rehome ADR-0005's `executor/<session_id>`. | [ADR-0007](../adr/0007-ephemeral-branches-agent-run-primitive.md) | 3–4 weeks | ADR-0005 Executor integration tests pass post-refactor, no SDK surface change. |
| W1.3 | **Hardening rollover** — blob-level GC; `agentic init` snapshot-capability hard guard in production mode (ADR-0002 Decision 4 follow-through); v1.1 benchmark suite; doc pass on `executor-sidecar.md`. | (ops / hardening) | 2–3 weeks | v1.0 perf numbers from `snapshot-model.md` §9 must not regress. |

### Timeline shape (12-week budget, same cadence as v1.0)

```
2026-05-22   v1.0 ships (broken-prompt demo, public repo release; design partners post-launch)
2026-05-27   v1.1 opens. No celebratory pause.
2026-06-09   W1.1.1, W1.1.2, W1.2.1, W1.2.2 done in parallel.
2026-06-30   W1.2.3–W1.2.6 (promote + ephemeral CLI + Executor rehome). W1.3.1 (blob GC).
2026-07-21   W1.1.3 (GCS production-readiness). W1.2.7. W1.3.2–W1.3.4.
2026-08-11   W1.1.4 (S3) OR W1.1.5 (first ManagedGitStore adapter), design-partner-driven.
2026-08-25   v1.1 ship target.
```

This is a sizing exercise, not a commitment. The actual schedule will be set after v1.0 ships and real design-partner feedback drives priorities. The Code.Storage assessment is the proximate cause for ADR-0006 and ADR-0007; v1.1 does **not** adopt Code.Storage (or any managed-Git provider) as the substrate — only as one candidate among several behind the `ObjectStore` trait, opt-in, never on the demo path.

### v1.2+ (deferred again, not in this plan)

Mem0 / Zep / Letta adapters, CrewAI / AutoGen / LlamaIndex integrations, web UI / agent-PR review primitive, hosted SaaS, per-hunk attestation, multi-agent commit graphs. These live in the same `Out of scope` discipline this section sits below. They open after v1.1 ships, with separate plans.
