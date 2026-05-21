# ADR-0013: Secret Scanner as a `put_raw` Pre-Hook

**Status:** Proposed
**Date:** 2026-05-21
**Deciders:** Toni
**Closes:** [`git.agentic-threat-model.md`](../../git.agentic-threat-model.md) TM-009 (the secret scanner advertised in `CLAUDE.md` / `AGENTS.md` / `docs/architecture/overview.md` / `docs/product/competitive-brief-entire.md` is not implemented in code today).
**Relates to:** [ADR-0001](./0001-architecture-foundations.md) Decision 6 ("Apache 2.0, batteries-included by default"), [ADR-0002](./0002-substrate-and-supercommit.md) Decision 3 (2PC staging order) and Decision 6 (storage abstraction), [ADR-0011](./0011-objectstore-async-trait-shape.md) (the trait this hook will follow into v1.1).

## Context

`git.agentic` ships secret hygiene as a foundational product claim. Four tracked files state this verbatim today (post-PR-1 they forward-reference this ADR):

- [`CLAUDE.md`](../../CLAUDE.md) §"What not to do": *"Per [ADR-0013], the daemon hard-rejects blobs containing matched secret patterns or high-entropy substrings at `put_raw` time, returning `Error::SecretDetected`. Don't bypass the scanner; fix the input."*
- [`AGENTS.md`](../../AGENTS.md): identical bullet.
- [`docs/architecture/overview.md`](../architecture/overview.md): *"Secrets are not yet machine-rejected: [ADR-0013] specifies a `put_raw`-time pattern + entropy scanner..."*
- [`docs/product/competitive-brief-entire.md`](../product/competitive-brief-entire.md): *"our daemon's pattern + entropy scanner is specified by [ADR-0013] and ships in v1.0 (PR-3 of the hardening sprint)."*

The 2026-05-21 threat model surfaced this as TM-009 critical:

> The "secret scanner" advertised in `CLAUDE.md` / `AGENTS.md` / `docs/architecture/overview.md` is not implemented in code. `git grep` for `scan|entropy|reject_token` across `crates/agentic-core/` and `crates/agenticd/` returns nothing. Going public with a documented-but-fake invariant is the worst outcome — operators who trust the docs would commit secrets expecting them to be caught.

The implementation half is gated by this ADR. The question this ADR settles is not "should we ship a scanner" — the docs commit us to that — but rather:

1. **Where does it run?** Closer to the storage layer or closer to the request handler?
2. **What does it detect?** Just patterns? Just entropy? Both?
3. **What happens on a hit?** Hard reject, soft warn, or operator-gated?
4. **How are false positives handled?** Allowlist mechanism shape.
5. **What's the failure mode for ambiguous input?** Test fixtures, base64 in code, etc.

The constraints:

