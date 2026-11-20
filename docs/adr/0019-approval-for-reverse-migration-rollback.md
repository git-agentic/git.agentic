# ADR-0019: Approval for Reverse-Migration Rollback

**Status:** Accepted
**Owner:** Toni Bergholm
**Date:** 2026-11-20
**Amends:** [ADR-0014](./0014-destructive-rollback-approval-gate.md) (the approval gate now also covers rollbacks that run reverse migrations)
**Threat model:** [`git.agentic-threat-model.md`](../../git.agentic-threat-model.md) TM-002, plain-rollback residual (TM-001 shares the per-operation authorization question, which stays open)
**Relates to:** [ADR-0012](./0012-socket-peer-authentication.md) Decision 2 (`peer_uid`), [ADR-0002](./0002-substrate-and-supercommit.md) Decision 5 (bounded rollback for destructive migrations)

## Context

ADR-0014 gates `Request::Rollback { accept_data_loss: true }` behind an operator approval token.
It leaves a plain rollback (`accept_data_loss: false`) ungated.
The 2026-11-20 threat-model reconciliation recorded the consequence in TM-002: an allowlisted socket peer, which in the sidecar shape is the adversarial worker, can send a plain `Rollback`.
The daemon then runs every reverse migration that is not marked `IRREVERSIBLE` and restores memory, with no approval token.
A reverse migration drops columns, tables and indexes.
Whether or not it carries the `IRREVERSIBLE` marker, it is a schema change on the per-tenant database that the worker could trigger at will.

ADR-0012 limits who can talk to the daemon.
It does not limit which of those peers may run a schema change.
The owner decided that any rollback that runs reverse migrations needs the ADR-0014 approval token, from any peer.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | A non-dry-run rollback whose plan includes at least one reverse migration passes the ADR-0014 gate, whether or not `accept_data_loss` is set. | The schema change is the dangerous operation, not the flag that acknowledges it. |
| 2 | Without an approval key, such a rollback is refused, and the error names `--approval-key-file`. | Fail closed, as ADR-0014 Decision 4. |
| 3 | If `--approval-key-file` is not given, the daemon uses `<repo>/.agentic/approval.key` when that file exists. | The operator needs no extra flag, and the CLI and daemon share one default location. |
| 4 | The `agentic` CLI attaches a token to rollback requests on its own when it can read the key. | The operator's commands stay the same. A socket client without the key cannot run reverse migrations. |
| 5 | A token is bound to a scope, `DataLoss` or `ReverseMigration`, by domain-separating the signed input. The token text format is unchanged. | One token must not authorize both modes: a reverse-migration approval replayed with `accept_data_loss` would switch off the `IRREVERSIBLE` check. |

## Decisions

### Decision 1: Reverse migrations pass the ADR-0014 gate

Once the daemon knows the plan, it checks whether the plan includes at least one reverse migration.
That means the live schema version differs from the target's and the backend reports at least one migration to reverse.
If so, and the request is not a dry run, the request passes the same gate as an `accept_data_loss` rollback.
The token format, the HMAC binding to the target commit and the peer UID, the five-minute window and the single-use ledger are the ADR-0014 ones, unchanged.

The gate runs before any side effect.
For a plain reverse-migration rollback, the down-migration files are loaded (read-only) just before the gate, so a missing or `IRREVERSIBLE` file is refused before a token is consumed or an `accepted` audit event is written.
With `accept_data_loss=true` the gate runs earlier, at the start of the request, so there the token is consumed and the event written before the files are loaded.
The code that builds the plan only reads: it resolves the ref, loads the target, checks the prompts tree and queries the backend for the live schema version and the pending migration names.
The gate sits right after that, before the reverse migrations run, the memory restore, the prompts write-back and the forward-record.
A refused request changes nothing.

A request with `accept_data_loss: true` already passed the gate at the start.
It is not checked a second time, so one token covers both reasons and is consumed once.

Rollbacks with no schema change are unchanged.
That includes a rollback whose target has no `schema_version`, and one whose target schema equals the live schema.

Audit.
The gate emits the existing `RollbackForcedDataLoss` event, with the same `decision` values.
An accepted event also carries `scope`, `data_loss` or `reverse_migration`, the scope the token verified under.
The event gains a `reason` field: `accept_data_loss` for the ADR-0014 trigger and `reverse_migration` for this one.
Events from before this ADR carry no `reason` and mean `accept_data_loss`.
An alert on forced data loss should filter on `reason = "accept_data_loss"`, so routine operator rollbacks that run reverse migrations do not trip it.
An alert on rejected attempts, `decision = "rejected_*"`, should cover both reasons.

