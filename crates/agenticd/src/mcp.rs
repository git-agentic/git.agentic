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
use serde::{Deserialize, Serialize};

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
pub async fn fingerprint_all(
    client: &reqwest::Client,
    servers: &[McpServerSpec],
) -> Vec<Result<McpFingerprint, anyhow::Error>> {
    let mut out = Vec::with_capacity(servers.len());
    for spec in servers {
        out.push(fingerprint_one(client, spec).await);
    }
    out
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
}