- **Backwards-compatible storage trait.** The scanner has to be added in v1.0 without changing the `ObjectStore` trait shape that v1.1's [ADR-0011](./0011-objectstore-async-trait-shape.md) is going to redesign. A `put_raw` pre-hook in the trait's only consumer keeps the trait stable.
- **No new heavy dependencies.** The workspace already pins `regex` transitively via tracing-subscriber and serde-related crates. Adding a `gitleaks`-binary subprocess would inflate the sidecar image, increase per-put latency by milliseconds, and require ship-along binary management. A native Rust implementation using `regex::RegexSet` and a single entropy pass over `&[u8]` is sufficient for v1.0.
- **Performance budget per CLAUDE.md.** [`CLAUDE.md`](../../CLAUDE.md) §"Performance targets" sets commit < 2s and write overhead < 5 ms p99. The scanner must fit inside that envelope per put. A 64 MiB segment manifest (the largest blob in the demo path) at 5 ms/MiB yields 320 ms — within budget. Larger blobs trip the entropy heuristic's long-run check naturally.
- **Hard-rejection language in the docs.** The four mentioning files all say "hard-rejects". An override flag would walk that back; this ADR commits to no override in v1.0.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **The scanner lives in `crates/agentic-core::scanner` and runs as a pre-hook inside `agentic-core::store::put_raw`.** Every backend (`FsObjectStore`, `GcsObjectStore`, future variants) inherits the check without per-backend modification. | The lowest layer with the right object identity. Putting the hook here means the daemon, the SDK, and any future direct consumer of the object store all get the same enforcement. |
| 2 | **Detection strategy is pattern + entropy.** A curated `Vec<TokenPattern>` of high-precision regexes runs in a single `regex::RegexSet` pass; a Shannon-entropy heuristic flags contiguous runs ≥ 20 chars from the base64 alphabet with entropy > 4.5 bits/char. | The literal CLAUDE.md / overview claim is "matched secret patterns or high-entropy substrings". Patterns alone miss custom token formats; entropy alone has too many false positives. Both together with a tight allowlist is the operating point. |
| 3 | **Hits return `Error::SecretDetected { hits: Vec<Hit> }` and the put never reaches the backend.** No override flag in v1.0. | The "hard-rejects" language commits us. An override would be the kind of escape hatch operators turn on and forget. If a fix-the-input loop is too painful in practice, that's the v1.1 conversation. |
| 4 | **Allowlist is blob-SHA256-scoped, loaded from `.agentic/scanner-allowlist.toml` at daemon startup.** Each entry whitelists exactly one blob's content hash — not a pattern, not a regex. | Hash-scoped allowlist cannot accidentally whitelist similar future content. Adding "this specific test fixture is OK" stays scoped to that fixture. The cost is verbosity, paid by the operator who hits the allowlist (which should be rare). |
| 5 | **Pattern set lives in `crates/agentic-core::scanner_patterns` as a `const &[TokenPattern]` array.** Each entry has `name`, `regex`, `description`. Patterns are reviewed at PR-time, not at runtime. | Patterns are part of the trust contract. A runtime-loaded pattern file would invite a "load this PCRE from disk" footgun. Compile-time patterns let the type system enforce reviewable change. |
| 6 | **Failure-injection tests at the put_raw boundary.** On scanner reject, the assertion is `store.has(hash) == false` AND no backend-side write was attempted. | Per [ADR-0002](./0002-substrate-and-supercommit.md) Decision 3, every 2PC boundary needs failure-injection tests. The scanner adds a new boundary at the entry to `put_raw`; this gets the same discipline. |
| 7 | **Out of scope for v1.0:** streaming-blob scanning, regex-based allowlist patterns, operator-supplied custom patterns, scan-on-read. | Streaming is a v2+ concern (large blobs > 100 MiB per ADR-0011 D4). Regex allowlist invites the same footgun as runtime-loaded patterns. Custom patterns are a v1.1+ feature behind an explicit ADR. Scan-on-read is YAGNI — the put boundary is where secrets land. |

## Decisions

### Decision 1 — Scanner lives in `agentic-core::scanner`, runs in `put_raw`

The new module sits at `crates/agentic-core/src/scanner.rs` with two friends:

- `crates/agentic-core/src/scanner_patterns.rs` — the pattern array (see Decision 5).
- `crates/agentic-core/src/store.rs::put_raw` — the call site (the existing function, modified to call the scanner).

The shape of the trait method that backends implement does not change. The `put_raw` function on `ObjectStore` is a default method (or a thin wrapper) that:

1. Calls `scanner::scan(bytes, &allowlist)`.
2. If the result has any hits, returns `Err(Error::SecretDetected { hits })`.
3. Otherwise delegates to the backend's actual put implementation.

The allowlist is part of the `ObjectStore`'s construction state. `FsObjectStore::open(dir)` and `GcsObjectStore::new(...)` learn an optional `&Path` to the allowlist file (or take a pre-loaded `Arc<Allowlist>`). Default: empty allowlist.

This decision deliberately does NOT route the scanner through the daemon's `agenticd` crate. Operators who use the Python SDK to call `put_raw` directly (a v1.1 platform-integrator path) get the same enforcement. The daemon is one consumer of `agentic-core`, not the privileged location for secret hygiene.

