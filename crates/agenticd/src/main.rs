//! agenticd — the git.agentic daemon.
//!
//! Long-lived Rust process. Listens on a Unix domain socket for SDK and
//! CLI requests, owns the object store, and orchestrates snapshots.

mod mcp;
mod server;

use agentic_memory::postgres::TrackedTable;
use anyhow::{anyhow, Context};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UnixListener;

use crate::mcp::parse_mcp_spec;
use crate::server::{handle_connection, DaemonState};

fn parse_tracked_tables(spec: &[String]) -> anyhow::Result<Vec<TrackedTable>> {
    spec.iter()
        .map(|s| {
            let (name, pk) = s
                .split_once(':')
                .ok_or_else(|| anyhow!("--tables expects table:pk, got {s:?}"))?;
            Ok(TrackedTable {
                name: name.trim().to_string(),
                pk: pk.trim().to_string(),
            })
        })
        .collect()
}

#[derive(Parser, Debug)]
#[command(name = "agenticd", version, about = "git.agentic daemon")]
struct Args {
    /// Path to the repo root (parent of `.agentic/`).
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Socket path. Default: <repo>/.agentic/agenticd.sock.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Postgres URL to attach as the memory backend. If absent, commits
    /// land without a `memory_snapshot` dimension (Chunk A behaviour).
    #[arg(long)]
    postgres: Option<String>,

    /// Comma-separated `table:pk` pairs to track. Required when
    /// `--postgres` is set. Example: `episodes:id,user_facts:id`.
    #[arg(long, value_delimiter = ',')]
    tables: Vec<String>,

    /// Comma-separated `name=url` MCP servers to fingerprint on each
    /// commit. Each `tools/list` response gets canonicalized and hashed
    /// into the commit's `tools` dimension. Example:
    /// `--mcp search=http://localhost:8001,rag=http://localhost:8002/rpc`.
    #[arg(long, value_delimiter = ',')]
    mcp: Vec<String>,
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

    let tables = parse_tracked_tables(&args.tables)?;
    let mcp_servers = parse_mcp_spec(&args.mcp)?;
    let state = Arc::new(
        DaemonState::open(
            agentic_dir.clone(),
            args.postgres.as_deref(),
            tables,
            mcp_servers,
        )
        .await?,
    );

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
