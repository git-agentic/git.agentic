# Architecture Overview

**Status:** Design — partial implementation
**Last updated:** 2026-05-19

This document describes the runtime topology of `git.agentic` at MVP and the seams that admit later expansion. Detailed object semantics are in [snapshot-model.md](./snapshot-model.md); the why-this-shape questions are in [ADR-0001](../adr/0001-architecture-foundations.md).

## 1. The 30-second picture

```
┌──────────────────────────────────────────────────────────────────────┐
│                           USER APPLICATION                            │
│  ┌──────────────────────────────────────────────────────────────┐   │
│  │  LangGraph agent                                              │   │
│  │  ┌────────────┐  ┌──────────────┐  ┌────────────────────┐   │   │
│  │  │  Nodes/    │─▶│ AgenticCheckpointer (Python SDK)        │   │
│  │  │  Edges     │  │  - on compile → commit                  │   │
│  │  └────────────┘  │  - on invoke  → snapshot ref            │   │
│  │                  └────────────────┬──────────────────────┘   │   │
│  └──────────────────────────────────│──────────────────────────┘   │
│                                     │ unix socket                    │
└─────────────────────────────────────│────────────────────────────────┘
                                      ▼
        ┌────────────────────────────────────────────────────┐
        │                  agenticd (Rust daemon)             │
        │  ┌─────────────────────────────────────────────┐   │
        │  │ Commit / Restore / Diff engine              │   │
        │  ├─────────────────────────────────────────────┤   │
        │  │ Memory segment writer + manifest builder    │   │
        │  ├─────────────────────────────────────────────┤   │
        │  │ Schema migration runner                     │   │
        │  ├─────────────────────────────────────────────┤   │
        │  │ MCP fingerprinter                           │   │
        │  └─────────────────────────────────────────────┘   │
        └───┬──────────────────────┬────────────────────┬────┘
            │                      │                    │
            ▼                      ▼                    ▼
   ┌────────────────┐   ┌──────────────────┐   ┌──────────────┐
   │ Object store    │   │ Postgres +        │   │ MCP servers   │
   │ .agentic/       │   │ pgvector          │   │ (versioned)   │
   │ objects/        │   │ + logical decoder │   └──────────────┘
   │ (blobs/trees/   │   └──────────────────┘
   │  segments/      │
   │  commits)       │
   └────────────────┘
                      ▲
                      │ `agentic` CLI (Rust binary)
                      │
              ┌───────┴───────┐
              │  Engineer at  │
              │  the terminal │
              └───────────────┘
```

Three top-level surfaces (Python SDK, CLI, daemon) all talk to the same Rust core. The object store is on the local filesystem in MVP; the daemon is the only writer.

## 2. Components

### 2.1 `agenticd` — the daemon

A long-lived Rust process. Single binary. Listens on a Unix domain socket (Windows named pipe later) for SDK and CLI requests.

Responsibilities:

- **Object store I/O.** All writes to `.agentic/objects/` go through the daemon.
- **Snapshot orchestration.** Coordinates the multi-step commit (§4 of `snapshot-model.md`) under an exclusive lock.
- **Memory segment streaming.** Consumes a Postgres logical replication slot, builds segments, hashes them, persists them.
- **MCP fingerprinting.** Maintains a small worker pool that asks each registered MCP server for its `tools/list` manifest and hashes the canonicalized response.
- **Schema migration runner.** On `rollback`, executes reverse migrations in dependency order.
- **Refs management.** Maintains `HEAD` and `refs/heads/*`.

Process model: single binary, multi-threaded with a `tokio` runtime. One commit at a time (global lock). One memory streamer thread per memory backend. One MCP fingerprinter pool. One IPC server thread.

We deliberately keep the daemon stateless across restarts: refs are on disk, segments are on disk, in-flight commits use a write-ahead log under `.agentic/wal/` so a crash mid-commit recovers cleanly.

### 2.2 `agentic` — the CLI

A Rust binary, separate from `agenticd`. It is a thin wrapper that opens the daemon socket and issues commands. Commands in MVP:

```
agentic init [--repo PATH] [--postgres URL]
agentic commit -m "..." [--allow-empty]
agentic log [--graph] [--oneline]
agentic checkout <ref>
agentic rollback <ref> [--yes] [--dry-run]
agentic diff <a> [<b>] [--prompts|--tools|--memory|--schema|--all]
agentic branch [list|create|delete] [name] [from]
agentic status
agentic gc                     # (stub in MVP)
agentic config                 # (stub in MVP)
agentic mcp pin <server>       # (re-fetch + pin a MCP manifest)
```

CLI output is human-first by default, with `--json` everywhere for scripting.

### 2.3 Python SDK — `agentic`

Pure-Python package, distributed on PyPI as `agentic-sdk` (the bare name `agentic` is likely taken). It connects to the local daemon over the same Unix socket and exposes:

```python
import agentic
from agentic.langgraph import AgenticCheckpointer

# Manual commit
agentic.commit(
    message="ship new search prompt",
    prompts={"search/system.txt": "..."},
    tools=["http://localhost:8001/mcp"],
    model="anthropic:claude-opus:2026-05-01",
)

# Manual rollback
agentic.rollback("v0.7.3")

# Diff
print(agentic.diff("HEAD^", "HEAD"))

# LangGraph integration
graph = StateGraph(...)
checkpointer = AgenticCheckpointer(repo=".agentic")
app = graph.compile(checkpointer=checkpointer)
```

The SDK is intentionally small. The interesting work happens in the daemon. The SDK is a typed protobuf client plus thin Pythonic ergonomics.

### 2.4 Object store

