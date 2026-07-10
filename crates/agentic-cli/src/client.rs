//! Unix-socket client for the `agentic` CLI.
//!
//! The CLI is a thin wrapper around the daemon: every command opens the
//! socket, sends one length-prefixed JSON envelope, and reads exactly one
//! envelope back. We deliberately do not pipeline requests — the latency
//! overhead is irrelevant for the human-driven CLI.

use std::path::{Path, PathBuf};

use agentic_proto::framing::{read_frame, write_frame};
use agentic_proto::{Envelope, Request, Response};
use anyhow::{anyhow, Context};
use tokio::net::UnixStream;

/// Resolve the repo directory: explicit `--repo` wins; otherwise walk up
/// from CWD looking for `.agentic/`, falling back to CWD.
pub fn resolve_repo(explicit: Option<&str>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(PathBuf::from(p));
    }
    let cwd = std::env::current_dir().context("getting current dir")?;
    let mut here = cwd.as_path();
    loop {
        if here.join(".agentic").is_dir() {
            return Ok(here.to_path_buf());
        }
        match here.parent() {
            Some(p) => here = p,
            None => return Ok(cwd),
        }
    }
}

/// Daemon socket path: the `AGENTIC_SOCKET` environment variable wins
/// (matching the Python SDK and `run-demo.sh`, which bind under /tmp
/// because nested repo paths overflow SUN_LEN); otherwise the repo
/// default `<repo>/.agentic/agenticd.sock`.
pub fn socket_path(repo: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("AGENTIC_SOCKET") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    repo.join(".agentic").join("agenticd.sock")
}

/// Send one request envelope and return the response payload.
pub async fn round_trip(repo: &Path, request: Request) -> anyhow::Result<Response> {
    let path = socket_path(repo);
    let mut sock = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to daemon at {}", path.display()))?;
    let envelope = Envelope::new(new_correlation_id(), request);
    let (read_half, write_half) = sock.split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let mut writer = tokio::io::BufWriter::new(write_half);
    write_frame(&mut writer, &envelope).await?;
    let reply: Envelope<Response> = read_frame(&mut reader)
        .await?
        .ok_or_else(|| anyhow!("daemon closed connection without reply"))?;
    if reply.correlation_id != envelope.correlation_id {
        return Err(anyhow!(
            "correlation id mismatch: sent {} got {}",
            envelope.correlation_id,
            reply.correlation_id
        ));
    }
    Ok(reply.payload)
}

fn new_correlation_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("cli-{pid}-{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both assertions live in one test: AGENTIC_SOCKET is process-global
    // state and separate parallel tests would race on it.
    #[test]
    fn socket_path_honors_agentic_socket_env() {
        let saved = std::env::var_os("AGENTIC_SOCKET");

        std::env::set_var("AGENTIC_SOCKET", "/tmp/short.sock");
        assert_eq!(
            socket_path(Path::new("/some/repo")),
            PathBuf::from("/tmp/short.sock")
        );

        std::env::remove_var("AGENTIC_SOCKET");
        assert_eq!(
            socket_path(Path::new("/some/repo")),
            Path::new("/some/repo/.agentic/agenticd.sock")
        );

        match saved {
            Some(v) => std::env::set_var("AGENTIC_SOCKET", v),
            None => std::env::remove_var("AGENTIC_SOCKET"),
        }
    }
}
