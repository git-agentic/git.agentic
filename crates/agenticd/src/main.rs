//! agenticd — the git.agentic daemon.
//!
//! Long-lived Rust process. Listens on a Unix domain socket for SDK and
//! CLI requests, owns the object store, and orchestrates snapshots.

mod server;

use anyhow::Context;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UnixListener;

use crate::server::{handle_connection, DaemonState};

#[derive(Parser, Debug)]
#[command(name = "agenticd", version, about = "git.agentic daemon")]
struct Args {
    /// Path to the repo root (parent of `.agentic/`).
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Socket path. Default: <repo>/.agentic/agenticd.sock.
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let agentic_dir = args.repo.join(".agentic");
    let state = Arc::new(DaemonState::open(agentic_dir.clone())?);

    let socket_path = args
        .socket
        .unwrap_or_else(|| agentic_dir.join("agenticd.sock"));

    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("removing stale socket {}", socket_path.display()))?;
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("binding {}", socket_path.display()))?;

    tracing::info!(
        socket = %socket_path.display(),
        repo = %args.repo.display(),
        "agenticd listening"
    );

    loop {
        let (sock, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(state, sock).await {
                tracing::warn!(error = %format!("{e:#}"), "connection error");
            }
        });
    }
}
