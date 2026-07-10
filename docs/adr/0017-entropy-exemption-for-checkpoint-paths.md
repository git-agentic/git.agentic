# ADR-0017: Entropy-Exemption for Declared Checkpoint Paths

Status: Accepted
Owner: Toni Bergholm
Date: 2026-07-10
Amends: ADR-0013 (secret scanner)

## Context

The ADR-0013 secret scanner runs a pattern detector and a Shannon-entropy
heuristic (threshold 4.5 bits over base64-alphabet runs ≥ 20 bytes) on every
Blob before it touches the object store.
The LangGraph checkpointer (`sdk/python/agentic/langgraph.py`) serialises
checkpoints as msgpack and base64-wraps them into a JSON envelope stored
under the commit's prompts tree at `__langgraph__/<thread-hash>/checkpoint.json`.
Base64 payloads sit near 6 bits of entropy, so the heuristic fires on every
checkpoint commit — the broken-prompt demo has been unable to commit a single
LangGraph step since the scanner landed on 2026-05-21 (issue #124 gap 5).

ADR-0013 Decision 4's blob-hash allowlist cannot address this:
checkpoint content changes every run, so there is no stable hash to allowlist.
The scanner runs inside the object store's `put`/`put_raw`, which has no path
context — blob paths live in the Tree object, known only to commit staging.

## Decision 1: skip only the entropy heuristic, only for declared prefixes

The entropy heuristic yields 100 % false positives on encoded checkpoint
payloads, so it carries zero signal for them.
Commit staging (`agentic-core::commit::stage_blob_tree`) — the layer that
knows each blob's tree path — stages blobs whose path matches a configured
exempt prefix with `ScanPolicy { skip_entropy: true }`.
Pattern rules (AWS keys, PEM blocks, and the rest of
`scanner_patterns.rs`) still run on every blob, exempt or not.
The exemption applies uniformly to the prompts and tools trees; paths are
opaque strings and get no framework-specific meaning in the daemon.

## Decision 2: configuration and default

The prefix list is daemon configuration:
`--scanner-exempt-entropy-prefix` (repeatable), defaulting to
`__langgraph__/`, following the `--scanner-allowlist` precedent.
Prefixes must be non-empty relative paths; the daemon refuses to start
otherwise.
An empty list (passing the flag zero times is not possible once a default
exists; operators can pass a never-matching prefix to disable) — rather,
operators who want full scanning everywhere run with
`--scanner-exempt-entropy-prefix "__disabled__/"`.

## Decision 3: observability

Every applied exemption emits a structured tracing event
(`target: "agentic_core::scanner"`, level INFO, with the blob path),
keeping the daemon's tracing-only observability discipline (issue #118).

## Consequences and accepted risk

A secret embedded inside serialised agent state under an exempt prefix is no
longer entropy-caught (pattern rules still apply).
Accepted because the scanner is a guardrail against accidental secret
commits by a trusted, peer-authenticated client (ADR-0012), not an
adversarial control — a hostile same-UID client could already bypass it by
naming any path under the exempt prefix, or by not using the daemon at all.
The SDK contract does not change: no wire changes, no framework-specific
Commit fields (ADR-0003 Decision 3 holds).
Rollback forward-recording re-stages target-commit blobs and therefore
applies the same exemption; without it, rolling back across a checkpoint
commit would be rejected by the scanner.
