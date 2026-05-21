//! MCP server fingerprinter.
//!
//! Closes the sixth ADR-0001 tuple dimension (`tools`). Given a list of
//! pinned MCP servers, we hit each one's JSON-RPC endpoint, request
//! `tools/list`, canonicalize the JSON response (sorted keys, no
//! whitespace), and produce a deterministic BLAKE3 fingerprint per
//! server. The commit dispatcher builds a Tree of one Blob per tracked
//! server keyed by name; that Tree's hash lands in `Commit.tools`.
//!
//! MVP transport is HTTP+JSON over a single POST. The MCP spec also
//! supports stdio and SSE; both are post-MVP. If a server requires an
//! `initialize` handshake before serving `tools/list`, the call returns
//! the server's error message verbatim.

use std::time::Duration;

use anyhow::{anyhow, Context};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

/// Maximum concurrent `fingerprint_one` calls inside `fingerprint_all`.
/// Keeps a slow-but-many MCP fleet from saturating the local outbound
/// HTTP connection pool while still letting independent servers run in
/// parallel. Audit §A7 / B1 / C3 / R9.
const FINGERPRINT_CONCURRENCY: usize = 8;

/// One server to fingerprint on each commit. The `name` is purely a
/// commit-tree key — it has no relationship to the MCP server's internal
/// identity. `url` must point at a JSON-RPC endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpServerSpec {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'a str,
    id: u32,
    method: &'a str,
    params: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// Output of fingerprinting a single MCP server.
#[derive(Clone, Debug)]
pub struct McpFingerprint {
    pub name: String,
    /// Kept around for diagnostics + future use by `agentic mcp pin`.
    /// Allowed to be unread today because the commit-staging path only
    /// needs `name` and the canonical bytes.
    #[allow(dead_code)]
    pub url: String,
    /// Canonical JSON bytes of the `tools/list` result.
    pub canonical_manifest: Vec<u8>,
}

/// Fingerprint every server in `servers`. Errors are returned per-server
/// so a failed server doesn't drop the others.
///
/// Audit §A7 / §B1 / §C3 / §R9: pre-A7 this loop was sequential, so
/// `handle_commit` held `commit_lock` for up to `N × 10s` on a slow MCP
/// fleet (10s is the per-server timeout in `fingerprint_one`). With
/// `futures::stream::iter(...).buffered(FINGERPRINT_CONCURRENCY)` the
/// total wall time becomes `max(per-server)` rather than `sum`, and the
/// output order remains the input order — preserved on purpose so
/// `commit::fingerprint_tools`'s `zip(state.mcp_servers.iter(), fingerprints)`
/// still attributes per-server errors to the correct server name.
pub async fn fingerprint_all(
    client: &reqwest::Client,
    servers: &[McpServerSpec],
) -> Vec<Result<McpFingerprint, anyhow::Error>> {
    stream::iter(servers.iter().map(|spec| fingerprint_one(client, spec)))
        .buffered(FINGERPRINT_CONCURRENCY)
        .collect()
        .await
}

/// Hit one MCP server's `tools/list` and return canonicalized bytes.
pub async fn fingerprint_one(
    client: &reqwest::Client,
    spec: &McpServerSpec,
) -> anyhow::Result<McpFingerprint> {
    let body = JsonRpcRequest {
        jsonrpc: "2.0",
        id: 1,
        method: "tools/list",
        params: serde_json::json!({}),
    };
    let resp = client
        .post(&spec.url)
        .timeout(Duration::from_secs(10))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {}", spec.url))?;
    let status = resp.status();
    let bytes = resp.bytes().await?;
    if !status.is_success() {
        return Err(anyhow!(
            "MCP server {} returned HTTP {}: {}",
            spec.name,
            status,
            String::from_utf8_lossy(&bytes)
        ));
    }
    let rpc: JsonRpcResponse = serde_json::from_slice(&bytes)
        .with_context(|| format!("decoding JSON-RPC reply from {}", spec.url))?;
    if let Some(err) = rpc.error {
        return Err(anyhow!(
            "MCP server {} returned JSON-RPC error {}: {}",
            spec.name,
            err.code,
            err.message
        ));
    }
    let result = rpc.result.ok_or_else(|| {
        anyhow!(
            "MCP server {} returned JSON-RPC reply with neither result nor error",
            spec.name
        )
    })?;
    let canonical_manifest = canonicalize(&result);
    Ok(McpFingerprint {
        name: spec.name.clone(),
        url: spec.url.clone(),
        canonical_manifest,
    })
}