### Decision 2 — Pattern + entropy detection

Two complementary detectors share one scan pass:

**Pattern detector.** A `regex::RegexSet` compiled from the entries in `scanner_patterns.rs`. The set matches all patterns in one linear pass over the blob bytes (UTF-8 lossy — the scanner is byte-oriented and tolerates invalid UTF-8). On a match, the responsible pattern's name and the matched byte range are recorded as a `Hit { kind: HitKind::Pattern(name), offset, length }`.

The starting pattern set (subject to PR-time review):

| Pattern name | Regex shape | Source / rationale |
|---|---|---|
| `github_pat_classic` | `gh[poshu]_[A-Za-z0-9_]{36,}` | GitHub PAT format (`ghp_`, `gho_`, `ghs_`, `ghu_` prefixes) |
| `aws_access_key` | `AKIA[0-9A-Z]{16}` | AWS access key ID format |
| `anthropic_api_key` | `sk-ant-(api|admin)\-[a-zA-Z0-9_-]{40,}` | Anthropic API and admin key format |
| `openai_api_key` | `sk-(proj-)?[a-zA-Z0-9]{48,}` | OpenAI standard and project key format |
| `stripe_live_key` | `(sk|pk)_live_[a-zA-Z0-9]{24,}` | Stripe live keys |
| `gcp_service_account` | `"type"\s*:\s*"service_account"` | GCP service-account JSON marker |
| `private_key_pem` | `-----BEGIN (RSA \|EC \|OPENSSH \|DSA \|)PRIVATE KEY-----` | PEM private-key headers |

The list is intentionally small and high-precision. Adding a regex with a false-positive rate above ~1/100k blobs in the demo path is a no-go; broad recall is the entropy detector's job.

**Entropy detector.** A single forward pass over the blob bytes that:

1. Identifies contiguous runs of ≥ 20 characters from the base64 alphabet `[A-Za-z0-9+/=_-]`.
2. For each run, computes Shannon entropy: `H = -Σ p_i log2 p_i` over the byte frequencies.
3. Emits a `Hit { kind: HitKind::HighEntropy, offset, length }` for any run with `H > 4.5` bits/char.

The 4.5 bits/char threshold sits above the entropy of natural-language base64-encoded text (which tends to be 3.5–4.2) and below near-uniform random output (which approaches 6.0 for the full 64-char alphabet). The 20-char minimum prevents false positives on short hex IDs and short timestamps that show up frequently in test code.

Both detectors share the same `&[u8]` input and the same scan pass. The scanner returns once it has the full hit list — it does not short-circuit on the first hit, so an operator who runs into a multi-hit blob sees every problem at once and can scrub them in one revision.

### Decision 3 — Hard reject, no override flag

A scan with any hits returns:

```rust
return Err(Error::SecretDetected { hits });
```

The caller's commit (via `agenticd`'s `commit::execute`) propagates this as a structured response that the CLI surfaces with file/byte offsets the operator can use to scrub.

There is **no `--allow-secrets-this-once` override flag in v1.0.** Three reasons:

1. The four tracked doc files explicitly say "hard-rejects". An override would walk back the language.
2. Override flags accumulate trust debt. Once shipped, they get used in CI, in cron jobs, and in scripts. Removing them later is harder than not adding them.
3. The legitimate "this specific blob is OK" case has a better fit: blob-SHA256 allowlist (Decision 4). It scopes the exception to the exact content, not to the act of writing.

If operator pain from hard rejection turns out to be too high (e.g., dozens of false positives per day in a real platform-partner deployment), the v1.1 conversation is whether to widen the allowlist mechanism, tighten the patterns, or both. Not whether to add an override.

### Decision 4 — Blob-SHA256-scoped allowlist

The allowlist file is TOML at `.agentic/scanner-allowlist.toml`:

```toml
# Each entry whitelists exactly one blob's SHA256.
# Add an entry only when the scanner rejected a blob that is genuinely
# safe (test fixture, public test key, documented constant in a regex
# fixture, etc.) and you have inspected the bytes manually.

[[ignore]]
blob_sha256 = "9f4e1c8e1234567890abcdef1234567890abcdef1234567890abcdef12345678"
reason = "anthropic-style test fixture in agentic-cli tests"
added_by = "toni"
added_date = "2026-05-21"

[[ignore]]
blob_sha256 = "deadbeefcafe..."
reason = "..."
```

At daemon startup the file is parsed into a `BTreeSet<Hash>` and held in `Arc<Allowlist>`. The scanner consults it when computing the final reject decision:

1. Run patterns + entropy in one pass.
2. Compute the blob's SHA256.
3. If any hits AND the blob's SHA256 is in the allowlist, return success with no hits.
4. Otherwise return success with the hit list or, if the hit list is empty, success with no hits.

Each allowlist entry whitelists exactly one blob. Adding "this specific test fixture is OK" cannot accidentally whitelist similar future content with a slightly different shape. The verbosity (one entry per allowed blob) is the deliberate cost — it forces an operator to look at each entry, understand what they're allowing, and document the reason.

The `reason`, `added_by`, and `added_date` fields are TOML-format niceties, not enforced by the scanner. They exist so a future operator reading the allowlist can audit it.

If `.agentic/scanner-allowlist.toml` does not exist, the allowlist is empty. The daemon does not error on a missing file (that would block first-time setups); it logs a `tracing::debug!` line noting "no allowlist file at {path}; no allowlist entries".

### Decision 5 — Patterns are compile-time, not runtime-loaded

The pattern array lives at `crates/agentic-core/src/scanner_patterns.rs`:

```rust
pub const PATTERNS: &[TokenPattern] = &[
    TokenPattern {
        name: "github_pat_classic",
        regex: r"gh[poshu]_[A-Za-z0-9_]{36,}",
        description: "GitHub PAT format (ghp_, gho_, ghs_, ghu_ prefixes)",
    },
    TokenPattern {
        name: "aws_access_key",
        regex: r"AKIA[0-9A-Z]{16}",
        description: "AWS access key ID format",
    },
    // ... etc per Decision 2
];
```

The `&[TokenPattern]` is compiled into a `regex::RegexSet` once at `Scanner::new()` and reused across all puts. Adding a new pattern is a code change, reviewed at PR time. There is no runtime path to load patterns from disk.

This decision rules out an alternative shape where operators could supply their own patterns via config. The footgun there is well-documented: PCRE-style runtime patterns invite catastrophic-backtracking input, regex injection, and pattern-set drift between environments. `git.agentic`'s threat model treats the scanner pattern set as part of the trust contract, the same way it treats the wire-protocol schema. Both belong to the codebase.