Dry runs.
A dry run reports that approval will be required, as an extra line in the plan output, when the plan includes a reverse migration.
A dry run without a token is not refused, so an operator can preview.
A dry run that carries a token still verifies it and does not consume it, as for `accept_data_loss` today.

Wire errors.
The refusals reuse the ADR-0014 codes: `approval_key_not_configured`, `approval_no_peer_uid`, `approval_required`, `approval_token_malformed`, `approval_token_expired`, `approval_token_invalid_signature` and `approval_token_replayed`.
All are non-retryable validation errors.
The messages name both triggers.

### Decision 2: Fail closed without a key

If no approval key is configured, a rollback that runs reverse migrations is refused with `approval_key_not_configured`.
The message says to start `agenticd` with `--approval-key-file <path>`, or to place a 32-byte key at `<repo>/.agentic/approval.key`.
There is no override flag.

A daemon running with `--insecure-allow-any-uid` has no attested peer UID, so no token can bind to a peer.
Such a rollback is refused with `approval_no_peer_uid`, as ADR-0014 already does for `accept_data_loss`.
A deployment that wants reverse migrations must run with `--allowed-uid`.

### Decision 3: Key discovery

If `--approval-key-file` is not given, the daemon uses `<repo>/.agentic/approval.key` when that file exists.
When the flag is given, the flag wins and the default file is ignored.
A missing default file means no key, so Decision 2 applies.

The file is read with the checks ADR-0014 and its 2026-11-20 amendment set.
A file with any group or other mode bit set aborts startup with an error that names the path and says to `chmod 600` it.
A key that is not exactly 32 bytes aborts startup.
An empty or unreadable file leaves the daemon running without a key.

### Decision 4: The CLI attaches a token

`agentic rollback` looks for a key: the `--key-file` flag if given, else `<repo>/.agentic/approval.key` if that file exists.
The flag already exists for token issuance (`--approval`), and the rollback command reuses it.
A key named with `--key-file` must work, or the command fails.
A default file the CLI cannot use, empty or the wrong length, gets a warning on stderr and no token.
The warning goes to stderr so `--json` output stays parseable.
(The daemon runs keyless on an empty file and refuses to start on a wrong-length one.)

If it has a key and the user supplied no token, the CLI pins the target.
It resolves the ref with a `ResolveRef` request, refuses a result that is not exactly 64 lowercase hex characters, and sends that hash, not the ref name, in both the preview and the applied request.
The token, the preview and the applied commit therefore name the same commit, even if a branch moves in between.

Tokens are minted as late as possible.
The preview, which is a dry run, carries no auto-minted token.
After the operator confirms, the CLI mints a fresh token for the pinned hash and the CLI process's effective UID, which is the UID the daemon reads from the socket, and attaches it to the applied request only.
A `--dry-run` request carries none either.
The one exception is a destructive preview (`--accept-data-loss`): the daemon refuses that without a token before it reaches the dry-run branch, so the preview gets its own freshly minted token, which a dry run does not consume.
The CLI signs the applied request only if the preview it just got reported that the plan needs approval.
The rollback output carries a new field for this, `approval_required: bool`, set when the plan includes a reverse migration or the request set `accept_data_loss`.
The field is additive: it defaults to `false` when absent, so older daemons and clients are unaffected.
Without this rule, a forward migration that lands between the preview and the apply would be reversed under a token minted for a plan the operator never saw.
With the rule, the daemon's gate refuses that apply and the operator previews again.
With `--yes` no plan is shown, but the CLI still sends the preview request silently, when it holds a key, to read the field.

