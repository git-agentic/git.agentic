# The Snapshot Model

**Status:** Design — not yet implemented
**Last updated:** 2026-05-19

This document specifies the technical heart of `git.agentic`: how we represent an agent's behavioral state as content-addressed objects, and how we snapshot and restore that state atomically.

## 1. The object model

We model behavior as a Merkle DAG of typed objects. Every object is identified by the BLAKE3 hash of its canonical serialization. There are four object kinds.

### 1.1 Blob

A blob is an opaque byte sequence. We use blobs for:

- Prompt template bodies
- Tool manifest JSON
- Model version strings
- Schema migration definitions
- Memory rows that don't fit cleanly into the segment model (see §3)

```
type Blob = bytes
hash(Blob) = blake3(blob_bytes)
```

### 1.2 Tree

A tree is a sorted map from name → (object_kind, hash). Trees group related blobs and other trees. We use trees for, e.g., "the set of prompts active in this snapshot," "the set of tools," etc.

```
type Tree = sorted_map<String, (Kind, Hash)>
hash(Tree) = blake3(canonical_serialize(tree))
```

### 1.3 Segment

A segment is a memory-specific object kind. It represents a content-addressed chunk of a memory table (rows + their associated embeddings + their metadata). Segments are immutable. A memory snapshot is a Merkle tree over segments. See §3 for the snapshot algorithm.

```
type Segment = {
  table: String,
  rows: Vec<Row>,
  embeddings: Vec<Embedding>,
  metadata: Map<String, Value>,
}
hash(Segment) = blake3(canonical_serialize(segment))
```

### 1.4 Commit

A commit is the top-level object. It holds the tuple.

```
type Commit = {
  parent: Option<Hash>,
  author: String,
  timestamp: u64,
  message: String,
  code_sha: GitSha,
  prompts: Hash,              // → Tree of Blobs
  tools: Hash,                // → Tree of (manifest Blob + version Blob) per tool
  model: Hash,                // → Blob containing provider:model:rev
  memory_snapshot: Hash,      // → Tree of Segments
  schema_version: SemVer,
}
hash(Commit) = blake3(canonical_serialize(commit))
```

The commit hash *is* the agent version. Two commits with the same tuple have the same hash. This is the property that makes branching, merging, and deduplication work.

## 2. The object store layout

Objects live on disk under `.agentic/objects/`. Layout:

```
.agentic/
├── HEAD                            # ref to current branch
├── refs/
│   ├── heads/
│   │   ├── main                    # → commit hash
│   │   └── ab-test-new-prompt      # → commit hash
│   └── tags/
├── objects/
│   ├── ab/
│   │   └── 12cd34ef...zst          # objects sharded by first 2 hex chars
│   └── ...
├── config.toml
└── pack/                           # packed object archives (created by GC; not in MVP)
```

Objects are stored compressed with zstd. The first two hex characters of the hash become the directory; the rest is the filename. This matches Git's familiar layout and lets us reuse battle-tested tooling concepts (`fsck`, packfile semantics, etc.).

## 3. Memory snapshots: the hard part

Memory snapshots are where this design earns its keep. We need to snapshot a potentially-very-large Postgres+pgvector store in <2 seconds with bounded storage growth, while the agent is actively reading and writing.

### 3.1 The naive approach (and why it fails)

The obvious approach is "run `pg_dump` on every commit." This is wrong because:

- `pg_dump` of a 10M-row table takes minutes, not seconds.
- Storage cost grows linearly with snapshot count — every snapshot is a full copy.
- It's not atomic with respect to non-Postgres state (we'd snapshot Postgres while the prompt tree was changing).

### 3.2 The segment-based approach

We partition each memory table into immutable segments of bounded size (default 64MB). Segments are:

- **Content-addressed.** A segment's hash is determined by its contents. Two identical segments are stored once.
- **Append-only.** Writes go to an active "head" segment; when it fills, it's sealed (hashed and persisted) and a new head opens.
- **Indexed by row primary key range.** This lets us answer "where does row X live now" without scanning.

