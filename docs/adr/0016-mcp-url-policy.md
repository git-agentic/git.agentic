# ADR-0016: MCP Fingerprinting URL Policy

**Status:** Accepted
**Date:** 2026-07-10
**Deciders:** Toni
**Closes:** 2026-07-09 security audit finding #4 (SSRF exposure from operator-configured MCP URLs), the policy half. The memory-exhaustion half (response/depth/manifest caps) shipped in PR #117.
**Relates to:** [ADR-0004](./0004-realtime-agenticd-for-executor.md) (sidecar `agenticd` on Cloud Run — the deployment whose metadata server is the SSRF prize), [ADR-0012](./0012-socket-peer-authentication.md) (peer-UID auth; MCP URLs are operator config, a different trust tier than socket input).

## Context

On every commit, `agenticd` fingerprints each configured MCP server by POSTing `tools/list` to its URL and hashing the response into the commit's `tools` dimension (`crates/agenticd/src/mcp.rs`). The server list comes from the operator-provided `--mcp name=url` startup flag — it is configuration, not untrusted socket input, so an attacker who can set it already controls the daemon.

The residual risk is different: what a *configured* endpoint can do to the daemon once it is talking to it. Two vectors:

- **Plaintext to a non-loopback host.** An `http://` URL to a remote host sends the request (and exposes the response that gets committed) over the wire in clear, and invites a network attacker to impersonate the server.
- **Redirects.** `reqwest`'s default client follows up to 10 redirects. A compromised or malicious configured server can answer the `tools/list` POST with a 302 to an internal address — on Cloud Run, the GCE metadata server (`169.254.169.254`) — and the daemon would fetch that, canonicalise it, and persist it into the commit tree. That is SSRF plus an exfiltration channel into the object store, and no scheme check on the *configured* URL stops it, because the dangerous destination is one the operator never wrote down.

## Decision

1. **HTTPS required except loopback.** At startup, reject any configured MCP URL whose scheme is not `https`, except that `http` is permitted when the host is loopback (`localhost`, `127.0.0.0/8`, `::1`). Rejection is a typed error that aborts daemon startup. There is **no** `--insecure-allow-http` escape hatch: remote-plaintext MCP is not a deployment shape we support even behind friction. (Contrast ADR-0012's `--insecure-allow-any-uid`, which exists because the demo needs it; the demo configures no MCP servers, so nothing analogous is needed here.)

2. **Redirect-following disabled.** The daemon's shared HTTP client is built with `reqwest::redirect::Policy::none()`. A redirect response to a fingerprint request becomes a per-server error naming the `Location` target, rather than being followed. This is the half that stops a genuine remote attacker: with redirects off, the set of hosts the daemon will contact equals the configured list exactly. A legitimate MCP JSON-RPC endpoint has no reason to redirect a POST; if a server moves, the operator updates the flag.

3. **No separate allowlist — closed by construction.** The audit suggested an operator allowlist. `--mcp name=url` is *already* an explicit operator enumeration of every URL the daemon will contact. With schemes validated (Decision 1) and redirects disabled (Decision 2), the destination set is exactly that list and nothing at runtime can extend it. A second host allowlist would restate the first list with added ceremony and no new coverage, so it is deliberately omitted.

## Consequences

**Positive.** The daemon's outbound fingerprint traffic is TLS off-loopback, cannot be laundered to an unconfigured host, and the reachable-host set is auditable from the `--mcp` flag alone. The demo is unaffected: `run-demo.sh` passes no `--mcp`, and the documented local-dev shapes (`http://localhost:...`) remain valid under the loopback exception.

**Negative.** An operator running a remote MCP server over plaintext must front it with TLS (or tunnel it through loopback). That is intentional friction. A server that legitimately relocates via HTTP redirect will fail fingerprinting until the operator updates the flag — also intentional.

**Shared-client caveat.** `DaemonState.http` is one client shared across all outbound use. `Policy::none()` is set on it for the MCP fingerprint path; any future non-MCP use of that client that legitimately needs redirects must build its own client rather than relaxing this one. A comment at the construction site pins this.

**Risks to revisit.** The loopback exception trusts that a loopback address on the sidecar is the operator's own co-located MCP server; in the single-tenant Cloud Run sidecar topology (ADR-0004) that holds. A future multi-tenant or shared-host deployment would need to revisit whether loopback is still a safe carve-out.