The CLI also checks who it is talking to.
In a flow that may auto-sign, every request (resolving the target, the preview, the applied request) goes over a connection whose peer uid, read from the kernel, is the uid that owns the key file.
If a check fails, the CLI gives up on signing: it warns on stderr and carries on without a token, and the daemon's gate refuses if one was needed.
With `--accept-data-loss` the applied request is signed after a successful preview even if the reply omits `approval_required`, as an older daemon's would.
Both sides open the key file with `O_NONBLOCK`, and check that the opened handle is a regular file.
The auto-discovered default path is also opened with `O_NOFOLLOW`, so a symlink swapped in after the discovery check is never followed, and a symlink at the default path is skipped (CLI, with a warning) or treated as no key (daemon).
A key named explicitly (`--approval-key-file`, `--key-file`, the issuer) is an operator choice and follows symlinks, because Kubernetes and Cloud Run secret volumes expose files through them.
The CLI skips a default key path that is a symlink, FIFO or other special file, with a warning, and the daemon treats such a default path as no key, so a special file cannot block either.
A key named explicitly must be a regular file (the CLI fails; the daemon warns and runs without a key, as for an unreadable file).
It reads at most 33 bytes of the key file, as the daemon does.

A token passed explicitly with `--approval-token` is sent as given, on both requests, and the target is left as typed.
If the CLI finds no usable key, it sends no token, and the daemon decides whether one is needed.

The CLI mints whenever it has a key, not only when a reverse migration is coming.
The daemon ignores a token it does not need.
A token is consumed only when the gate passes on a non-dry-run request.

This also lets an operator who holds the key run `--accept-data-loss` without minting a token by hand.
The flag is still required.

### Decision 5: Tokens are bound to a scope

One token format now covers two modes, so a token issued for an ordinary reverse-migration rollback could be replayed with `accept_data_loss=true`.
The daemon would then load the down migrations with the `IRREVERSIBLE` check off.
Approval tokens therefore carry a scope.

The scope is part of the signed input, not of the token text.
The text stays `<unix_ts>:<hex_hmac>`.
A `DataLoss` token signs exactly the ADR-0014 input, `git.agentic/rollback-approval/v1:<commit>:<uid>:<ts>`, so every token minted by an older CLI is a `DataLoss` token and keeps verifying.
A `ReverseMigration` token signs the same fields under the domain string `git.agentic/rollback-approval/v1/reverse-migration`.
The two domain strings differ at a fixed position before any variable field, so no commit, uid or timestamp can make the inputs equal.

Verification:

- An `accept_data_loss` request needs a `DataLoss` token.
- A plain reverse-migration rollback accepts either scope, because a data-loss approval also covers the weaker action.
- The single-use ledger keys on the decoded tag as before.

The CLI mints a `DataLoss` token for `--accept-data-loss` requests and a `ReverseMigration` token otherwise.
The `agentic rollback --approval` issuance mode takes `--scope reverse-migration|data-loss`.
The default is `data-loss`, so existing callers keep getting the tokens they got before.
An operator who hands a worker a token for an ordinary rollback should use `--scope reverse-migration`, because a `data-loss` token also authorizes `accept_data_loss`.
The daemon's missing-token error says so: for a reverse-migration rollback it recommends `--scope reverse-migration`, and for `accept_data_loss` it recommends `--scope data-loss`.
Both errors keep the wire code `approval_required`.

## Consequences

**Positive:**

- A worker without the key cannot run reverse migrations.
  The TM-002 plain-rollback residual closes for that case.
- The operator's CLI keeps working with no new steps, as long as the key sits at the default path or is passed with `--key-file`.
- No new wire fields, error codes or ledger.
  The change is one more call site for the ADR-0014 gate.

**Negative:**

- Breaking for deployments that ran token-less rollbacks with reverse migrations.
  They now need an approval key on the daemon and a token from the caller.
  The daemon must also run with `--allowed-uid`.
- The broken-prompt demo creates a key and starts the daemon with `--allowed-uid`.
  Its current rollback has no schema change, so it consumes no token.
- Any process under the same UID that can read the key can mint tokens.
  ADR-0014's amendment says this already.
  The mode check only excludes other UIDs.
- The Python SDK sends no token, so an SDK client cannot run reverse migrations.
  That is intended, because the SDK is what the worker uses.

**Not covered:**

- The token authorizes the target commit, not a specific migration list, because ADR-0014 binds approval to the target commit.
  A rollback to that commit reverses whatever is above it at apply time.
  The CLI's preview rule (Decision 4) narrows the window, but binding approval to the plan would need a token format change.
- A rollback with no schema change still restores memory without a token.
  Per-operation authorization for the other requests, `ReadObject` among them, stays an open decision in TM-001 and TM-002.
- The ledger is per process, as in ADR-0014.
