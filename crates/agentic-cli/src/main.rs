//! The `agentic` command-line interface.
//!
//! Thin shell over `agenticd`: every command opens the daemon socket, sends
//! one request, prints the reply.

mod client;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentic_proto::{CommitInput, Request, Response};
use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};

use crate::client::{resolve_repo, round_trip};

#[derive(Parser, Debug)]
#[command(name = "agentic", version, about = "Git for agent behavior")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Path to the agentic repo (default: search upward from cwd, like git).
    #[arg(long, global = true)]
    repo: Option<String>,

    /// Output as JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Initialise a new agentic repository in the current directory.
    Init {
        /// Postgres URL to attach as the memory backend (Chunk B).
        #[arg(long)]
        postgres: Option<String>,
    },

    /// Verify the daemon is responsive.
    Ping,

    /// Create a new commit capturing the current agent-state tuple.
    Commit {
        #[arg(short, long)]
        message: String,
        /// Directory containing prompt files. Every regular file inside it
        /// is included as a prompt blob, keyed by its relative path.
        #[arg(long, default_value = "prompts")]
        prompt_dir: PathBuf,
        /// Model version string captured into the commit.
        #[arg(long)]
        model: Option<String>,
        /// Skip the memory snapshot dimension. Useful when no backend is
        /// attached or for fast iteration on prompts alone.
        #[arg(long)]
        no_memory: bool,
    },

    /// Show commit history.
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        oneline: bool,
    },

    /// Resolve a ref to its commit hash.
    Resolve { name: String },

    /// Show the current branch + head hash.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let repo = resolve_repo(cli.repo.as_deref())?;

    match cli.command {
        Command::Init { postgres } => cmd_init(&repo, postgres.as_deref()).await,
        Command::Ping => cmd_ping(&repo, cli.json).await,
        Command::Commit {
            message,
            prompt_dir,
            model,
            no_memory,
        } => cmd_commit(&repo, message, prompt_dir, model, no_memory, cli.json).await,
        Command::Log { limit, oneline } => cmd_log(&repo, limit, oneline, cli.json).await,
        Command::Resolve { name } => cmd_resolve(&repo, name, cli.json).await,
        Command::Status => cmd_status(&repo, cli.json).await,
    }
}

async fn cmd_init(repo: &Path, postgres: Option<&str>) -> anyhow::Result<()> {
    let agentic_dir = repo.join(".agentic");
    std::fs::create_dir_all(&agentic_dir).context("creating .agentic/")?;
    std::fs::create_dir_all(repo.join("prompts")).context("creating prompts/")?;
    println!(
        "initialised agentic repo at {}{}",
        repo.display(),
        postgres
            .map(|p| format!(" (postgres={p})"))
            .unwrap_or_default()
    );
    println!(
        "next: start the daemon — `agenticd --repo {}`",
        repo.display()
    );
    Ok(())
}

async fn cmd_ping(repo: &Path, json: bool) -> anyhow::Result<()> {
    let resp = round_trip(repo, Request::Ping).await?;
    match resp {
        Response::Pong if json => println!("{{\"pong\":true}}"),
        Response::Pong => println!("pong"),
        Response::Error { message } => return Err(anyhow!(message)),
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

async fn cmd_commit(
    repo: &Path,
    message: String,
    prompt_dir: PathBuf,
    model: Option<String>,
    no_memory: bool,
    json: bool,
) -> anyhow::Result<()> {
    let resolved_dir = if prompt_dir.is_absolute() {
        prompt_dir
    } else {
        repo.join(prompt_dir)
    };
    let prompts = read_prompt_dir(&resolved_dir)?;
    let input = CommitInput {
        message,
        author: Some(default_author()),
        code_sha: read_git_head(repo).ok(),
        branch: None,
        prompts,
        mcp_servers: Vec::new(),
        model,
        no_memory,
    };
    let resp = round_trip(repo, Request::Commit(input)).await?;
    match resp {
        Response::Commit(out) if json => println!(
            "{{\"commit_hash\":\"{}\",\"branch\":\"{}\"}}",
            out.commit_hash, out.branch
        ),
        Response::Commit(out) => println!(
            "[{}] committed to {} as {}",
            &out.commit_hash[..7],
            out.branch,
            out.commit_hash
        ),
        Response::Error { message } => return Err(anyhow!(message)),
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

async fn cmd_log(repo: &Path, limit: usize, oneline: bool, json: bool) -> anyhow::Result<()> {
    let resp = round_trip(repo, Request::Log { limit }).await?;
    match resp {
        Response::Log { entries } if json => {
            println!("{}", serde_json::to_string(&entries)?);
        }
        Response::Log { entries } if oneline => {
            for e in entries {
                println!("{} {}", &e.hash[..7], e.message);
            }
        }
        Response::Log { entries } => {
            for e in entries {
                println!("commit {}", e.hash);
                println!("Author: {}", e.author);
                println!("Date:   {}", e.timestamp);
                println!();
                println!("    {}", e.message);
                println!();
            }
        }
        Response::Error { message } => return Err(anyhow!(message)),
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

async fn cmd_resolve(repo: &Path, name: String, json: bool) -> anyhow::Result<()> {
    let resp = round_trip(repo, Request::ResolveRef { name }).await?;
    match resp {
        Response::ResolveRef { hash } if json => println!("{{\"hash\":\"{hash}\"}}"),
        Response::ResolveRef { hash } => println!("{hash}"),
        Response::Error { message } => return Err(anyhow!(message)),
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

async fn cmd_status(repo: &Path, _json: bool) -> anyhow::Result<()> {
    let resp = round_trip(
        repo,
        Request::ResolveRef {
            name: "HEAD".into(),
        },
    )
    .await?;
    match resp {
        Response::ResolveRef { hash } => println!("HEAD → {hash}"),
        Response::Error { .. } => println!("HEAD → (no commits yet)"),
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

fn read_prompt_dir(dir: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    if !dir.exists() {
        return Ok(out);
    }
    walk(dir, dir, &mut out)?;
    Ok(out)
}

fn walk(root: &Path, here: &Path, out: &mut BTreeMap<String, String>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(here).with_context(|| format!("reading {}", here.display()))? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            let body = std::fs::read_to_string(&path)
                .with_context(|| format!("reading prompt {}", path.display()))?;
            out.insert(rel, body);
        }
    }
    Ok(())
}

fn default_author() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".into());
    format!("{user}@{host}")
}

/// Best-effort: read the parent Git repo's HEAD commit SHA. Not fatal on
/// failure — the commit just gets `code_sha: None`.
fn read_git_head(repo: &Path) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(repo)
        .output()?;
    if !output.status.success() {
        return Err(anyhow!("git rev-parse HEAD failed"));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
