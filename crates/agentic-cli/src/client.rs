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
    send_to(connect(&socket_path(repo)).await?, request).await
}

/// An auto-minted approval token and the uid that owns the key it came from.
pub struct AutoToken {
    pub token: String,
    pub key_owner_uid: u32,
}

/// The daemon socket is served by a uid other than the owner of the approval
/// key (or its peer uid could not be read), so the CLI will not take part in
/// an auto-signed flow with it.
#[derive(Debug)]
pub struct PeerMismatch(pub String);

impl std::fmt::Display for PeerMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PeerMismatch {}

/// Check a connection's peer uid, taken from the kernel (SO_PEERCRED, or
/// getpeereid on macOS), against the uid that owns the approval key.
pub fn check_peer(peer: std::io::Result<u32>, key_owner_uid: u32) -> Result<(), PeerMismatch> {
    match peer {
        Ok(uid) if uid == key_owner_uid => Ok(()),
        Ok(_) => Err(PeerMismatch(
            "the daemon socket's peer is not the owner of the approval key".to_string(),
        )),
        Err(e) => Err(PeerMismatch(format!(
            "could not read the daemon socket's peer uid: {e}"
        ))),
    }
}

/// Decide whether an auto-minted token may be sent to the server on this
/// connection. The token is a bearer credential for one rollback, so it goes
/// only to a server whose peer uid is the owner of the key it was minted
/// from. Otherwise it is withheld, with a warning on stderr (not stdout, so
/// `--json` stays parseable).
pub fn token_for_peer(
    peer: std::io::Result<u32>,
    key_owner_uid: u32,
    token: String,
) -> Option<String> {
    match check_peer(peer, key_owner_uid) {
        Ok(()) => Some(token),
        Err(e) => {
            eprintln!("warning: not sending the approval token: {e}");
            None
        }
    }
}

/// Like [`round_trip`], for a request in an auto-signing flow (resolving the
/// target, the preview). Nothing is sent unless the connection's peer uid is
/// `key_owner_uid`; otherwise the error is a [`PeerMismatch`]. `token`, if
/// any, is attached to a rollback request after that check.
pub async fn round_trip_pinned(
    repo: &Path,
    request: Request,
    key_owner_uid: u32,
    token: Option<String>,
) -> anyhow::Result<Response> {
    exchange_at(
        &socket_path(repo),
        request,
        Some(key_owner_uid),
        token,
        true,
    )
    .await
}

/// Like [`round_trip`], for the applied request. If `auto` is given and
/// `request` is a rollback, the token is attached only after the connection's
/// peer uid is checked; on a mismatch the request is still sent, without a
/// token, and the daemon decides whether one was needed.
pub async fn round_trip_signed(
    repo: &Path,
    request: Request,
    auto: Option<AutoToken>,
) -> anyhow::Result<Response> {
    let (owner, token) = match auto {
        Some(a) => (Some(a.key_owner_uid), Some(a.token)),
        None => (None, None),
    };
    exchange_at(&socket_path(repo), request, owner, token, false).await
}

/// Connect to `path`. If `owner` is set, check the connection's peer uid
/// against it: `strict` turns a mismatch into an error before anything is
/// sent, otherwise the token is withheld with a warning. A token is attached
/// to a rollback request only if the check passed.
async fn exchange_at(
    path: &Path,
    mut request: Request,
    owner: Option<u32>,
    token: Option<String>,
    strict: bool,
) -> anyhow::Result<Response> {
    let sock = connect(path).await?;
    if let Some(owner) = owner {
        let peer = sock.peer_cred().map(|c| c.uid());
        if strict {
            check_peer(peer, owner)?;
            if let (Some(t), Request::Rollback { approval_token, .. }) = (token, &mut request) {
                *approval_token = Some(t);
            }
        } else if let (Some(t), Request::Rollback { approval_token, .. }) = (token, &mut request) {
            *approval_token = token_for_peer(peer, owner, t);
        }
    }
    send_to(sock, request).await
}

async fn connect(path: &Path) -> anyhow::Result<UnixStream> {
    UnixStream::connect(path)
        .await
        .with_context(|| format!("connecting to daemon at {}", path.display()))
}

async fn send_to(mut sock: UnixStream, request: Request) -> anyhow::Result<Response> {
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

    use agentic_proto::framing::{read_frame, write_frame};
    use tokio::net::UnixListener;

    fn my_uid() -> u32 {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() }
    }

    /// Serve one connection on a fresh socket; the task yields the request
    /// it received, or `None` if the client closed without sending one.
    fn one_shot_server() -> (
        tempfile::TempDir,
        PathBuf,
        tokio::task::JoinHandle<Option<Request>>,
    ) {
        let dir = tempfile::Builder::new()
            .prefix("agc")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let task = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let (r, w) = s.split();
            let mut r = tokio::io::BufReader::new(r);
            let mut w = tokio::io::BufWriter::new(w);
            let env: Envelope<Request> = read_frame(&mut r).await.ok()??;
            let reply = Response::Pong;
            write_frame(&mut w, &Envelope::new(env.correlation_id, reply))
                .await
                .unwrap();
            Some(env.payload)
        });
        (dir, path, task)
    }

    fn rollback_request() -> Request {
        Request::Rollback {
            target: "t".into(),
            dry_run: false,
            accept_data_loss: false,
            approval_token: None,
        }
    }

    #[tokio::test]
    async fn pinned_round_trip_sends_nothing_to_a_peer_the_key_owner_does_not_run() {
        let (_dir, path, server) = one_shot_server();
        let err = exchange_at(&path, Request::Ping, Some(my_uid() + 1), None, true)
            .await
            .expect_err("a mismatched peer must abort the flow");
        assert!(err.downcast_ref::<PeerMismatch>().is_some(), "{err:#}");
        assert!(server.await.unwrap().is_none(), "no request may be sent");
    }

    #[tokio::test]
    async fn pinned_round_trip_talks_to_the_key_owners_daemon() {
        let (_dir, path, server) = one_shot_server();
        let resp = exchange_at(&path, Request::Ping, Some(my_uid()), None, true)
            .await
            .unwrap();
        assert!(matches!(resp, Response::Pong));
        assert!(matches!(server.await.unwrap(), Some(Request::Ping)));
    }

    #[tokio::test]
    async fn pinned_round_trip_with_a_token_aborts_on_a_mismatched_peer_before_sending() {
        let (_dir, path, server) = one_shot_server();
        let err = exchange_at(
            &path,
            rollback_request(),
            Some(my_uid() + 1),
            Some("1:ab".into()),
            true,
        )
        .await
        .expect_err("strict mode aborts");
        assert!(err.downcast_ref::<PeerMismatch>().is_some());
        assert!(server.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn signed_round_trip_withholds_the_token_from_a_mismatched_peer() {
        for (owner, expect_token) in [(my_uid() + 1, false), (my_uid(), true)] {
            let (_dir, path, server) = one_shot_server();
            exchange_at(
                &path,
                rollback_request(),
                Some(owner),
                Some("1:ab".into()),
                false,
            )
            .await
            .unwrap();
            let Some(Request::Rollback { approval_token, .. }) = server.await.unwrap() else {
                panic!("server saw no rollback");
            };
            assert_eq!(approval_token.is_some(), expect_token, "owner {owner}");
        }
    }

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