Pattern updates ship in normal point releases. If a new high-value token format appears (e.g., a new cloud provider's key shape), a contributor opens a PR that adds the pattern, includes a known-good fixture in the test, and the pattern lands the next release.

### Decision 6 — Failure-injection tests at the put_raw boundary

Per [ADR-0002](./0002-substrate-and-supercommit.md) Decision 3, every 2PC boundary gets failure-injection tests. The scanner adds a new boundary at the entry to `put_raw`. The minimum test set:

1. **Pattern hit, FsObjectStore.** `put_raw(kind, &[..token..])` returns `Err(Error::SecretDetected)` and `store.has(hash)` is `false`. No file appears on disk.
2. **Pattern hit, GcsObjectStore (mocked via `httpmock`).** Same assertion, plus: no HTTP POST was issued to the mock GCS endpoint. The scanner short-circuits before the network round-trip.
3. **Entropy hit, FsObjectStore.** A synthetic 24-character base64-shaped blob with > 4.5 bits/char produces `HitKind::HighEntropy` and rejects identically.
4. **Allowlist suppression.** A blob that would hit a pattern, but whose SHA256 is in a fixture allowlist, succeeds. `store.has(hash)` is `true`.
5. **Multi-hit reporting.** A blob containing both a GitHub PAT and an AWS key produces two `Hit` entries in the error. The operator sees both.
6. **Empty allowlist behaviour.** Daemon starts with no allowlist file. All patterns + entropy still fire. `tracing::debug!` line "no allowlist file at {path}" is observable.

The test crate is `crates/agentic-core/src/scanner.rs::tests` for the unit-level checks; the GcsObjectStore integration test uses the existing `httpmock` fixture under `crates/agentic-core/tests/`.

### Decision 7 — Explicit out-of-scope for v1.0

- **Streaming-blob scanning.** v1.0 reads the full blob into memory before scanning. The 64 MiB segment manifest target is well within memory limits. Streaming scanning is a v2+ concern (see [ADR-0011](./0011-objectstore-async-trait-shape.md) D4 — streaming put/get is also v2+).
- **Regex-based allowlist patterns.** Same footgun as runtime-loaded scan patterns; explicitly deferred.
- **Operator-supplied custom patterns.** Behind an explicit future ADR. v1.0 ships the curated set only.
- **Scan-on-read.** YAGNI. The scanner runs at the put boundary; once a blob is in the store, it has been scanned. Scanning at read time only matters if the store is shared across trust zones, which is out of scope for v1.0 per ADR-0004's per-instance object store.

## Consequences

**Positive:**

- TM-009 closes. The four tracked doc files become honest: a scanner exists in code that matches the contract they describe.
- Operator trust surface tightens. A platform-partner integrator reading [`CLAUDE.md`](../../CLAUDE.md) §"What not to do" sees a guarantee they can verify by running the test suite.
- The scanner lives at the storage layer, so the SDK and the CLI both inherit it without per-consumer code paths.
- Blob-SHA256 allowlist is verbose but auditable. A future operator reading the file can determine, for each allowed blob, why it's allowed.
- Compile-time pattern set keeps the pattern surface part of the trust contract reviewed at PR time, not at runtime.

**Negative:**

- Per-put latency increases. The 5 ms/MiB budget covers the demo path, but a deployment that puts very large blobs (close to the segment manifest cap) will see the scanner as a non-trivial fraction of put time. Documented; not a deal-breaker.
- Pattern coverage is finite. The starting set covers the top six or seven token formats; custom formats slip through unless the entropy detector catches them. Operators in production may want to PR new patterns over time. This is the normal Rust release cadence — not a problem, but worth noting.
- Hard reject without override means a daemon stuck in a "your fixture trips the entropy detector" loop forces an allowlist entry. The friction is intentional (Decision 3); operators who feel acutely should escalate via a v1.1 conversation, not a workaround.
- The allowlist file is operator-managed. A misconfigured allowlist (over-broad, untrusted entries) is a way to silently disable the scanner for a class of blobs. The structured logging at startup (which lists how many entries are loaded) is the observability primitive that catches this in production.

**Risks to revisit:**

- The 4.5 bits/char threshold is a tuning choice. The first deployment under real load may surface false-positive rate problems. The risk is documented; the threshold lives in `scanner.rs` as a `const ENTROPY_THRESHOLD: f64 = 4.5;` so re-tuning is a small PR.
- The pattern set is the high-precision side of the trade-off. If a major cloud provider rotates their key format mid-2026 (e.g., GitHub's PAT format change), the existing pattern fails open until a new pattern PR lands. Mitigation: the entropy detector still flags the new shape if it's high-entropy + base64-ish, so degraded coverage rather than zero coverage.
- The compile-time pattern array means platform-partner integrators cannot ship their own patterns without forking. If real demand surfaces for operator-supplied patterns, the v1.1 ADR addressing this needs to balance the runtime-pattern footgun against the integrator's actual need.

See also: [`docs/superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md`](../superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md) §"ADR-0013" (the sprint design that frames this ADR), and `git.agentic-threat-model.md` TM-009 (the row this ADR closes).
