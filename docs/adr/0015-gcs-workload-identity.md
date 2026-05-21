# ADR-0015: GCS Workload-Identity for the Sidecar `agenticd`

**Status:** Proposed
**Date:** 2026-05-21
**Deciders:** Toni
**Closes:** [`git.agentic-threat-model.md`](../../git.agentic-threat-model.md) TM-007 (sibling worker reads agenticd's GCS bearer token via shared UID/mount, exfiltrates or overwrites the per-tenant bucket).
**Relates to:** [ADR-0002](./0002-substrate-and-supercommit.md) Decision 6 (storage abstraction; the `ObjectStore` trait can swap backends), [ADR-0004](./0004-realtime-agenticd-for-executor.md) Decisions 1–5 (sidecar topology, GCS-backed `ObjectStore`), [ADR-0012](./0012-socket-peer-authentication.md) (the peer-auth boundary that this ADR's threat model assumes is breached).

## Context

[ADR-0004](./0004-realtime-agenticd-for-executor.md) Decision 5 commits v1.0's sidecar deployment to a GCS-backed `ObjectStore`. The current `GcsObjectStore` implementation in `crates/agentic-core/src/gcs_store.rs` takes a `bearer_token: String` at construction time, holds it in process state for the lifetime of the daemon, and includes it in every `Authorization: Bearer <token>` HTTP header on every put/get.

The threat model classifies this as TM-007 high-priority:

> Process can read agenticd's env or config files. Read GCS bearer token, exfiltrate or overwrite bucket. Impact: confidentiality + integrity of all per-tenant GCS data. Existing mitigation: rustls-only, non-root UID 65532. If token is in env/file, worker UID can read it.

The threat assumption that ADR-0012 already partially closes — sibling worker is allowlisted but adversarial — is what makes TM-007 sharp. The worker shares the UID, the filesystem mount, and `/proc` visibility with the daemon in the v1.0 Cloud Run sidecar shape. Any long-lived secret in agenticd's process state is reachable by `/proc/<agenticd-pid>/environ`, `/proc/<agenticd-pid>/cwd/<config-file>`, or a simple `ls` on the shared mount.

The standard GCP fix is workload-identity: agenticd asks the GCE metadata server for a short-lived OAuth2 token, scoped to the per-tenant GCS bucket, refreshed before expiry. The token never appears in env or on disk; the metadata server is reachable only from inside the Cloud Run instance and is governed by GCP's IAM service-account binding.

The constraints:

- **The local-Docker-compose demo must still work.** No GCP metadata server is reachable from a developer laptop; the demo uses `FsObjectStore`, but a contributor exploring `GcsObjectStore` against a real bucket needs a path.
- **No new heavy dependencies.** The workspace already pins `reqwest` (rustls-tls) and `serde`; nothing else should be needed. No `google-cloud-rust` crate, no service-account-key-file parser, no separate auth library.
- **Stays compatible with the v1.1 ADR-0011 trait redesign.** The async-trait shape will inherit whatever shape this ADR settles for `GcsObjectStore`.
- **Fail-secure preference.** If the metadata server is reachable, the daemon should prefer it; loading a static bearer when workload-identity is available is a misconfiguration the daemon should refuse, not silently honor.

## Decision summary

| # | Decision | One-line rationale |
|---|---|---|
| 1 | **`GcsObjectStore` authentication is modeled as `enum GcsAuth { WorkloadIdentity(MetadataClient), StaticBearer(SecretString) }`.** Production deployments use `WorkloadIdentity`; the local-Docker-compose demo uses `StaticBearer` only under `--insecure-static-bearer`. | Single auth abstraction; the type system enforces that a deployment knows which path it's on. The `SecretString` wrapper prevents accidental `Debug` leaks the same way ADR-0012 does for peer-related state. |
| 2 | **`WorkloadIdentity` fetches tokens from `http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token` with the `Metadata-Flavor: Google` header.** Tokens are cached and refreshed eagerly before expiry. | This is the documented GCP metadata-server contract for Cloud Run workload-identity. No new dependency required; `reqwest` already handles the HTTP. |
| 3 | **Tokens are refreshed when the remaining TTL drops below 60 seconds.** Refresh is opportunistic on the next put/get call path; failures fall back to the still-valid current token until expiry. | Eager refresh inside the still-valid window absorbs metadata-server flakes without blocking puts. The 60-second buffer matches the GCP retry/timeout envelope for Cloud Run cross-container calls. |
| 4 | **The daemon refuses to load a static bearer when the metadata server is reachable, unless `--insecure-static-bearer` is explicitly passed.** Fail-secure default for accidental misconfigurations. | An operator who deploys to Cloud Run with both `--gcs-bearer` set AND the metadata server reachable has a configuration bug; the daemon makes it loud. |
| 5 | **The static-bearer path is preserved for the local-Docker-compose demo and for unit tests.** Contributors run `gcloud auth print-access-token` and pass the result via `--insecure-static-bearer "$TOKEN"`. The flag's name is self-documenting. | The demo discipline (broken-prompt flow under 5 min) requires a workable `GcsObjectStore` path off-GCP. The escape hatch makes it explicit that this is the non-production path. |
| 6 | **No service-account key files in v1.0.** Anyone who needs to authenticate from outside Cloud Run uses `gcloud auth print-access-token` to mint a token on demand. | Service-account-key JSON files are exactly the kind of long-lived secret TM-007 is about. v1.0 does not introduce them; v1.1 may add an explicit ADR if a deployment shape forces it. |

## Decisions

### Decision 1 — `GcsAuth` enum, `SecretString` for the bearer variant

The existing `GcsObjectStore::new(bucket, prefix, bearer_token)` signature changes:

```rust
pub struct GcsObjectStore { /* … */ auth: GcsAuth, /* … */ }

pub enum GcsAuth {
    WorkloadIdentity(MetadataClient),
    StaticBearer(SecretString),
}

impl GcsObjectStore {
    pub fn with_workload_identity(bucket: &str, prefix: &str) -> Result<Self>;
    pub fn with_static_bearer(bucket: &str, prefix: &str, bearer: SecretString) -> Result<Self>;
}
```

`SecretString` is a thin wrapper that implements `Debug` and `Display` as `"<redacted>"`. The same pattern as ADR-0012's audit-line redaction discipline. Adding a `secrecy` crate dependency is acceptable here (well-maintained, narrow purpose); if the project prefers to vend it in-house, a 20-line wrapper in `agentic-core::secret` covers the use case.

The selection of `GcsAuth` happens at daemon startup in `agenticd::main::open_object_store` based on CLI flags:

- `--object-store gcs --gcs-bucket <name>` (no auth flags) → tries `with_workload_identity`. If the metadata server is unreachable AND `--insecure-static-bearer` is not set, startup fails.
- `--object-store gcs --gcs-bucket <name> --insecure-static-bearer <token>` (or `... --insecure-static-bearer-file <path>`) → `with_static_bearer`. If the metadata server IS reachable, startup fails unless `--insecure-static-bearer-allow-on-gcp` is ALSO passed (Decision 4).

### Decision 2 — `MetadataClient` semantics

The metadata client makes HTTP GET requests to:

```
http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token
```

with the required `Metadata-Flavor: Google` header. The response shape is documented by GCP:

```json
{
  "access_token": "ya29.…",
  "expires_in": 3599,
  "token_type": "Bearer"
}
```

The client wraps the token + an absolute `Instant` deadline computed from `expires_in`. The `MetadataClient` exposes one async method:

```rust
impl MetadataClient {
    pub async fn token(&self) -> Result<SecretString>;
}
```

Internally, `token()` checks the cached token's deadline. If > 60 seconds remain, returns the cache. If ≤ 60 seconds remain, attempts a refresh; on success, updates the cache and returns the new token; on failure (metadata server unreachable, malformed response, etc.) returns the still-valid current token until it expires.

There is no proactive background refresh task. The next caller drives the refresh. This matches the daemon's existing "no background work" discipline and keeps the failure model simple (no orphaned background tasks holding tokens past shutdown).

The metadata server endpoint is hardcoded; there is no `--metadata-server-url` flag. Operators who need to test against a mock metadata server use `httpmock` in unit tests; production deployments always hit `metadata.google.internal`.

### Decision 3 — 60-second refresh-before-expiry window

The constant `const TOKEN_REFRESH_BUFFER_SECONDS: u64 = 60;` lives in `agentic-core/src/gcs_auth.rs`.

The choice of 60 seconds is the GCP-conventional refresh window. It is well within the typical GCS request RTT (single-digit ms) plus retry budget (a few seconds), so a token refreshed at the 60-second mark will still be valid even if every retry path is exercised on the in-flight put/get.

A token that fails to refresh (metadata server intermittently unreachable) is used until its actual `expires_in` deadline. Each put/get attempts a refresh; the daemon does not error until the cached token is genuinely expired. Audit-log lines fire on refresh failure at `tracing::warn!` to surface metadata-server flakiness.

### Decision 4 — Fail-secure when both paths are available

If `--insecure-static-bearer` is passed AND the daemon can reach the metadata server at startup, the daemon refuses to start unless `--insecure-static-bearer-allow-on-gcp` is also passed.

The check at startup:

```rust
async fn select_gcs_auth(args: &Args) -> Result<GcsAuth> {
    let metadata_reachable = MetadataClient::probe().await.is_ok();
    match (&args.insecure_static_bearer, metadata_reachable, args.insecure_static_bearer_allow_on_gcp) {
        (Some(_), true, false) => Err("static bearer supplied but metadata server is reachable; \
                                       this is almost certainly a misconfiguration. \
                                       Drop --insecure-static-bearer to use workload-identity, \
                                       or pass --insecure-static-bearer-allow-on-gcp to override."),
        (Some(bearer), _, _) => Ok(GcsAuth::StaticBearer(bearer.clone())),
        (None, true, _) => Ok(GcsAuth::WorkloadIdentity(MetadataClient::new()?)),
        (None, false, _) => Err("GCS auth requires --insecure-static-bearer when the GCE metadata server \
                                 is unreachable. Use `gcloud auth print-access-token`."),
    }
}
```

This catches the "I deployed to Cloud Run but my devshell-derived bearer is still in env" footgun. The override flag (`--insecure-static-bearer-allow-on-gcp`) exists for the legitimate testing case where an operator deliberately wants to use a static bearer on Cloud Run; the flag's name documents that this is the unusual path.

### Decision 5 — Static-bearer path stays usable for the demo

The local-Docker-compose broken-prompt demo runs `agenticd` on the contributor's laptop. There is no metadata server reachable from there. The demo's `GcsObjectStore` exercise (the GCS integration tests under `crates/agentic-core/tests/`) uses:

```
DATABASE_URL=… cargo run -p agentic-cli -- daemon \
    --object-store gcs \
    --gcs-bucket my-test-bucket \
    --insecure-static-bearer "$(gcloud auth print-access-token)"
```

The flag is deliberately verbose. A contributor who wants to use a static bearer on their laptop sees `--insecure-static-bearer` in their shell history and knows this is the off-prod path. There is no `--gcs-bearer` short alias.

For unit tests against `httpmock`, the static-bearer path is the canonical test surface — `httpmock` simulates GCS endpoints, and the `MetadataClient::probe()` returns Err on a developer laptop, so neither override flag is needed.

### Decision 6 — No service-account key files in v1.0

The daemon does not read `*.json` service-account key files in v1.0. The two reasons:

1. The TM-007 threat is exactly "long-lived secret in process state or on disk." Service-account-key files are precisely that, just in a different format from a raw bearer token.
2. The legitimate use cases for service-account keys are (a) CI/CD pipelines running outside GCP, and (b) self-hosted deployments. For (a), CI systems can mint short-lived tokens via OIDC federation. For (b), self-hosted is out of scope for the v1.0 sidecar deployment per ADR-0001 Decision 9.

If a deployment shape emerges in v1.1+ that requires service-account keys, that's an explicit ADR with its own threat-model addendum. v1.0 ships without them.

## Consequences

**Positive:**

- TM-007 closes for production deployments. The bearer token never appears in env, files, or process command-line history. A worker sharing UID with agenticd cannot read a static token because there is none.
- Workload-identity tokens are GCP-rotated; their blast radius on leak is bounded to the GCP-configured token TTL (typically 1 hour) instead of "however long agenticd has been running."
- The `GcsAuth` enum is a clean abstraction for v1.1's async-trait redesign (ADR-0011) and any future auth modes (KMS-backed, OIDC-federated, etc.). The trait stays small.
- The fail-secure default (refuse static bearer when metadata is reachable) catches a real misconfiguration class: operators who copy their devshell bearer into a Cloud Run env-var and don't realize workload-identity was already set up.
- The contributor demo path stays usable. The friction of `--insecure-static-bearer` is precisely calibrated: low enough to not block legitimate exploration, high enough to make the off-prod path visible.

**Negative:**

- The metadata-server dependency is a new fragile path. If `metadata.google.internal` is unreachable mid-flight, in-flight puts/gets fall back to the still-valid current token, then fail when it expires. Cloud Run instance lifetime is typically minutes-to-hours, so this is unlikely to bite in practice, but operators monitoring should watch for the `tracing::warn!` lines on refresh failure.
- The static-bearer escape hatch is real and operator-managed. The same critique that applies to `--insecure-allow-any-uid` (ADR-0012) applies here: a deployment that turns it on and forgets is a real risk. Mitigation: every daemon startup under `--insecure-static-bearer` logs a `tracing::warn!` ("running with --insecure-static-bearer; ..."), same observability pattern as ADR-0012.
- v1.0 doesn't help self-hosted users who run on hardware without workload-identity. The static-bearer path is the only option; operators must rotate their own tokens. This is consistent with ADR-0001 Decision 9 (CLI-first, self-hosted Docker compose) for v1.0 and a v1.1 conversation otherwise.

**Risks to revisit:**

- The 60-second refresh buffer is a guess. The first production deployment may surface latency-correlated refresh races; the constant is one-PR tunable.
- The metadata-server URL is hardcoded. If GCP changes the endpoint or introduces a region-specific variant, the daemon needs a rebuild. Acceptable for v1.0; if it bites, a config flag can be added.
- The `MetadataClient::probe()` check at startup adds one extra HTTP request to the daemon's boot path. Bounded by a short timeout (1 second) so it doesn't slow boot on a misconfigured deployment.

See also: [`docs/superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md`](../superpowers/specs/2026-05-21-pre-public-hardening-sprint-design.md) §"ADR-0015" (the sprint design that frames this ADR), `git.agentic-threat-model.md` TM-007 (the row this ADR closes), [ADR-0004](./0004-realtime-agenticd-for-executor.md) Decision 5 (the GCS-backed `ObjectStore` commitment this ADR specifies the auth shape for).
