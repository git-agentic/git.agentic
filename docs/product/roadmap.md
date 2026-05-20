# 12-Week MVP Roadmap

**Status:** Draft v0.1
**Last updated:** 2026-05-19
**Target ship:** 2026-08-11 (Tuesday, week 12)

This roadmap is organized around a single criterion: **at the end of each week, what works that didn't work before?** No invisible weeks. Each week ends with a checkpoint that is either visibly broken or visibly working.

## Phase guideposts

| Phase | Weeks | Outcome |
|---|---|---|
| **Phase 0 — Foundations** | 1–2 | Repo, daemon skeleton, object store can write/read a blob. |
| **Phase 1 — Snapshot primitive** | 3–6 | `agentic commit` works against a real Postgres+pgvector instance. |
| **Phase 2 — Rollback** | 7–9 | `agentic rollback` works including schema migrations. |
| **Phase 3 — Integration + demo** | 10–11 | LangGraph adapter, the broken-prompt demo runs end-to-end. |
| **Phase 4 — Sharpening + design partners** | 12 | Three design partners running it. |

---

## Week-by-week

### Week 1 — Skeleton & object store
**Goal:** the daemon starts, the CLI talks to it, blobs can be written and re-read.

- [ ] Rust workspace builds (already scaffolded; ensure CI is green)
- [ ] `agenticd` listens on a Unix socket, responds to `Ping`
- [ ] `agentic ping` succeeds
- [ ] BLAKE3-keyed blob writes to `.agentic/objects/`; round-trip verified by test
- [ ] CI: `cargo test`, `cargo fmt`, `cargo clippy --deny warnings`
- [ ] CI: GitHub Actions matrix on Linux + macOS

**Demo at end of week:** `agentic init && agentic blob put hello.txt && agentic blob get <hash>` shows the same bytes.

### Week 2 — Tree, commit, refs
**Goal:** the object model is real. We can commit a tuple of pure on-disk artifacts (no memory yet) and read back the history.

- [ ] Tree objects implemented
- [ ] Commit objects implemented (with `parent`, `prompts`, `tools`, `model`, `code_sha` only — `memory_snapshot` and `schema_version` placeholder)
- [ ] `refs/heads/main` and `HEAD`; atomic ref updates via rename
- [ ] `agentic commit -m "..."` works against a directory of prompt files + a static tool manifest
- [ ] `agentic log` walks parents and prints
- [ ] WAL for crash recovery during commit

**Demo at end of week:** `agentic commit` twice with a changed prompt, then `agentic log --oneline` shows both commits.

### Week 3 — pgvector adapter, segment writer (read-only)
**Goal:** the daemon attaches to a Postgres database and *reads* its tables into segments. Doesn't snapshot yet; just builds the segment store from a snapshot of current state.

- [ ] Postgres connection via `sqlx` with pgvector type support
- [ ] `agentic init --postgres URL` validates pgvector and asks which tables to track
- [ ] Bulk segment build: full-table read → sealed 64MB segments, content-addressed
- [ ] Segment manifest object kind added to the object model
- [ ] Integration test with a 1M-row table

**Demo at end of week:** `agentic memory bootstrap` produces a deterministic segment manifest for a fixed test database.

### Week 4 — Logical decoding stream
**Goal:** new writes to Postgres land in segments in real time.

