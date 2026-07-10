# Issue #118 — Socket availability hardening — design

**Date:** 2026-07-10
**Issue:** [#118](https://github.com/git-agentic/git.agentic/issues/118) (deferred remainder of 2026-07-09 security-audit finding #5, decision recorded on #109)
**Status:** Approved by Toni in brainstorming session 2026-07-10

## Context and motivation

Audit finding #5 (medium, lowest of six): authorized socket peers can perform
low-cost availability attacks against `agenticd`. Frame *size* is capped
(`agentic-proto::framing::MAX_FRAME_BYTES`), and the cheapest slice — a
read-idle timeout on the connection loop — shipped in-map
(`server.rs::READ_IDLE_TIMEOUT`, 30s). Everything else was deferred to #118
with a revisit trigger of "threat model widens."

This work is **proactive backlog work** — the trigger has not fired. The v1.0
threat model still holds: a co-located Cloud Run sidecar over a Unix socket
whose only peer is the operator's own Coding worker at an allowlisted UID.
Toni chose to implement the **full issue scope anyway** (all four bullets,
including per-UID machinery) rather than a globals-only slice.

## What ships

1. **Global + per-UID connection caps** in the accept loop
   (`spawn_local` is currently unbounded, `crates/agenticd/src/main.rs:358`).
2. **Per-UID request rate budget** in the connection loop.
   Note: the connection loop is serial per connection
   (read → dispatch → write → loop, `server.rs:198`), so in-flight-per-UID
   is already bounded by the connection cap; the *rate* budget is the only
   genuinely distinct control from the issue's bullet 2.
3. **`commit_lock` queue-depth bound + observable gauge** — waiters currently
   queue silently and unboundedly.
4. **Write-idle deadline** complementing the shipped read-idle timeout.

## Decisions made during brainstorming

| Decision | Choice | Alternatives rejected |
|---|---|---|
| Scope | Full issue scope incl. per-UID | Globals-only; structure-for-per-UID-ship-global |
| Observability | Structured `tracing` events, `target: "agenticd::limits"` | `metrics` crate facade (new dep → ADR); Prometheus endpoint (new TCP listener on a Unix-socket-only daemon) |
| Config surface | CLI flags with defaults | Hardcoded constants; preset `--limits` profiles |
| Mechanism | Hand-rolled, tokio primitives only | `governor` crate (new dep → ADR, keyed store sized for thousands of keys we don't have); backpressure/stall semantics (recreates the silent-wedge failure mode the issue exists to eliminate) |

**No wire-protocol change, no new dependencies, no ADR required.** New error
codes ride inside the existing always-retryable `Concurrency` class —
additive per ADR-0010 Decision 2.

## Architecture

New module **`crates/agenticd/src/limits.rs`** owns all admission-control
state and policy. Consumed by the accept loop (`main.rs`) and the connection
loop (`server.rs`). `DaemonState` gains a `limits` field.

### `LimitsConfig` — new CLI flags, all with defaults

| Flag | Default | Meaning |
|---|---|---|
| `--max-connections` | 64 | Global concurrent-connection cap |
| `--max-connections-per-uid` | 16 | Per-UID concurrent-connection cap |
| `--rate-per-uid` | 200 | Requests/sec per UID (token bucket, burst 2× rate) |
| `--commit-queue-depth` | 8 | Max requests queued-or-executing on `commit_lock` |
| `--write-idle-secs` | 30 | Deadline on writing a response frame |
| `--read-idle-secs` | 30 | Promotion of the existing hardcoded `READ_IDLE_TIMEOUT`; default unchanged |

Zero/invalid values are rejected loudly at startup. The resolved config is
logged once at info level so incident logs begin with the limits in force.
Limits are static per process — reload means bouncing the daemon, same as
the ADR-0013 scanner allowlist.

### Components

- **`ConnGate`** — global + per-UID connection counters. `try_admit(uid)`
  returns an RAII guard (decrements on drop) or a rejection. Checked in the
  accept loop immediately after the ADR-0012 UID-allowlist check.
- **`RateBucket`** — hand-rolled per-UID token bucket (capacity, refill rate,
  last-refill instant; injectable clock for tests). Checked in the connection
  loop after envelope parse, before dispatch.
- **Commit-queue slots** — `tokio::sync::Semaphore` with
  `commit_queue_depth` permits fronting `commit_lock`. Dispatch arms that
  take `commit_lock` — Commit, Rollback, and Diff (which holds it only
  briefly for its ref snapshot); ReadObject does not take the lock — first
  `try_acquire` a slot; failure is an immediate structured rejection.
  The bound therefore covers everything that can queue on `commit_lock`,
  not just write-path requests.
  The permit is held for the queued + lock-held duration, so the bound
  covers both. An atomic depth counter alongside is the observable gauge.
  Centralised in one `DaemonState` helper (e.g. `acquire_commit_slot()`)
  so all arms share semantics and the existing double `check_shutdown()`
  discipline (before queuing, after acquiring) stays intact.

### Insecure mode

Under `--insecure-allow-any-uid` the UID carries no auth meaning, but
`SO_PEERCRED` still reports the real peer UID. Per-UID accounting keys on
that observed UID regardless, so budgets behave identically in both modes.

## Data flow and error handling

### Accept path (`main.rs`)

```
accept → peer_cred → UID allowlist (existing) → ConnGate.try_admit(uid)
  ├─ admitted  → spawn_local(handle_connection), guard rides with the task
  └─ rejected  → tracing warn (agenticd::limits) + drop socket
```

No structured reply on gate rejection — no frame has been read, so there is
no `correlation_id`. Log-and-close, identical to the existing UID-rejection
arm and the ADR-0010 Decision 4 precedent. Reconnecting over a Unix socket
is cheap.

### Request path (`server.rs`)

```
read frame (read-idle deadline, existing)
→ parse envelope
→ RateBucket.check(uid)
  ├─ ok        → dispatch
  └─ exhausted → Response::Error { class: Concurrency,
                 code: "rate_budget_exhausted", retryable: true }
                 — connection stays open, loop continues
→ write response under write-idle deadline
  └─ elapsed   → log-and-close (the write path itself is broken;
                 no reply is possible)
```

The rate check runs after envelope parse so the rejection carries the
correlation id. A rate-limited client keeps its connection.

### Write path (dispatch arms taking `commit_lock`)

```
commit_slots.try_acquire()
  ├─ ok   → depth gauge +1 → await commit_lock → existing logic unchanged
            → guard drop: gauge −1, permit released
  └─ full → Response::Error { class: Concurrency,
            code: "commit_queue_full", retryable: true }
```

### Error surface

Two new opaque codes in the existing `Concurrency` class:
`rate_budget_exhausted`, `commit_queue_full`. The Python SDK already maps
this class to `AgenticConcurrencyError` with the `retryable` flag and treats
codes as opaque tokens — **zero SDK changes**. Connection-cap and write-idle
enforcement are close-only, consistent with their unattributable positions.

### Invariants preserved

- **2PC staging order (ADR-0002 D3) untouched.** Limits gate *entry* to the
  write path; a handler holding `commit_lock` cannot be interrupted by any
  new mechanism. Write-idle closes happen strictly after dispatch returns,
  i.e. after the lock is released.
- **Lifecycle drain untouched.** `Lifecycle::drain` acquires `commit_lock`
  directly and never competes for queue slots.

## Observability

Structured tracing events, `target: "agenticd::limits"`:

| Event | Level | Fields |
|---|---|---|
| connection rejected (cap) | warn | `peer_uid`, `global_conns`, `uid_conns`, which cap tripped |
| rate budget exhausted | warn | `peer_uid`, `correlation_id` |
| commit queue full | warn | `peer_uid`, `correlation_id`, `depth` |
| commit queue depth change | debug | `depth`, `peer_uid` (slot acquire/release) |
| write-idle close | warn | `peer_uid`, `write_idle_secs` |

Warn events are the alertable signals; the debug gauge gives depth history.
In the sidecar deployment these land in Cloud Logging.

## Testing

Every pattern has shipped precedent in this codebase:

- **Unit (`limits.rs`):** token-bucket refill/burst/per-UID isolation with an
  injectable clock; `ConnGate` global-vs-per-UID cap interaction; RAII guard
  drop decrements.
- **Unit (`server.rs`):** mirror of `dispatch_diff_blocks_on_commit_lock` —
  hold `commit_lock`, saturate the slot semaphore, assert the next
  write-path request gets `Concurrency`/`commit_queue_full` with
  `retryable: true` instead of parking. Rate-budget rejection keeps the
  connection open (a later request after refill succeeds). Write-idle:
  mirror of `idle_connection_is_dropped_after_deadline` with a peer that
  stops reading, injectable deadline.
- **Integration (`crates/agenticd/tests/`):** real-socket test alongside
  `peer_auth_integration.rs` — open connections past the cap, assert the
  excess are dropped while existing connections keep working.
- **Config plumbing:** flag parsing → `LimitsConfig` defaults; zero/negative
  values rejected loudly at startup.

## Demo and performance impact

The demo path (`run-demo.sh`) is a single serial client — defaults of
64 connections / 200 req/s / queue depth 8 are orders of magnitude above
what it exercises; behavior is unchanged. Per-request overhead is a
token-bucket check and an atomic or two, well inside the <5 ms p99 write
overhead budget (snapshot-model §9).

## Out of scope

- Per-tenant budgets beyond the allowlisted-UID set — no distinguishing
  tenant exists until the #109 revisit trigger fires.
- Any metrics stack beyond `tracing` (facade crate, Prometheus endpoint).
- Dynamic limit reload — bounce the daemon.
- Backpressure/stall semantics — explicitly rejected; rejections must be
  loud and observable.
