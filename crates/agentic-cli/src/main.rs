//! The `agentic` command-line interface.
//!
//! Thin shell over `agenticd`: every command opens the daemon socket, sends
//! one request, prints the reply.

mod client;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agentic_proto::{CommitInput, ErrorClass, Request, Response};
use anyhow::{anyhow, Context};
use clap::{Parser, Subcommand};

use crate::client::{resolve_repo, round_trip};

/// Format a structured `Response::Error` as a human-readable error string
/// suitable for `anyhow!`. The shape is `[class:code] message`, e.g.
/// `[not_found:ref_not_found] ref 'feature-x' not found`. Per ADR-0010
/// Decision 1 / Implementation plan step 4.
fn daemon_error(class: ErrorClass, code: &str, message: &str, retryable: bool) -> anyhow::Error {
    let cls = match class {
        ErrorClass::Protocol => "protocol",
        ErrorClass::Validation => "validation",
        ErrorClass::NotFound => "not_found",
        ErrorClass::Storage => "storage",
        ErrorClass::Memory => "memory",
        ErrorClass::Concurrency => "concurrency",
        ErrorClass::Internal => "internal",
    };
    let suffix = if retryable { " (retryable)" } else { "" };
    anyhow!("[{cls}:{code}]{suffix} {message}")
}

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

    /// Show a behavioral diff between two refs.
    Diff {
        /// From-ref (commit hash, "HEAD", or branch name).
        from: String,
        /// To-ref. Defaults to "HEAD".
        #[arg(default_value = "HEAD")]
        to: String,
    },

    /// Print the canonical bytes of any stored object (blob, tree, commit).
    /// Equivalent to `git cat-file`. Clients can verify the output against
    /// the hash: agentic_core::Hash::of(data) == hash.
    CatObject {
        /// Full content-addressed hash.
        hash: String,
        /// Emit raw bytes to stdout instead of a hex dump.
        #[arg(long)]
        raw: bool,
    },

    /// Roll back to a previous agent version. Forward-records the
    /// rollback as a new commit so history is preserved.
    Rollback {
        /// Target ref (commit hash or branch name).
        target: String,
        /// Show the plan without executing.
        #[arg(long)]
        dry_run: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Reserved for the migration runner: allow destructive
        /// migrations whose reverse loses data between snapshot and now.
        #[arg(long)]
        accept_data_loss: bool,
    },
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
        Command::CatObject { hash, raw } => cmd_cat_object(&repo, hash, raw).await,
        Command::Diff { from, to } => cmd_diff(&repo, from, to, cli.json).await,
        Command::Rollback {
            target,
            dry_run,
            yes,
            accept_data_loss,
        } => cmd_rollback(&repo, target, dry_run, yes, accept_data_loss, cli.json).await,
    }
}

