# Pre-public-release hardening sprint — design

**Date:** 2026-05-21
**Owner:** Toni
**Status:** Design (pre-implementation)
**Trigger:** [`git.agentic-threat-model.md`](../../../git.agentic-threat-model.md) flagged 10 findings (TM-001 through TM-010) that all must be addressed before flipping the repo private → public on GitHub.
**Scope basis:** [/brainstorming session, 2026-05-21](../../../) — user-confirmed: all 10 before flip, ADRs-first sequencing, ship the scanner.

## Why

The repo is mechanically ready to flip public (history-rewrite landed; cleanup PRs #56–#59 merged). Two threat-model findings would be embarrassing or actively harmful to ship public uncorrected:

1. **TM-009 — the "secret scanner" advertised in `CLAUDE.md`, `AGENTS.md`, `docs/architecture/overview.md` §5, and `docs/product/competitive-brief-entire.md` does not exist in code.** Going public with a documented-but-fake invariant is the worst outcome — operators who trust the docs would commit secrets expecting them to be caught.
2. **TM-001 + TM-003 — every connection to the daemon's Unix socket is treated as trusted**, but the prioritized v1.0 deployment (ADR-0004 Cloud Run sidecar) puts a fully adversarial Coding worker on the other end of that socket. Without peer authentication, the worker can enumerate every object via `ReadObject` and stage forged Commits with arbitrary content.

The remaining eight findings (TM-002, TM-004, TM-005, TM-006, TM-007, TM-008, TM-010) are smaller but the user has elected to close all of them before the public flip rather than ship a known threat-model gap.

This sprint is a focused hardening pass. It does not add product features; every change closes a specific threat-model row.

## Constraints

- One conceptual change per PR (project convention, [`CONTRIBUTING.md`](../../../CONTRIBUTING.md)).
- ADR before implementation for architectural changes (project convention, [`CLAUDE.md`](../../../CLAUDE.md)).
- Wire-protocol changes must preserve backwards compatibility per [ADR-0002](../../adr/0002-substrate-and-supercommit.md) — the `Commit` object is the platform API contract.
- No new long-running daemon dependencies beyond rustls / reqwest / sqlx already in the workspace.
- All Rust code in the daemon: no `unwrap()` outside tests without a `// SAFETY:` or `// INVARIANT:` comment.
- `cargo clippy --workspace --all-targets -- -D warnings` stays green.
- The broken-prompt demo (`examples/langgraph-rollback/scripts/run-demo.sh`) keeps working end-to-end.

## Out of scope

Explicitly deferred to v1.1 or later:

- Cryptographic peer attestation beyond `SO_PEERCRED` UID (SPIFFE / mTLS over Unix socket).
- Tenant-isolation review of the shared `gcs://<your-bucket>/<tenant>` convention — belongs to platform-partner integration work, not this sprint.
- Ongoing scanner regex / pattern maintenance — ships with a curated set; ongoing tuning is a separate process.
- Pulling [ADR-0010](../../adr/0010-wire-protocol-error-model.md) / [ADR-0011](../../adr/0011-objectstore-async-trait-shape.md) forward.
- Web UI, hosted SaaS, additional memory backends, additional framework adapters.

## Sprint shape (Approach A — ADRs first, then implementation)

Four new ADRs land before any implementation PR. Each ADR is one focused architectural decision. Implementation PRs reference their ADR.

### ADR wave

#### ADR-0012: Socket peer authentication and Commit attestation
- **Closes:** TM-001, TM-003.
- **Decision 1:** `agenticd` reads peer credentials via `SO_PEERCRED` (Linux) on every socket accept. Rejects connections whose UID is not in the `--allowed-uid <UID>` allowlist. Daemon refuses to start in production if no allowlist is configured unless `--insecure-allow-any-uid` is explicitly passed (demo-only escape hatch).
- **Decision 2:** The `Commit` object gains an optional `peer_uid: Option<u32>` field. The dispatcher reads it from the accepted connection's peer credentials and threads it through `commit::execute` into `CommitInputs`. Anyone walking the tuple history can audit "who shaped this commit." `peer_pid` may be included alongside for forensics; not used in any decision logic.
- **Wire compatibility:** Additive `Option` field on the `Commit` JSON schema, backwards-compatible with v1.0 readers that don't know about it (per ADR-0002 D6 — Commit object is the platform API contract; additive extensions are allowed, never breaking ones).
- **Sidecar guidance:** v1.0 deployment with worker and operator both in the allowlist is acceptable; long-term the worker should run as a distinct UID from the operator.
- **Platform-API impact:** Documents in [ADR-0003](../../adr/0003-claude-agent-sdk-integration.md) §"framework-neutral SDK contract" that platform integrators must declare their peer UID at deployment time.

#### ADR-0013: Secret scanner as a `put_raw` pre-hook
- **Closes:** TM-009.
- **Decision 1:** New `agentic_core::scanner` module. `ObjectStore::put_raw` calls `scanner::scan(bytes)` before delegating to the backend. On any hit, returns a new `Error::SecretDetected { hits: Vec<Hit> }` error and the put never reaches the store.
- **Decision 2:** Detection strategy is **pattern + entropy**.
  - Pattern set: curated `Vec<TokenPattern>` of high-precision regexes — GitHub PATs (`ghp_`, `gho_`, `ghs_`, `ghu_`), AWS access keys (`AKIA[0-9A-Z]{16}`), Anthropic keys (`sk-ant-[a-zA-Z0-9-]{40,}`), OpenAI keys (`sk-[a-zA-Z0-9]{48}`), Stripe keys (`sk_live_…`, `pk_live_…`), GCP service-account JSON markers (`"type":\s*"service_account"`), PEM headers (`-----BEGIN (RSA |EC |OPENSSH |)PRIVATE KEY-----`).
  - Entropy heuristic: Shannon entropy `> 4.5` bits/char over contiguous runs of `≥ 20` chars from the base64 alphabet `[A-Za-z0-9+/=_-]`. Catches custom token formats the pattern set misses.
- **Decision 3:** Allowlist mechanism: `.agentic/scanner-allowlist.toml` at repo root. Entries are `[[ignore]] blob_sha256 = "..."` — scoped by exact blob hash, not by pattern. Adding "this specific test fixture is OK" cannot accidentally whitelist similar future content. No regex-based allowlist in v1.0.
- **Decision 4:** Hard reject only. No `--allow-secrets-this-once` override flag in v1.0. Operator scrubs the input.
- **Performance budget:** scanner runs inside the existing `spawn_blocking` wrapper in `agentic-core::store::put_raw`; budget `< 5 ms/MiB` (regex set + single-pass entropy scan; regex `RegexSet` for combined match).
- **Test discipline:** every pattern has a known-good fixture; the entropy heuristic catches a synthetic 24-char base64 secret; the allowlist suppresses a known-OK blob hash.

#### ADR-0014: Destructive-rollback approval gate
- **Closes:** TM-002.
- **Decision 1:** `Request::Rollback` grows an `approval_token: Option<String>` field. `rollback::execute` rejects any request with `accept_data_loss = true` AND no valid `approval_token`. Without an approval-key configured on the daemon (`--approval-key-file <path>`), `accept_data_loss = true` is *always* rejected (fail-closed default).
- **Decision 2:** Approval tokens are HMAC-SHA256 over the canonical bytes of `(commit_hash, requesting_peer_uid, timestamp)` with a key held by an out-of-band approver. `requesting_peer_uid` is the UID of the connection that will present the token at verification time (typically the worker UID in the sidecar shape) — the operator must know this UID at signing time, which in Cloud Run is fixed by the service config and is therefore knowable. Tokens are short-lived (`≤ 5 min` between `timestamp` and current time at verification). Replay prevention via the timestamp window; no anti-replay store needed.
- **Decision 3:** Every forced-data-loss rollback emits a structured `RollbackForcedDataLoss` audit event regardless of whether it succeeded (rejected attempts also emit, with the failure reason). The audit event includes `peer_uid`, `target_commit_hash`, and the rejection reason if applicable.
- **Sidecar guidance:** the approval-key is held by the operator CLI, not by the daemon or the worker. In the sidecar shape the worker cannot construct a valid token on its own.

#### ADR-0015: GCS workload-identity
- **Closes:** TM-007.
- **Decision 1:** `GcsObjectStore` learns to obtain tokens from the GCE metadata server (`http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token`) and cache them with refresh-before-expiry. Tokens are short-lived and audience-scoped to the per-tenant bucket.
- **Decision 2:** Authentication is modeled as a `GcsAuth` enum: `WorkloadIdentity` (default in production) or `StaticBearer(String)` (fallback for the local-Docker-compose demo where the operator runs `gcloud auth print-access-token`).
- **Decision 3:** Static bearer is **never** loaded from a file by the daemon; only from the env / a CLI flag explicitly tagged `--insecure-static-bearer`. The daemon refuses to load a static bearer if it can reach the metadata server (fail-secure preference).
- **Effect:** the long-lived bearer is removed from the daemon's process state entirely in production. A hostile process sharing the UID can no longer read a static token from env or files.

### Implementation wave (PR-1 through PR-6)

#### PR-1: Documentation honesty + `PgConfig` Debug redaction
- **Closes:** TM-009 (docs side, preliminary) + TM-010.
- **Files:**
  - `CLAUDE.md`, `AGENTS.md`, `docs/architecture/overview.md` §5, `docs/product/competitive-brief-entire.md`: update the existing "the daemon scans every blob…" claim to point at ADR-0013 ("v1.0 ships a pattern + entropy secret scanner per ADR-0013 that hard-rejects detected tokens at `put_raw` time"). The text reads accurately as a forward reference even if PR-3 slips.
  - `crates/agentic-memory/src/postgres.rs`: implement custom `Debug` for `PgConfig` that redacts the password. Add unit test `pgconfig_debug_redacts_password` asserting `format!("{cfg:?}")` does not contain the configured password substring.
- **Dependencies:** none. Lands first.

#### PR-2: Socket peer authentication + Commit attestation
- **Closes:** TM-001, TM-003. Implements ADR-0012.
- **Files:**
  - `crates/agenticd/src/main.rs:131`: accept loop reads `SO_PEERCRED` immediately on each accept; if UID isn't in the allowlist, log the rejection and close the connection.
  - `crates/agenticd/src/main.rs`: CLI flags `--allowed-uid <UID>` (repeatable) and `--insecure-allow-any-uid` (no-arg). Refuse to start in production if neither is set.
  - `crates/agentic-proto/src/lib.rs`: `Commit` object adds `peer_uid: Option<u32>`. Bump wire-compat note.
  - `crates/agenticd/src/server.rs::DaemonState`: connection handler carries the peer UID.
  - `crates/agenticd/src/commit.rs::execute`: write `peer_uid` into `CommitInputs` going to `agentic-core::stage_and_commit_with_now`.
  - `crates/agentic-core/src/commit.rs::CommitInputs`: gains `peer_uid: Option<u32>` field, threaded into the Commit blob.
- **Tests:**
  - Unit test: Commit object with `peer_uid` round-trips through JSON serde without changing the hash of older Commits without the field.
  - Integration test (Linux-only `#[cfg(target_os = "linux")]`): connect from a process running under a non-allowed UID via `setuid`; assert the daemon closes the socket.
- **Dependencies:** ADR-0012 merged.

#### PR-3: Secret scanner
- **Closes:** TM-009 (code side). Implements ADR-0013.
- **Files:**
  - `crates/agentic-core/src/scanner.rs` (new): `pub fn scan(bytes: &[u8]) -> ScanResult`; `struct Hit { kind: HitKind, offset: usize, length: usize }`; `enum HitKind { Pattern(&'static str), HighEntropy }`. Uses `regex::RegexSet` for combined pattern match (one pass).
  - `crates/agentic-core/src/scanner_patterns.rs` (new): `pub const PATTERNS: &[TokenPattern]` array. One row per pattern with `name`, `regex`, `description`.
  - `crates/agentic-core/src/store.rs::put_raw`: calls `scanner::scan` before delegating; returns `Error::SecretDetected { hits }` on hit.
  - `crates/agentic-core/src/lib.rs`: `Error::SecretDetected` variant.
  - `.agentic/scanner-allowlist.toml` loader at daemon startup (CLI flag `--scanner-allowlist <path>`, default `.agentic/scanner-allowlist.toml`); allowlist loaded into the `DaemonState`'s `Arc<ObjectStore>` wrapper.
- **Tests:**
  - One unit test per pattern, each with a known-good fixture string.
  - Entropy test: synthetic 24-char base64 input matches; 24-char repetitive `aaaa…` doesn't.
  - Allowlist test: blob with a matching pattern but its sha256 in the allowlist passes.
  - End-to-end: `put_raw` with a secret-bearing blob returns `Error::SecretDetected` and `store.has(hash)` is `false`.
- **Dependencies:** ADR-0013 merged. Independent of PR-2.

#### PR-4: Rollback approval gate + Unicode-normalized path validator
- **Closes:** TM-002, TM-008. PR-4's TM-002 part implements ADR-0014.
- **Files:**
  - `crates/agentic-proto/src/lib.rs::Request::Rollback`: adds `approval_token: Option<String>`.
  - `crates/agentic-core/src/approval.rs` (new): HMAC-SHA256 verifier; loader for the key file.
  - `crates/agenticd/src/main.rs`: CLI flag `--approval-key-file <path>`.
  - `crates/agenticd/src/rollback/mod.rs::execute`: reject `accept_data_loss=true` without a valid token; emit `RollbackForcedDataLoss` structured audit event on every attempt (success and failure).
  - `crates/agenticd/src/rollback/writeback.rs::validate_tree_entry_name`: extend to reject (a) non-NFC names, (b) NUL or any C0 control char, (c) any name that, when joined with the prompts directory and canonicalized, does not prefix-match the canonical prompts directory.
- **Tests:**
  - Rollback rejected with `approval_token: None` when `accept_data_loss=true`.
  - Rollback rejected with an expired token (5+ minutes old).
  - Rollback accepted with a fresh valid token; audit event emitted.
  - Path validator rejects NFKC-equivalent path-traversal (`prompts/\u{2024}\u{2024}/etc`), denormalized Unicode dots (`prompts/\u{0307}.\u{0307}/etc`), embedded NUL.
- **Dependencies:** ADR-0014 merged; PR-2 merged (so `peer_uid` is available to include in the HMAC signed payload).

#### PR-5: MCP hardening
- **Closes:** TM-005, TM-006. No new ADR.
- **Files:**
  - `crates/agenticd/src/mcp.rs::fingerprint_one`: read response with a 1 MiB byte cap (use `bytes_stream` + accumulator with early-exit, or reqwest's content-length check before reading); reject with a typed error if exceeded.
  - `crates/agenticd/src/mcp.rs::fingerprint_one`: schema-validate `result.tools` — must be an array, each element must have string `name` and object `inputSchema`. Reject with a typed error otherwise.
  - `crates/agenticd/src/mcp.rs`: per-server `BackoffState { consecutive_failures: u32, next_attempt: Instant }`. On `≥ 3` consecutive failures, skip the server for an exponentially-growing window (`60s × 2^(n-3)`, capped at 1 hour). Reset on success.
- **Tests:**
  - Oversized body fixture (2 MiB): rejected within the first 1 MiB read.
  - Malformed `result.tools` (object instead of array; missing `name`): rejected with typed error.
  - Backoff: after 3 timeouts, the next `fingerprint_all` call skips the server until the backoff window expires.
- **Dependencies:** none. Independent of PR-2/3/4. Can land any time after PR-1.

#### PR-6: GCS workload-identity + per-peer rate limit
- **Closes:** TM-007, TM-004. PR-6's TM-007 part implements ADR-0015.
- **Files:**
  - `crates/agentic-core/src/gcs_store.rs`: refactor authentication into `GcsAuth { WorkloadIdentity { cached_token: Mutex<Option<...>> }, StaticBearer(String) }`. Implement the metadata-server token fetch + refresh-before-expiry. Static bearer becomes an `--insecure-static-bearer` opt-in.
  - `crates/agenticd/src/server.rs::DaemonState`: per-peer-UID token bucket. Token bucket implementation: `BTreeMap<u32, RateLimitState>` keyed by peer UID. Default 100 ops/min, configurable via `--rate-limit-per-uid <ops/min>`. Exceeding returns a `RateLimited` response immediately without taking `commit_lock`.
- **Tests:**
  - GCS workload-identity path: mock metadata server via `httpmock`; assert token cached and refreshed before expiry.
  - Static-bearer fallback: still works when `--insecure-static-bearer` is passed and metadata server is unreachable.
  - Rate limit: 101st request within a minute returns `RateLimited` without `commit_lock` being held.
- **Dependencies:** ADR-0015 merged; PR-2 merged (rate limit keys on `peer_uid`).

### Sequence and parallelism

```
                ┌── ADR-0012 ──┬── PR-2 (peer auth + attestation) ──┬── PR-4 (rollback + path)
                │              │                                    │
PR-1 ──────────┤              ├── (concurrent)                     ├── PR-6 (workload-identity + RL)
                │              │                                    │
                ├── ADR-0013 ──┴── PR-3 (scanner) ──────────────────┤
                │                                                   │
                ├── ADR-0014 (gates PR-4) ──────────────────────────┤
                │                                                   │
                └── ADR-0015 (gates PR-6) ──────────────────────────┘
                                                                    │
                                                              PR-5 (MCP) — independent, lands any time after PR-1
```

ADRs can be in flight in parallel. Implementation PRs that depend on a single ADR can start as soon as that ADR merges, without waiting for other ADRs.

### Done definition

Sprint is complete and the repo is ready to flip public when **all** of the following hold:

1. ADR-0012, ADR-0013, ADR-0014, ADR-0015 merged to `main`.
2. PR-1 through PR-6 merged to `main`.
3. `agenticd` refuses to start without `--allowed-uid` unless `--insecure-allow-any-uid` is explicitly passed.
4. `agentic-core::scanner::scan` rejects all known-pattern fixtures and the entropy fixture; `put_raw` returns `Error::SecretDetected` and writes zero bytes to the backend on detection.
5. `Rollback { accept_data_loss: true, approval_token: None }` is rejected; with an expired token rejected; with a fresh token accepted; audit event emitted on every attempt.
6. `mcp::fingerprint_one` rejects a 2 MiB synthetic response within the first 1 MiB read.
7. `GcsObjectStore` succeeds against the GCE metadata mock; static-bearer path still works for the demo under `--insecure-static-bearer`.
8. `cargo clippy --workspace --all-targets -- -D warnings` green; `cargo test --workspace` green; `examples/langgraph-rollback/scripts/run-demo.sh` runs end-to-end on a fresh machine (still the long-standing A1 from the previous sprint).
9. [`git.agentic-threat-model.md`](../../../git.agentic-threat-model.md) updated: every TM-001 through TM-010 row gets a "Status: shipped in PR-N" note in the "Existing controls" column.
10. Final `git grep -i scanner` and `git grep -i 'peer_uid'` show the implementation matches every doc claim that mentions either.

### Calendar estimate

- 4 ADRs: ~1–2 days each to draft + review = ~3 days if reviewed in parallel, ~1 week if sequentially.
- PR-1: < 1 day.
- PR-2: 2–3 days (wire schema bump + `SO_PEERCRED` test fixtures).
- PR-3: 3–5 days (largest single piece of new code).
- PR-4: 2 days.
- PR-5: 1–2 days.
- PR-6: 2–3 days (token cache + refresh + httpmock).

**Total:** ~2.5–3 weeks sequential, ~2 weeks with 2–3 PRs in flight in parallel. The MVP 2026-08-11 target absorbs this comfortably.

## Risks and mitigations

- **`SO_PEERCRED` is Linux-specific.** The macOS-equivalent is `LOCAL_PEERCRED` via `getsockopt`. Demo path is Linux (Docker compose). For macOS-native dev, the daemon will require `--insecure-allow-any-uid`. Acceptable; flagged in ADR-0012.
- **Scanner false-positive rate.** Documented mitigation: blob-sha256-scoped allowlist. The pattern set is conservative (high-precision regexes only). The entropy heuristic threshold (4.5 bits/char over 20-char runs) is tuned to exclude common base64 in tests (short SHA-256 hashes are 64 chars but appear in code as named consts not in user-supplied prompts).
- **HMAC key distribution for ADR-0014.** v1.0 punts this to "operator manages the key file." A future ADR may introduce KMS-backed approval signing. Not in scope here.
- **Workload-identity availability assumption.** Outside GCP (e.g. dev on macOS), the metadata server is unreachable. The static-bearer fallback under `--insecure-static-bearer` preserves the demo workflow.

## Cross-references

- Threat model: [`git.agentic-threat-model.md`](../../../git.agentic-threat-model.md) (the input to this design).
- [`CLAUDE.md`](../../../CLAUDE.md) §"What not to do" — secret-scanning claim that this sprint makes honest.
- [ADR-0001](../../adr/0001-architecture-foundations.md) Decision 1 ("Tuple-as-version") — peer UID is added to the tuple element.
- [ADR-0002](../../adr/0002-substrate-and-supercommit.md) Decision 6 ("Storage abstraction") — `put_raw` pre-hook lives at this layer.
- [ADR-0003](../../adr/0003-claude-agent-sdk-integration.md) Decision 3 ("Framework-neutral SDK contract") — `peer_uid` is documented as a contract element.
- [ADR-0004](../../adr/0004-realtime-agenticd-for-executor.md) Decision 4 ("Loud-fail") — the rollback audit event integrates with this discipline.