- [ ] Logical decoding client connecting to a replication slot
- [ ] Trigger-based fallback for managed Postgres without `wal_level=logical`
- [ ] Streamer batches writes into segments, seals at threshold
- [ ] Backpressure handling (slow segment writes don't block agent writes; we buffer to disk)
- [ ] Integration test: 10k writes through the agent show up in segments; checksums match

**Demo at end of week:** start the daemon, write 10,000 rows from a test client, see them appear as new segments without restarting Postgres.

### Week 5 — Atomic memory snapshot
**Goal:** `agentic commit` captures a coherent memory snapshot.

- [ ] Snapshot algorithm: advisory lock, copy-on-write the active head, build manifest, release
- [ ] Snapshot manifest written into the commit's `memory_snapshot` field
- [ ] Commit-time concurrent writes are correctly placed in the *next* commit
- [ ] Benchmark: target < 2s on 1M rows / 100 deltas

**Demo at end of week:** `agentic commit` with simulated concurrent traffic — verify atomicity by replaying the commit's snapshot and confirming the snapshotted state matches a known good baseline.

### Week 6 — MCP fingerprinting, model versioning, schema versioning
**Goal:** the tuple is complete. Every commit captures all six dimensions.

- [ ] MCP fingerprinter: connect, request `tools/list`, hash canonicalized JSON
- [ ] `.agentic/config.toml` schema; `agentic mcp pin <server>`
- [ ] Model version captured from SDK; refused if missing
- [ ] Schema version captured from `agentic_schema_version()` helper
- [ ] Schema migration directory + numbering convention; up-only execution

**Demo at end of week:** `agentic commit` against a real LangGraph-shaped setup; the resulting commit object has all six dimensions populated.

### Week 7 — Diff
**Goal:** `agentic diff` produces a useful behavioral diff.

- [ ] Prompt diff (line-based on text blobs)
- [ ] Tool diff (manifest hash changes; pinned-version changes)
- [ ] Model diff (string equality, version comparison)
- [ ] Memory diff (segment additions/removals + sampled row diffs)
- [ ] Schema diff (migration numbers)
- [ ] `--json` output

**Demo at end of week:** modify a prompt + add 1000 memory rows + bump a tool version → `agentic diff HEAD^ HEAD` shows all three changes clearly.

### Week 8 — Rollback (memory + schema)
**Goal:** `agentic rollback` restores memory and schema coherently.

- [ ] Compute rollback plan; surface migration requirements
- [ ] Reverse schema migrations executed in correct order
- [ ] Memory restore: stream the diff back into Postgres inside a transaction
- [ ] Validation pass (row counts + sampled checksums)
- [ ] Rollback is itself committed (forward-recorded history)

**Demo at end of week:** insert garbage rows + apply a schema change → roll back → garbage is gone, schema is back, agent behavior matches pre-garbage baseline.

### Week 9 — Rollback (prompts + tools + model)
**Goal:** the remaining tuple dimensions roll back; failure modes are clean.

- [ ] Prompt files written back to disk
- [ ] Tool pins updated in `config.toml`
- [ ] Model version surfaced (user must redeploy if the model has changed)
- [ ] Missing reverse migrations abort with clear error
- [ ] `--dry-run` prints the plan without executing
- [ ] `--yes` skips confirmation

**Demo at end of week:** the full broken-prompt scenario, but driven from the CLI, end-to-end. Rollback runs in < 5s.

### Week 10 — Python SDK + LangGraph integration
**Goal:** developers can use git.agentic from a real LangGraph application.

- [ ] `agentic-sdk` package on test PyPI
- [ ] `agentic.commit()`, `agentic.rollback()`, `agentic.diff()`, `agentic.log()`
- [ ] `AgenticCheckpointer` implements LangGraph's checkpointer interface
- [ ] Drop-in replacement smoke test against a published LangGraph example

**Demo at end of week:** a 20-line LangGraph script that uses `AgenticCheckpointer` and gets free commits on every graph step.

### Week 11 — The demo, polished
**Goal:** the broken-prompt demo runs reliably on a fresh machine in < 5 minutes.

- [ ] Demo repo: minimal LangGraph agent, scripted scenario
- [ ] `docker-compose up` brings up Postgres + agenticd + the demo agent
- [ ] Scenario script: deploy → break → roll back; output is legible
- [ ] Recorded screencast / asciinema
- [ ] Quickstart documentation in `examples/langgraph-rollback/README.md`

**Demo at end of week:** YC-partner-style cold viewing: someone who hasn't seen it can run the demo from `git clone` in five minutes and see the rollback work.

### Week 12 — Design partners
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

ADR-0003 commits the the first platform-partner integration as the first non-LangGraph integration target in v1.0, with **atomic real-time integration via a co-located sidecar `agenticd`** (ADR-0004). This is the largest MVP scope add since the foundational ADRs. The roadmap must absorb it; if it threatens the broken-prompt demo, the documented escape hatch is to revert to the originally-drafted manifest-export shape (ADR-0003 Decision 2's earlier framing).

Touch points across the existing weeks:

- **Week 6 — schema and harness verification.** When the Commit object captures all six tuple dimensions for LangGraph, verify (a) the schema can express a Claude Agent SDK session without framework-specific fields, and (b) the Claude Agent SDK's checkpoint primitives match what ADR-0004 Decision 3 assumes — `on_checkpoint`-style firing at tool-call boundaries, plus pause/restore support. If either fails, this is the cheapest week to discover it, and the fallback is to revert to manifest-export per the ADR-0003 escape hatch.
- **Weeks 7–9 — GCS-backed `ObjectStore`.** Alongside the rollback work, add a GCS-backed implementation of the `ObjectStore` trait in `crates/agentic-core/src/store.rs`. Write-through on every checkpoint; read-through local cache for diff/replay. Integration tests against a real GCS bucket (or `fake-gcs-server` in CI). This is real engineering on the substrate, not glue, and is the single biggest piece of new code introduced by the Executor workstream.
- **Week 10 — sidecar packaging.** Container image that runs `agenticd` as a sidecar with config for GCS-backed storage and Unix-socket IPC. Document how the Coding worker mounts the socket and what env vars wire the dependency. Alongside, ship the LangGraph SDK as planned for week 10 — these two tracks run in parallel.
- **Week 11 — second smoke demo.** End-to-end Executor session against the sidecar (using a stubbed Cloud Run worker calling the ticket dispatcher MCP, since the real Executor may not be ready). Demonstrate atomic rollback of an in-flight session: pause mid-tool-call, restore prior tuple, resume. This is a second demo alongside the LangGraph broken-prompt demo.

**Coordination with the platform-partner integration team.** The Coding worker must be written checkpoint-aware against the Claude Agent SDK. That is the Executor's responsibility, but our schedule depends on it. Weekly sync starting week 6 to verify the Executor's harness work isn't blocking our sidecar work and vice versa.

**Escape hatch — hard decision point at end of week 8.** If the GCS-backed `ObjectStore` is not passing **API-contract integration tests** (put / get / has / not-found roundtrips against `fsouza/fake-gcs-server` or a real GCS bucket) by end of week 8, revert ADR-0003 Decision 2 to its originally-drafted manifest-export shape, defer atomic Executor to v1.1, and ship the layered/offline path for v1.0. The demo is the discipline; atomic Executor is additive proof. This trade is non-negotiable.

*Status, 2026-05-20:* the API-contract bar is met — `crates/agentic-core/tests/gcs_integration.rs` runs green against fake-gcs in the `gcs` CI job (PR #20). **Production-readiness validation** — concurrent writers (multiple in-flight commits against the same bucket, per the ADR-0002 §3 2PC staging order), partial-upload failure injection (verify the 2PC boundary holds when GCS returns 5xx mid-stream), real-GCS auth with a service-account token, and large-blob streaming — is explicitly **not** gated by this kill-criterion. Those land as separate hardening work in v1.0 → v1.1 with their own milestones; see `docs/product/sprint-2026-05-20.md` for the immediate follow-up framing.

**Negative slack on this track.** Atomic in v1.0 means the plan no longer has zero slack — it has *negative* slack on the Executor track relative to the original 12-week budget. Closing the gap requires one of: additional engineering capacity dedicated to the Executor workstream, cutting another piece of MVP scope, or accepting that the 2026-08-11 ship date is at higher risk than ADR-0001 assumed. Surface this to design partners up front; do not pretend the plan is unchanged.

## Slip budget

The plan has zero slack. Slip will happen. Slip strategy:

1. **Week 5 (atomic snapshot) is the most likely slip.** If it slips, push weeks 6–7 by the same amount; do not skip them.
2. **Week 10 (SDK + LangGraph) cannot slip past week 11.** If we're behind by week 9, drop one diff feature, not the LangGraph integration.
3. **Week 12 design partners must land even if MVP features are reduced.** Three users on a smaller MVP is better than zero users on a perfect MVP.
4. **The Executor atomic-integration track (ADR-0003 Decision 2 + ADR-0004) is the highest slip risk in the plan.** If the GCS-backed `ObjectStore` + sidecar work cannot land with API-contract integration tests passing by **end of week 8** (see the §"Executor integration workstream" escape-hatch paragraph for what "passing" means here — roundtrip contract, NOT production-scale concurrent / failure-injection coverage), revert to the originally-drafted manifest-export shape and defer atomic Executor to v1.1. Do not let this track compromise the broken-prompt demo.

## Kill criteria

If at the **week 6 checkpoint** we cannot produce a commit object containing all six tuple dimensions for a non-trivial agent, we stop and reassess. The wedge depends on this working; if it can't be made to work in six weeks, the technical thesis is wrong.

If at the **week 11 checkpoint** the demo takes more than 15 minutes to set up from `git clone`, design partners will not adopt. We stop and fix the setup story before week 12.

If at the **end of week 12** zero design partners are using the tool weekly in their own work, the wedge does not have product-market pull and we abandon the current scope.

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