A memory snapshot is then a **manifest** — a Merkle tree mapping `(table, pk_range) → segment_hash`. Snapshotting becomes:

1. Pause writes for the snapshot duration (typically <100ms via Postgres advisory lock).
2. Capture the current segment manifest. Most segments are unchanged from the previous snapshot — we just reference them.
3. For the active (unsealed) head segment, copy-on-write its contents into a new sealed segment.
4. Publish the new manifest hash; resume writes.

Storage cost grows roughly with **changed data**, not with snapshot count. A typical snapshot adds one new sealed head segment (≤64MB) plus a few KB of manifest. A snapshot every minute for a year costs gigabytes, not terabytes.

### 3.3 Coexistence with native pgvector

We do not replace pgvector's storage. We sit *next to* it. The Postgres tables remain the source of truth for live reads and writes; the segment store is a parallel write-through cache that records every committed row in content-addressed form.

Implementation: a Postgres logical decoding plugin (or a trigger-based fallback) streams every committed row insert/update/delete into the segment writer in real time. The segment writer batches and seals segments. Snapshots are taken against the segment store, not against Postgres directly — which is what makes them fast.

Tradeoff: writes incur a small overhead (a network hop to the daemon, then a serialize+hash). We measure this and document it. Reads are unaffected.

### 3.4 Restoring memory

Restoring a memory snapshot is:

1. Read the manifest at the target snapshot.
2. Diff against the current manifest: identify segments to add, remove, and replace.
3. Stream the differing rows back into Postgres inside a single transaction:
   - Truncate ranges that the target snapshot doesn't include.
   - Insert ranges that it does (using `INSERT ... ON CONFLICT` for safety).
4. Validate row counts and a sampled checksum.
5. Commit.

Restore time is bounded by the diff size, not the table size. A rollback that only touches the last 1000 inserted rows is fast.

### 3.5 Schema compatibility

Memory schemas evolve. A snapshot taken at schema v3.1 may not restore cleanly into a database running schema v3.2 if columns were added with non-null constraints, or if embedding dimensions changed.

We require every schema change to ship a pair of migrations: forward (`up`) and reverse (`down`). The commit object records the schema version. On rollback:

1. Read the target commit's `schema_version`.
2. Read the current schema version from the live database.
3. If different, run reverse migrations from current → target, in order.
4. Then restore the memory data.

If a reverse migration is missing or marked irreversible (e.g., a dropped column with no backup), rollback **fails loudly** rather than producing a half-restored database. The user is told exactly which migration is the problem and can choose to write a reverse, accept data loss, or abort.

This is the same discipline Rails/Django/Ecto have enforced for a decade. We just demand it for agent memory.

## 4. Snapshot atomicity across the tuple

A commit captures six dimensions and we need them coherent. The order of operations matters.

```
commit() {
  1. acquire daemon lock                         // serialize commits
  2. snapshot prompts → hash_p                   // read from disk; fast
  3. snapshot tools → hash_t                     // query MCP servers, hash manifests
  4. read model version string → hash_m
  5. take memory snapshot → hash_s, version_v    // §3
  6. get current git head → code_sha             // shells to `git rev-parse`
  7. build commit object {parent, ..., hash_s}
  8. write commit → object store
  9. update branch ref
 10. release daemon lock
}
```

The memory snapshot (step 5) is the only step with a meaningful pause-writes window, and that window is sub-100ms in normal operation. The rest are read-only against state that the agent doesn't mutate during a commit.

We do not require the agent to be paused during commit. If the agent writes new memory rows during steps 2–4, those rows simply land in the *next* commit. This is acceptable because:
- We are not promising "snapshot of all in-flight work."
- The commit captures the state as of the moment step 5 began.

## 5. Rollback semantics

`agentic rollback <commit>` is the headline command. Pseudocode:

```
rollback(target_commit) {
  1. validate that target_commit exists in this repo
  2. compute the diff: (current_commit, target_commit)
     - prompts diff (which files change)
     - tools diff (which MCP servers / versions change)
     - model diff (current → target version string)
     - memory diff (segment delta)
     - schema diff (current_version → target_version, with migration plan)
  3. if any reverse migration is missing or irreversible: abort with a clear message
  4. show the plan to the user; require confirmation (unless --yes)
  5. acquire daemon lock; pause agent traffic if a runtime hook is registered
  6. apply schema reverse-migrations
  7. restore memory from target snapshot (§3.4)
  8. write prompts to disk; update tool pins
  9. update HEAD to target_commit
 10. resume traffic; release lock
}
```

Rollbacks are **forward-recorded**: rolling back from commit C to commit A produces a new commit C' whose state matches A but whose parent is C. This preserves the history (you can always see when and why a rollback happened) without resurrecting the old hash chain.

## 6. Branches and the agent-version graph

Branches are pointers to commits, exactly as in Git. A `branch` is useful for:

- **A/B testing**: branch from `main`, change the prompt, run traffic against both versions, compare evaluation metrics, then either fast-forward `main` or discard the branch.
- **Hotfix isolation**: roll back production to a known-good branch while you investigate `main`.
- **Long-running experiments**: a memory-heavy fine-tune evaluation that should not pollute the main timeline.

Merges across branches are deliberately *not* supported in MVP. Merging two divergent memory snapshots is a research problem (conflict resolution over append-only segments) and we explicitly defer it. If you want to "merge in" a prompt change from a branch, you cherry-pick it: a separate command that creates a new commit on the target branch with just the prompt dimension replaced.

## 7. Diffs

`agentic diff <a> <b>` produces a structured behavioral diff:

```
diff main^ main

prompts/
  - system_prompt.txt   (modified, 3 lines changed)
tools/
  - search.mcp          (version 1.4.0 → 1.4.1)
model:
  (unchanged: anthropic:claude-opus:2026-05-01)
memory:
  + 1,247 rows in table `episodes`
  ~ 8 rows updated in table `user_facts`
schema:
  (unchanged: 3.1.2)
```

The CLI also supports `--prompt-diff`, `--tools-diff`, etc., to scope to a single dimension. Diffs are deterministic and content-addressed: re-running produces byte-identical output.

## 8. What we explicitly do not store

To keep the object store bounded and the privacy story clean:

- We do not capture full model weights. We capture the version string only.
- We do not capture inference traces by default. (We expose hooks for a future eval/observability integration to do so externally.)
- We do not capture environment variables or secrets. The daemon refuses to write any value whose key matches a secrets pattern.
- We do not capture user PII from memory rows. Memory snapshotting is opt-in per table; a table marked `pii: true` is fingerprinted but its raw rows live only in Postgres, not in the segment store.

The PII story will need refinement with a real security review. Documented now so we don't forget.

## 9. Performance targets (MVP)

| Operation | Target | Conditions |
|---|---|---|
| `commit` | < 2s | 1M-row pgvector table, 100 new rows since last commit |
| `rollback` | < 5s | Same scale, rolling back 10 commits |
| `diff` | < 1s | Same scale |
| Write overhead | < 5ms per row | p99 latency added to agent writes by segment streaming |
| Snapshot storage | < 2× changed data | Amortized over many snapshots |

These are aspirational and will be re-tuned as we benchmark. The point is to commit to numbers publicly so we don't ship a fast demo and a slow product.

## 10. Open implementation questions

- **Logical decoding vs. triggers.** Logical decoding is cleaner but requires Postgres configuration (`wal_level=logical`, replication slots). Triggers work on any Postgres. Lean: logical decoding with triggers as a fallback, both gated on `agentic init` detecting capabilities.
- **Segment size tuning.** 64MB is a guess. Real workloads may want 16MB or 256MB. Make it configurable.
- **Compression.** zstd-3 by default; zstd-19 for cold packfiles created during GC.
- **Concurrent commits.** Daemon serializes commits with an exclusive lock today. A future refinement is per-table locking, but it's not needed at MVP scale.
- **Network protocol.** Daemon ↔ SDK over Unix socket using length-prefixed protobuf. Remote daemon (over TLS) is post-MVP.

---

See [ADR-0001](../adr/0001-architecture-foundations.md) for the architectural decisions backing this design.