/// Canonicalize a JSON value: sort object keys recursively, preserve
/// array order, emit minimal-whitespace JSON. This is the byte string we
/// hash and persist as the manifest Blob.
pub fn canonicalize(value: &serde_json::Value) -> Vec<u8> {
    let normalized = sort_keys(value);
    // INVARIANT: `sort_keys` only produces a `serde_json::Value` built from
    // owned strings, owned vectors, booleans, numbers, and `Null`. Every
    // such value is representable as JSON by construction — `serde_json::to_vec`
    // only fails for I/O on a `Writer` (we pass a `Vec<u8>`) or for keys
    // that aren't strings (`Map` keys are `String` here). So this is
    // unreachable; CLAUDE.md requires the comment for any non-test `expect`.
    serde_json::to_vec(&normalized).expect("canonical serialize cannot fail")
}

fn sort_keys(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: std::collections::BTreeMap<String, serde_json::Value> =
                std::collections::BTreeMap::new();
            for (k, v) in map {
                sorted.insert(k.clone(), sort_keys(v));
            }
            let mut out = serde_json::Map::new();
            for (k, v) in sorted {
                out.insert(k, v);
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sort_keys).collect())
        }
        other => other.clone(),
    }
}

/// Parse a `name=url,…` spec into a server list. Used for the `--mcp`
/// CLI flag.
pub fn parse_mcp_spec(items: &[String]) -> anyhow::Result<Vec<McpServerSpec>> {
    items
        .iter()
        .map(|s| {
            let (name, url) = s
                .split_once('=')
                .ok_or_else(|| anyhow!("--mcp expects name=url, got {s:?}"))?;
            if name.is_empty() || url.is_empty() {
                return Err(anyhow!("--mcp expects non-empty name and url, got {s:?}"));
            }
            Ok(McpServerSpec {
                name: name.trim().to_string(),
                url: url.trim().to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonicalize_sorts_keys_recursively() {
        let a = json!({"b": 2, "a": {"y": 1, "x": [3, 2, 1]}});
        let b = json!({"a": {"x": [3, 2, 1], "y": 1}, "b": 2});
        assert_eq!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn canonicalize_preserves_array_order() {
        let a = json!([1, 2, 3]);
        let b = json!([3, 2, 1]);
        assert_ne!(canonicalize(&a), canonicalize(&b));
    }

    #[test]
    fn parse_mcp_spec_basic() {
        let v = parse_mcp_spec(&[
            "search=http://localhost:8001".into(),
            "rag=http://localhost:8002/rpc".into(),
        ])
        .unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "search");
        assert_eq!(v[1].url, "http://localhost:8002/rpc");
    }

    #[test]
    fn parse_mcp_spec_rejects_malformed() {
        assert!(parse_mcp_spec(&["no-equals-sign".into()]).is_err());
        assert!(parse_mcp_spec(&["=http://x".into()]).is_err());
        assert!(parse_mcp_spec(&["name=".into()]).is_err());
    }

    // ---------------------------------------------------------------
    // Audit §A7 — parallelisation tests.
    //
    // A hand-rolled minimal HTTP server avoids pulling in `wiremock`
    // (issue #53 will add a proper mock fixture for the broader MCP
    // test gaps). Each fake responds to any single POST after sleeping
    // for `delay` and returns a fixed `tools/list` JSON-RPC payload.
    // ---------------------------------------------------------------

    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawn a fake MCP server on an ephemeral 127.0.0.1 port. Accepts
    /// `n_requests` connections, sleeps `delay` per request, replies
    /// with a fixed `tools/list` JSON-RPC result. The task exits after
    /// `n_requests` accepts so test teardown is clean.
    async fn spawn_slow_mcp_server(
        delay: Duration,
        n_requests: usize,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            for _ in 0..n_requests {
                let (mut sock, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                // Read whatever the client sent — we don't actually
                // need the body, just drain enough to avoid the client
                // blocking on its own write.
                let mut buf = [0u8; 4096];
                let _ = tokio::time::timeout(Duration::from_millis(200), sock.read(&mut buf)).await;
                tokio::time::sleep(delay).await;
                let body = br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(headers.as_bytes()).await;
                let _ = sock.write_all(body).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, handle)
    }

    /// AC for issue #42 / audit §A7: three servers each delaying
    /// `delay` should finish in roughly `delay`, not `3 × delay`.
    /// Audit's pseudocode said 5s per server; we use 1s to keep CI
    /// fast while still leaving headroom between the parallel and
    /// serial bounds (serial = 3s, parallel ≈ 1s + scheduler slack).
    #[tokio::test]
    async fn fingerprint_all_runs_servers_in_parallel() {
        let delay = Duration::from_millis(1000);
        let (addr1, h1) = spawn_slow_mcp_server(delay, 1).await;
        let (addr2, h2) = spawn_slow_mcp_server(delay, 1).await;
        let (addr3, h3) = spawn_slow_mcp_server(delay, 1).await;
        let servers = vec![
            McpServerSpec {
                name: "a".into(),
                url: format!("http://{addr1}/"),
            },
            McpServerSpec {
                name: "b".into(),
                url: format!("http://{addr2}/"),
            },
            McpServerSpec {
                name: "c".into(),
                url: format!("http://{addr3}/"),
            },
        ];

        let client = reqwest::Client::new();
        let start = std::time::Instant::now();
        let results = fingerprint_all(&client, &servers).await;
        let elapsed = start.elapsed();

        assert_eq!(results.len(), 3);
        assert!(
            results.iter().all(|r| r.is_ok()),
            "all servers should fingerprint cleanly; errors: {:?}",
            results
                .iter()
                .filter_map(|r| r.as_ref().err())
                .collect::<Vec<_>>()
        );
        // Generous upper bound: the serial pre-A7 path would have
        // taken >= 3 × 1s = 3s. Parallel path takes ~1s + scheduling
        // slack; 2.5s gives plenty of headroom on a loaded runner
        // without making the test flake-prone.
        assert!(
            elapsed < Duration::from_millis(2500),
            "parallel fingerprinting of 3 servers each delaying {delay:?} should complete < 2.5s; took {elapsed:?}"
        );

        // Clean up.
        let _ = tokio::time::timeout(Duration::from_secs(1), h1).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), h2).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), h3).await;
    }

    /// Output ordering: `buffered(N)` preserves input order even when
    /// inner futures complete out of order, so callers can `zip` with
    /// the input `servers` slice and still get correct attribution.
    /// `commit::fingerprint_tools` relies on this for per-server error
    /// messages. (Test-analyzer review pattern from PR #52 / issue #53
    /// — the cheaper, dep-free part of that test gap is covered here.)
    #[tokio::test]
    async fn fingerprint_all_preserves_input_order() {
        // Server 0 is SLOW (300ms), server 1 is FAST (no delay). If
        // the implementation used `buffer_unordered` instead of
        // `buffered`, the fast server would land in results[0] and
        // this assertion would fail.
        let (addr_slow, h_slow) = spawn_slow_mcp_server(Duration::from_millis(300), 1).await;
        let (addr_fast, h_fast) = spawn_slow_mcp_server(Duration::ZERO, 1).await;
        let servers = vec![
            McpServerSpec {
                name: "slow".into(),
                url: format!("http://{addr_slow}/"),
            },
            McpServerSpec {
                name: "fast".into(),
                url: format!("http://{addr_fast}/"),
            },
        ];

        let client = reqwest::Client::new();
        let results = fingerprint_all(&client, &servers).await;

        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].as_ref().unwrap().name,
            "slow",
            "results[0] must match servers[0] regardless of completion order"
        );
        assert_eq!(results[1].as_ref().unwrap().name, "fast");

        let _ = tokio::time::timeout(Duration::from_secs(1), h_slow).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), h_fast).await;
    }
}
