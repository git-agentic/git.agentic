# git.agentic

Pre-MVP "Git for agent behavior": atomic, reversible snapshots of the
`(code + prompts + tools + model + memory + schema)` tuple that
determines how an AI agent acts. Rust workspace (`agentic-core`,
`agentic-memory`, `agentic-proto`, `agentic-cli`, `agenticd` daemon)
plus a Python SDK (`sdk/python/agentic`) with a LangGraph integration.
The daemon talks to clients over a local Unix socket using
length-prefixed `agentic-proto` frames; storage is a content-addressed
object store (filesystem today, GCS planned per ADR-0004) plus
Postgres+pgvector for agent memory.

## Auth shape

There is **no end-user authn/authz**. The daemon is a single-tenant,
single-user process bound to a Unix socket under the user's runtime
directory; trust boundary = filesystem permissions on that socket.
Relevant primitives:

- `agenticd` Unix-socket listener (`crates/agenticd/src/server.rs`) —
  authority is "can `connect(2)` the socket". No per-message auth.
- `ObjectStore` / `GcsObjectStore` (`agentic-core::store`,
  `gcs_store.rs`) — writes are addressed by SHA-256 content hash;
  callers can write arbitrary bytes under any hash they compute.
- `MemoryAdapter` (Postgres+pgvector) in `agentic-memory` — connection
  string comes from env / config; daemon assumes the DB it's pointed
  at is exclusively its own.
- Python SDK `Client` (`sdk/python/agentic/client.py`) — trusts daemon
  responses; no signature verification on returned Commit objects.

## Threat model

Primary risk: a malicious or compromised agent process writes
attacker-controlled bytes (prompts, tool definitions, memory rows,
model IDs) into a Commit that a human later trusts and rolls back to,
silently re-introducing prompt-injection / data-poisoning payloads.
Secondary: secrets in agent state (API keys in prompts, tokens in tool
configs) get persisted into the object store and leak via backups or
the future GCS bucket. Tertiary: the 2PC commit path (blobs → Commit
blob → Git ref) corrupts the ref graph on partial failure, breaking
the "atomic rollback" guarantee that is the entire product promise.
Out of scope: multi-tenant isolation, network-exposed daemon.

## Project-specific patterns to flag

- **Writes to `ObjectStore` / `GcsObjectStore` that bypass the
  documented secret scanner.** CLAUDE.md mandates the daemon scan
  every blob for high-entropy / token-shaped strings before writing;
  the scanner is not yet implemented, so any new `put_blob`-style
  callsite is a place a future scanner must be wired in.
- **Reordering of the 2PC commit staging** in `agenticd` (must be
  blobs → content-hash collection → Commit blob → Git push → ref
  update). Any code path that updates a Git ref before the Commit
  blob is durable in the object store breaks rollback atomicity.
- **Framework-specific fields leaking into the `Commit` struct**
  (`agentic-core/src/object.rs`, `commit.rs`). Per ADR-0002/0003 the
  Commit object is the platform API contract — LangGraph- or
  Executor-specific fields there are a backwards-compat trap.
- **Postgres calls that mock or stub snapshot/restore.** Snapshot uses
  logical decoding, advisory locks, and pgvector storage; any test or
  code path that pretends to snapshot without a real Postgres is
  silently incorrect.
- **`unwrap()` / `expect()` on the daemon's request path** without a
  `// SAFETY:` or `// INVARIANT:` comment — a panic in `agenticd`
  aborts an in-flight commit and can leave the 2PC staging half-done.
- **MCP server surface** (`crates/agenticd/src/mcp.rs`) — if/when
  exposed beyond the local socket, treat every tool argument as
  untrusted; today it inherits the socket's trust assumption.

## Known false-positives

- `examples/langgraph-rollback/` — demo agent, fixture prompts and
  the deliberately-broken prompt in `demo.cast` / `tests/` are
  intentional "bad" inputs for the broken-prompt demo, not real
  secrets or live attack payloads.
- `examples/langgraph-rollback/docker-compose.yml` — hardcoded
  Postgres credentials on port 54322 are demo-only; the compose file
  is never deployed.
- `sdk/python/agentic/_framing.py` — hand-rolled length-prefixed
  framing over the Unix socket is intentional (matches the Rust
  `agentic-proto` wire format); flagging it as "reinventing a
  protocol" is noise.
- `.wolf/` directory — OpenWolf session metadata (`anatomy.md`,
  `cerebrum.md`, `buglog.json`, `memory.md`). Plain-text by design,
  no secrets expected, not part of the product surface.
- `crates/agentic-core/src/gcs_store.rs` — GCS object store is being
  pulled forward from v2+ per ADR-0004; partial / unused code paths
  there are work-in-progress, not dead code.
