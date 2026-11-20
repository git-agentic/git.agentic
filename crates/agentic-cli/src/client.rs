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
    round_trip_signed(repo, request, None).await
}

/// An auto-minted approval token and the uid that owns the key it came from.
pub struct AutoToken {
    pub token: String,
    pub key_owner_uid: u32,
}

/// Decide whether an auto-minted token may be sent to the server on this
/// connection. The token is a bearer credential for one rollback, so it goes
/// only to a server whose peer uid is the owner of the key it was minted
/// from. Otherwise it is withheld, with a warning on stderr (not stdout, so
/// `--json` stays parseable). `peer` is the connection's peer uid, taken
/// from the kernel (SO_PEERCRED, or getpeereid on macOS).
pub fn token_for_peer(
    peer: std::io::Result<u32>,
    key_owner_uid: u32,
    token: String,
) -> Option<String> {
    match peer {
        Ok(uid) if uid == key_owner_uid => Some(token),
        Ok(uid) => {
            eprintln!(
                "warning: not sending the approval token: the daemon socket is served by uid \
                 {uid}, but the approval key belongs to uid {key_owner_uid}"
            );
            None
        }
        Err(e) => {
            eprintln!(
                "warning: not sending the approval token: could not read the daemon socket's \
                 peer uid: {e}"
            );
            None
        }
    }
}

/// Like [`round_trip`]. If `auto` is given and `request` is a rollback, the
/// token is attached only after the connection's peer uid is checked.
pub async fn round_trip_signed(
    repo: &Path,
    mut request: Request,
    auto: Option<AutoToken>,
) -> anyhow::Result<Response> {
    let path = socket_path(repo);
    let mut sock = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to daemon at {}", path.display()))?;
    if let (Some(auto), Request::Rollback { approval_token, .. }) = (auto, &mut request) {
        let peer = sock.peer_cred().map(|c| c.uid());
        *approval_token = token_for_peer(peer, auto.key_owner_uid, auto.token);
    }
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

    #[test]
    fn token_is_sent_only_to_a_peer_owned_by_the_key_owner() {
        let t = || "1:ab".to_string();
        assert_eq!(token_for_peer(Ok(501), 501, t()), Some(t()));
        assert_eq!(token_for_peer(Ok(0), 501, t()), None);
        assert_eq!(token_for_peer(Ok(502), 501, t()), None);
        let err = std::io::Error::other("no cred");
        assert_eq!(token_for_peer(Err(err), 501, t()), None);
    }

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
