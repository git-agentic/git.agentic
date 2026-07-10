//! Integration tests for issue #118 socket availability limits, driven
//! through the real binary over a real Unix socket. Uses
//! --insecure-allow-any-uid so no UID fixtures are needed; limits key
//! on the observed UID regardless of auth mode, and every connection
//! here shares the test process's UID.

use agentic_proto::framing::{read_frame, write_frame};
use agentic_proto::{Envelope, Request, Response};
use std::process::{Child, Command};
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::UnixStream;

fn agenticd_bin() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_agenticd").into()
}

/// Reaps the spawned daemon on drop so a failing assertion can't leak
/// an orphaned agenticd process holding the test socket.
struct DaemonProc(Child);

impl Drop for DaemonProc {
    fn drop(&mut self) {
        self.0.kill().ok();
        let _ = self.0.wait();
    }
}

/// Spawn the daemon with `extra_args`, wait for the socket. Panics if
/// the socket never appears.
async fn spawn_daemon(dir: &TempDir, extra_args: &[&str]) -> (DaemonProc, std::path::PathBuf) {
    let sock = dir.path().join("agenticd.sock");
    let mut cmd = Command::new(agenticd_bin());
    cmd.arg("--repo")
        .arg(dir.path())
        .arg("--socket")
        .arg(&sock)
        .arg("--insecure-allow-any-uid");
    for a in extra_args {
        cmd.arg(a);
    }
    let child = DaemonProc(cmd.spawn().expect("spawn agenticd"));
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(sock.exists(), "agenticd never created its socket");
    (child, sock)
}

/// Open a connection and prove it is live with a Ping/Pong round trip.
async fn connect_and_ping(sock: &std::path::Path, tag: &str) -> UnixStream {
    let mut conn = UnixStream::connect(sock).await.expect("connect");
    write_frame(&mut conn, &Envelope::new(tag.to_string(), Request::Ping))
        .await
        .expect("write ping");
    let reply: Envelope<Response> = read_frame(&mut conn)
        .await
        .expect("read pong")
        .expect("daemon must reply on a live connection");
    assert!(matches!(reply.payload, Response::Pong));
    conn
}

/// Assert the daemon dropped this connection: the next read hits EOF
/// (or an I/O reset), never a well-formed reply. Same tri-state
/// discrimination as peer_auth_integration.rs.
async fn assert_dropped(sock: &std::path::Path) {
    let outcome = tokio::time::timeout(Duration::from_secs(2), async {
        let mut conn = UnixStream::connect(sock).await?;
        let _ = write_frame(&mut conn, &Envelope::new("dropped", Request::Ping)).await;
        let frame: Option<Envelope<Response>> = match read_frame(&mut conn).await {
            Ok(opt) => opt,
            Err(agentic_proto::framing::FrameError::Io(_)) => None,
            Err(other) => {
                return Err(anyhow::Error::new(other)
                    .context("daemon sent a malformed frame on a capped connection"));
            }
        };
        Ok::<_, anyhow::Error>(frame)
    })
    .await
    .expect("connection attempt timed out — daemon hung instead of dropping")
    .expect("transport-level failure other than a drop");
    assert!(
        outcome.is_none(),
        "daemon replied on a connection past the cap; got {outcome:?}"
    );
}

#[tokio::test]
async fn global_connection_cap_drops_excess_and_spares_existing() {
    let dir = TempDir::new().unwrap();
    let (_daemon, sock) = spawn_daemon(&dir, &["--max-connections", "2"]).await;

    let mut c1 = connect_and_ping(&sock, "c1").await;
    let _c2 = connect_and_ping(&sock, "c2").await;

    // Third connection is over the global cap: dropped at accept.
    assert_dropped(&sock).await;

    // Existing connections are unaffected.
    write_frame(&mut c1, &Envelope::new("c1-again", Request::Ping))
        .await
        .expect("c1 write");
    let reply: Envelope<Response> = read_frame(&mut c1)
        .await
        .expect("c1 read")
        .expect("c1 must still be served");
    assert!(matches!(reply.payload, Response::Pong));
}

#[tokio::test]
async fn per_uid_connection_cap_drops_excess() {
    let dir = TempDir::new().unwrap();
    // Global cap far above the per-UID cap so the per-UID path is the
    // one that trips: every connection here shares the test's UID.
    let (_daemon, sock) = spawn_daemon(
        &dir,
        &["--max-connections", "64", "--max-connections-per-uid", "1"],
    )
    .await;

    let _c1 = connect_and_ping(&sock, "c1").await;
    assert_dropped(&sock).await;
}

#[tokio::test]
async fn zero_limit_refuses_startup() {
    let dir = TempDir::new().unwrap();
    let sock = dir.path().join("agenticd.sock");
    let mut child = Command::new(agenticd_bin())
        .arg("--repo")
        .arg(dir.path())
        .arg("--socket")
        .arg(&sock)
        .arg("--insecure-allow-any-uid")
        .arg("--max-connections")
        .arg("0")
        .spawn()
        .expect("spawn agenticd");
    // The daemon must exit non-zero without ever creating the socket.
    let mut status = None;
    for _ in 0..100 {
        if let Some(s) = child.try_wait().expect("try_wait") {
            status = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let status = status.unwrap_or_else(|| {
        child.kill().ok();
        panic!("daemon did not exit on an invalid --max-connections 0");
    });
    assert!(!status.success(), "startup must fail loudly on zero limits");
    assert!(
        !sock.exists(),
        "socket must never be created when limits are invalid"
    );
}