async fn cmd_rollback(
    repo: &Path,
    target: String,
    dry_run: bool,
    yes: bool,
    accept_data_loss: bool,
    json: bool,
) -> anyhow::Result<()> {
    if !dry_run && !yes {
        // First show the plan, then confirm.
        let plan = match round_trip(
            repo,
            Request::Rollback {
                target: target.clone(),
                dry_run: true,
                accept_data_loss,
            },
        )
        .await?
        {
            Response::Rollback(p) => p,
            Response::Error {
            class,
            code,
            message,
            retryable,
        } => return Err(daemon_error(class, &code, &message, retryable)),
            other => return Err(anyhow!("unexpected response: {other:?}")),
        };
        println!("Planned rollback to {target}:");
        for step in &plan.planned_steps {
            println!("  - {step}");
        }
        eprint!("Proceed? [y/N] ");
        use std::io::{BufRead, Write};
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            return Err(anyhow!("rollback aborted by user"));
        }
    }

    let resp = round_trip(
        repo,
        Request::Rollback {
            target,
            dry_run,
            accept_data_loss,
        },
    )
    .await?;
    match resp {
        Response::Rollback(out) if json => println!("{}", serde_json::to_string(&out)?),
        Response::Rollback(out) => {
            for step in &out.planned_steps {
                println!("  - {step}");
            }
            match (out.executed, out.new_head_hash) {
                (true, Some(h)) => println!("✓ rollback complete; HEAD now at {}", &h[..7]),
                (true, None) => println!("✓ rollback executed (no new head)"),
                (false, _) => println!("(dry run; nothing executed)"),
            }
        }
        Response::Error {
            class,
            code,
            message,
            retryable,
        } => return Err(daemon_error(class, &code, &message, retryable)),
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

async fn cmd_cat_object(repo: &Path, hash: String, raw: bool) -> anyhow::Result<()> {
    let resp = round_trip(repo, Request::ReadObject { hash }).await?;
    match resp {
        Response::ObjectData {
            hash,
            object_kind,
            data,
        } => {
            if raw {
                use std::io::Write;
                std::io::stdout().write_all(&data)?;
            } else {
                println!("hash: {hash}");
                println!("kind: {object_kind}");
                println!("size: {} bytes", data.len());
                println!();
                // Hex dump — 16 bytes per line.
                for (i, chunk) in data.chunks(16).enumerate() {
                    let hex: String = chunk
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    let ascii: String = chunk
                        .iter()
                        .map(|b| {
                            if b.is_ascii_graphic() || *b == b' ' {
                                *b as char
                            } else {
                                '.'
                            }
                        })
                        .collect();
                    println!("{:08x}  {:<48}  |{ascii}|", i * 16, hex);
                }
            }
        }
        Response::Error {
            class,
            code,
            message,
            retryable,
        } => return Err(daemon_error(class, &code, &message, retryable)),
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

async fn cmd_diff(repo: &Path, from: String, to: String, json: bool) -> anyhow::Result<()> {
    let resp = round_trip(repo, Request::Diff { from, to }).await?;
    match resp {
        Response::Diff(d) if json => println!("{}", serde_json::to_string(&d)?),
        Response::Diff(d) => render_diff(&d),
        Response::Error {
            class,
            code,
            message,
            retryable,
        } => return Err(daemon_error(class, &code, &message, retryable)),
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

fn render_diff(d: &agentic_proto::DiffOutput) {
    println!("diff {} → {}", &d.from[..7], &d.to[..7]);
    if !d.prompts.is_empty() {
        println!("\nprompts:");
        for line in &d.prompts {
            println!("  {line}");
        }
    }
    if !d.tools.is_empty() {
        println!("\ntools:");
        for line in &d.tools {
            println!("  {line}");
        }
    }
    if d.model_changed {
        println!("\nmodel:    changed");
    }
    if !d.memory_summary.is_empty() {
        println!("\nmemory:   {}", d.memory_summary);
    }
    if !d.schema_summary.is_empty() {
        println!("\nschema:   {}", d.schema_summary);
    }
    if d.prompts.is_empty()
        && d.tools.is_empty()
        && !d.model_changed
        && d.memory_summary.is_empty()
        && d.schema_summary.is_empty()
    {
        println!("(no changes)");
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
        Response::Error {
            class,
            code,
            message,
            retryable,
        } => return Err(daemon_error(class, &code, &message, retryable)),
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
        Response::Error {
            class,
            code,
            message,
            retryable,
        } => return Err(daemon_error(class, &code, &message, retryable)),
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
        Response::Error {
            class,
            code,
            message,
            retryable,
        } => return Err(daemon_error(class, &code, &message, retryable)),
        other => return Err(anyhow!("unexpected response: {other:?}")),
    }
    Ok(())
}

async fn cmd_resolve(repo: &Path, name: String, json: bool) -> anyhow::Result<()> {
    let resp = round_trip(repo, Request::ResolveRef { name }).await?;
    match resp {
        Response::ResolveRef { hash } if json => println!("{{\"hash\":\"{hash}\"}}"),
        Response::ResolveRef { hash } => println!("{hash}"),
        Response::Error {
            class,
            code,
            message,
            retryable,
        } => return Err(daemon_error(class, &code, &message, retryable)),
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

// Returns raw bytes per ADR-0010 Decision 3: the wire carries
// `BTreeMap<String, Vec<u8>>` so non-UTF-8 prompts (compiled templates,
// instruction blobs with embedded NULs) survive the round-trip
// unchanged.
fn read_prompt_dir(dir: &Path) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    if !dir.exists() {
        return Ok(out);
    }
    walk(dir, dir, &mut out)?;
    Ok(out)
}

fn walk(root: &Path, here: &Path, out: &mut BTreeMap<String, Vec<u8>>) -> anyhow::Result<()> {
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
            let body = std::fs::read(&path)
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