On-disk content-addressed store under `.agentic/objects/`. See [snapshot-model.md §2](./snapshot-model.md#2-the-object-store-layout) for layout. In MVP this is local-only; remote object stores (S3-compatible) come later behind the same `ObjectStore` trait.

### 2.5 Memory backend — Postgres + pgvector

The customer's existing Postgres database, with one addition: a `wal_level=logical` configuration and a replication slot named `agentic_slot`. The daemon connects as a logical decoding client and receives a stream of every commit's row changes.

We provide an `agentic init --postgres URL` command that:

1. Verifies pgvector is installed.
2. Creates the replication slot.
3. Installs a small set of helper functions (`agentic_schema_version()`, an audit table for migrations).
4. Detects existing tables and asks which should be tracked.

If `wal_level=logical` is unavailable (managed Postgres without permission, etc.), we fall back to a trigger-based capture mode. This is documented as a degraded experience: snapshots still work but write overhead is higher.

### 2.6 MCP integration

A registered MCP server has an entry in `.agentic/config.toml`:

```toml
[[tools.mcp]]
name = "search"
url  = "http://localhost:8001"
pin  = "sha256:9c1e4ad..."   # set by `agentic mcp pin search`
```

The daemon talks JSON-RPC over MCP's transport (stdio or HTTP+SSE) and hashes the `tools/list` response. If a tool is not pinned, we still hash but warn that this commit is not fully reproducible.

### 2.7 Schema migrations

Migrations live in `.agentic/schema/`:

```
.agentic/schema/
├── 001_init.up.sql
├── 001_init.down.sql
├── 002_add_episode_metadata.up.sql
├── 002_add_episode_metadata.down.sql
└── ...
```

Numbered, paired (`up`/`down`), and tracked in a `agentic_migrations` table inside the user's Postgres. We piggyback on the conventions of `goose` / `dbmate` / `golang-migrate` rather than invent new ones.

## 3. Data flow: a commit

1. User writes code, edits a prompt, deploys, runs the agent.
2. Agent calls `await checkpointer.put(...)` after a graph step.
3. `AgenticCheckpointer` sends a `Commit` request to `agenticd` over the socket.
4. Daemon acquires the global commit lock.
5. Daemon hashes prompts, tools, model into trees/blobs.
6. Daemon issues a `pg_advisory_xact_lock` on the agent's schema, takes the current segment manifest from the streamer, copy-on-writes the active head segment, and builds the memory snapshot tree.
7. Daemon shells `git rev-parse HEAD` for the code SHA.
8. Daemon assembles the `Commit` object, writes it to the object store, updates `refs/heads/<current>`.
9. Lock released. Response returned to SDK: `{commit_hash: "..."}`.

Total wall-clock target: < 2s.

## 4. Data flow: a rollback

1. Engineer types `agentic rollback v0.7.3`.
2. CLI sends `Rollback` request to daemon.
3. Daemon computes the diff between current `HEAD` and `v0.7.3` (see [snapshot-model §7](./snapshot-model.md#7-diffs)).
4. Daemon builds a migration plan. If any reverse migration is missing or marked irreversible, the plan is aborted with a clear message.
5. Plan shown to user; confirmation required (unless `--yes`).
6. Daemon acquires the commit lock.
7. Daemon pauses Postgres writes via advisory lock.
8. Daemon applies reverse schema migrations.
9. Daemon restores memory rows (diff-based; only changed segments are streamed back).
10. Daemon writes prompts to disk, updates tool pins in `config.toml`.
11. Daemon updates `HEAD` to `v0.7.3`, then writes a *new* commit recording the rollback action (so history is preserved).
12. Lock released.

Total wall-clock target: < 5s for typical rollbacks.

## 5. Failure modes and recovery

- **Daemon crash mid-commit.** On restart, daemon reads `.agentic/wal/` and either finishes the in-flight commit (if it had completed step 9) or rolls back partial object writes. Refs are atomic file renames; they cannot be torn.
- **Postgres unavailable during commit.** Commit fails. Pending segment-streamer entries are buffered to disk; they apply on reconnect. The agent application is unaffected — we don't sit in the agent's write path; segment streaming is async.
- **Disk full.** Commits fail. CLI surfaces the error clearly and suggests `agentic gc` (post-MVP).
- **MCP server unreachable.** Commit fails with a clear error indicating which server is unreachable; user can re-try or temporarily disable that tool's fingerprinting.
- **Reverse migration missing.** Rollback aborts. User chooses to write one, accept data loss (`--accept-data-loss`), or abort.

## 6. Security model (MVP)

The daemon runs as the same user as the application. There is no authentication on the socket beyond filesystem permissions. The object store is unencrypted at rest. Secrets are hard-rejected at `put_raw` per [ADR-0013](../adr/0013-secret-scanner.md) — the scanner runs both a curated pattern set and a Shannon-entropy heuristic, and any match returns `Error::SecretDetected` without writing the blob.

Post-MVP, we add:

- TLS for remote daemons.
- Snapshot signing via Sigstore.
- Per-branch ACLs for hosted deployments.
- Encrypted object store backends for S3.

## 7. The seams for v1.1 and beyond

The architecture deliberately admits these expansions without a rewrite:

- **More memory backends.** Add another implementation of the `MemoryAdapter` trait (Mem0, Zep, Letta). Snapshot quality is documented per backend.
- **Remote object store.** Implement `ObjectStore` for S3 / GCS / Azure Blob. Daemon then optionally pushes/pulls.
- **Hosted SaaS.** Front the daemon with a TLS-terminating gateway; add OIDC auth; add multi-tenant namespacing in the object store.
- **A web UI.** A read-only HTTP API on the daemon exposes the object graph; a separate Next.js or SolidJS front end consumes it.
- **More framework adapters.** CrewAI, AutoGen, LlamaIndex all get their own `Agentic*Checkpointer` modules in the Python SDK.
- **Eval integration.** Other tools (LangSmith, Braintrust) can read our diff API to ground evals against specific snapshots. We're a data source for them, not a competitor.

## 8. What this architecture explicitly is not

- It is not a memory database. The customer's Postgres remains the system of record.
- It is not a runtime orchestrator. We don't schedule agent execution.
- It is not an observability platform. We don't capture inference traces unless an external integration asks us to.
- It is not an MCP host. We talk to MCP servers; we don't run them.

Keeping these things out is the discipline that makes the rest tractable.

---

See [snapshot-model.md](./snapshot-model.md) for object semantics and [ADR-0001](../adr/0001-architecture-foundations.md) for the decision rationale.
