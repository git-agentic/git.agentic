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

use crate::client::{
    resolve_repo, round_trip, round_trip_pinned, round_trip_signed, AutoToken, PeerMismatch,
};

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
        // Forward-compat: a future ADR may add a class this CLI binary
        // doesn't know about. Print it under "unknown" rather than
        // crashing the user's shell.
        ErrorClass::Unknown => "unknown",
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

/// What an issued approval token authorizes (ADR-0019 Decision 5).
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum IssueScope {
    /// `--accept-data-loss`. Also covers a plain reverse-migration rollback.
    DataLoss,
    /// A rollback that runs reverse migrations, without `--accept-data-loss`.
    ReverseMigration,
}

impl From<IssueScope> for agentic_core::approval::ApprovalScope {
    fn from(s: IssueScope) -> Self {
        match s {
            IssueScope::DataLoss => Self::DataLoss,
            IssueScope::ReverseMigration => Self::ReverseMigration,
        }
    }
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
    ///
    /// Also the operator's approval-token issuer (ADR-0014): pass
    /// `--approval <commit-hash> --uid <worker-uid> --key-file <path>
    /// [--scope reverse-migration|data-loss]` to mint a token to stdout
    /// without contacting the daemon.
    Rollback {
        /// Target ref (commit hash or branch name). Required for an actual
        /// rollback; ignored in `--approval` token-issuance mode.
        target: Option<String>,
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
        /// Operator approval token for a rollback that needs one
        /// (`--accept-data-loss` per ADR-0014, or one that runs reverse
        /// migrations per ADR-0019). Optional: when omitted, the CLI mints
        /// a token itself if it can read the approval key (`--key-file`,
        /// else `<repo>/.agentic/approval.key`).
        #[arg(long)]
        approval_token: Option<String>,
        /// Issuance mode: mint an approval token for this commit hash and
        /// print it to stdout (no daemon contact). Requires `--uid` and
        /// `--key-file`.
        #[arg(long, value_name = "COMMIT_HASH")]
        approval: Option<String>,
        /// Worker UID the minted token is bound to (issuance mode).
        #[arg(long)]
        uid: Option<u32>,
        /// Scope of the minted token (issuance mode). `reverse-migration`
        /// authorizes only a rollback that runs reverse migrations;
        /// `data-loss` (the default, as before) also authorizes
        /// `--accept-data-loss`. Hand a worker the narrowest scope.
        #[arg(long, value_enum, default_value_t = IssueScope::DataLoss)]
        scope: IssueScope,
        /// Path to the 32-byte approval key. Used by the issuance mode and
        /// to attach a token to a rollback automatically (ADR-0019).
        #[arg(long)]
        key_file: Option<PathBuf>,
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
            approval_token,
            approval,
            uid,
            scope,
            key_file,
        } => {
            if let Some(commit_hash) = approval {
                // Issuance mode: mint a token, no daemon contact.
                return cmd_mint_approval(&commit_hash, uid, key_file, scope.into());
            }
            let target = target
                .ok_or_else(|| anyhow!("rollback requires a target ref (or use --approval)"))?;
            cmd_rollback(
                &repo,
                RollbackOpts {
                    target,
                    dry_run,
                    yes,
                    accept_data_loss,
                    approval_token,
                    key_file,
                    json: cli.json,
                },
            )
            .await
        }
    }
}

/// An approval key and the uid that owns its file.
struct LoadedKey {
    key: agentic_core::approval::ApprovalKey,
    /// Owner of the key file. A token minted from this key is sent only to
    /// a daemon socket served by this uid (see `client::token_for_peer`).
    owner_uid: u32,
}

/// Read at most one byte past the key length: enough to reject a
/// wrong-length file, and a default path that points at an endless or huge
/// file cannot exhaust memory (the daemon does the same).
fn read_capped(reader: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    let limit = (agentic_core::approval::APPROVAL_KEY_LEN + 1) as u64;
    reader.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_key(key_file: &Path) -> anyhow::Result<LoadedKey> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let ctx = || format!("reading approval key {}", key_file.display());
    // O_NONBLOCK so opening a FIFO returns at once instead of waiting for a
    // writer; the regular-file check below then refuses it. O_NOFOLLOW so a
    // symlink swapped in after the default-path check is never followed.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(key_file)
        .with_context(ctx)?;
    let meta = file.metadata().with_context(ctx)?;
    if !meta.is_file() {
        return Err(anyhow!(
            "approval key {} is not a regular file",
            key_file.display()
        ));
    }
    // The same mode check as the daemon: a key that group or other can read
    // lets them mint tokens.
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(anyhow!(
            "approval key {} has mode {mode:03o}; it must not be accessible by group or \
             other. Run `chmod 600 {}`",
            key_file.display(),
            key_file.display()
        ));
    }
    let bytes = read_capped(&file).with_context(ctx)?;
    let key = agentic_core::approval::ApprovalKey::from_bytes(&bytes)
        .with_context(|| format!("approval key {} is malformed", key_file.display()))?;
    Ok(LoadedKey {
        key,
        owner_uid: meta.uid(),
    })
}

/// Mint an approval token of `scope` bound to `(commit_hash, uid, now)`.
fn mint_with_key(
    key: &agentic_core::approval::ApprovalKey,
    scope: agentic_core::approval::ApprovalScope,
    commit_hash: &str,
    uid: u32,
) -> anyhow::Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| anyhow!("system clock is before the Unix epoch"))?;
    Ok(agentic_core::approval::generate_token_scoped(
        key,
        scope,
        commit_hash,
        uid,
        now,
    ))
}

/// Mint a token of `scope` from the 32-byte key at `key_file`.
fn mint_token(
    key_file: &Path,
    scope: agentic_core::approval::ApprovalScope,
    commit_hash: &str,
    uid: u32,
) -> anyhow::Result<String> {
    mint_with_key(&read_key(key_file)?.key, scope, commit_hash, uid)
}

/// ADR-0014 operator token issuer. Prints a token to stdout. Does not
/// require the daemon.
fn cmd_mint_approval(
    commit_hash: &str,
    uid: Option<u32>,
    key_file: Option<PathBuf>,
    scope: agentic_core::approval::ApprovalScope,
) -> anyhow::Result<()> {
    let uid = uid.ok_or_else(|| anyhow!("--approval requires --uid <worker-uid>"))?;
    let key_file = key_file.ok_or_else(|| anyhow!("--approval requires --key-file <path>"))?;
    println!("{}", mint_token(&key_file, scope, commit_hash, uid)?);
    Ok(())
}

/// The key a rollback mints its own token from (ADR-0019 Decision 4): the
/// `--key-file` flag, else `<repo>/.agentic/approval.key` when it exists as a
/// regular file. A default path that is a symlink, FIFO or other special file
/// is skipped with a warning on stderr, so it cannot block the command.
fn rollback_key_path(repo: &Path, key_file: Option<PathBuf>) -> Option<PathBuf> {
    if key_file.is_some() {
        return key_file;
    }
    let default = repo.join(".agentic").join("approval.key");
    match std::fs::symlink_metadata(&default) {
        Err(_) => None,
        Ok(m) if m.is_file() => Some(default),
        Ok(_) => {
            eprintln!(
                "warning: not attaching an approval token: {} is not a regular file",
                default.display()
            );
            None
        }
    }
}

/// The caller's effective uid, which the daemon reads from the socket peer
/// credentials and binds the token to.
#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

/// The key a rollback mints its own tokens from, if there is a usable one.
/// A key the operator named with `--key-file` must work, so a bad one is an
/// error. A default file the CLI cannot use (empty or the wrong length) gets
/// a warning on stderr and no token: the daemon runs keyless on an empty
/// file and refuses to start on a wrong-length one, so there is nothing a
/// token could do. The warning goes to stderr so `--json` stdout stays
/// parseable.
fn load_rollback_key(repo: &Path, key_file: Option<PathBuf>) -> anyhow::Result<Option<LoadedKey>> {
    let explicit = key_file.is_some();
    let Some(path) = rollback_key_path(repo, key_file) else {
        return Ok(None);
    };
    match read_key(&path) {
        Ok(key) => Ok(Some(key)),
        Err(e) if !explicit => {
            eprintln!("warning: not attaching an approval token: {e:#}");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// A commit hash the CLI is willing to sign: exactly 64 lowercase hex
/// characters. The CLI signs whatever the daemon returns, so check it.
fn is_signable_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Resolve `target` to the commit hash the daemon will gate on.
async fn resolve_pinned_hash(
    repo: &Path,
    target: &str,
    key_owner_uid: u32,
) -> anyhow::Result<String> {
    let hash = match round_trip_pinned(
        repo,
        Request::ResolveRef {
            name: target.to_string(),
        },
        key_owner_uid,
        None,
    )
    .await?
    {
        Response::ResolveRef { hash } => hash,
        Response::Error {
            class,
            code,
            message,
            retryable,
        } => return Err(daemon_error(class, &code, &message, retryable)),
        other => return Err(anyhow!("unexpected response: {other:?}")),
    };
    if !is_signable_hash(&hash) {
        return Err(anyhow!(
            "daemon resolved {target:?} to {hash:?}, which is not a 64-character lowercase hex \
             commit hash; refusing to sign an approval token for it"
        ));
    }
    Ok(hash)
}

struct RollbackOpts {
    target: String,
    dry_run: bool,
    yes: bool,
    accept_data_loss: bool,
    /// Token the user supplied. Passed through unchanged.
    approval_token: Option<String>,
    key_file: Option<PathBuf>,
    json: bool,
}

async fn cmd_rollback(repo: &Path, o: RollbackOpts) -> anyhow::Result<()> {
    let RollbackOpts {
        target,
        dry_run,
        yes,
        accept_data_loss,
        approval_token,
        key_file,
        json,
    } = o;
    // ADR-0019 Decision 4. With no user-supplied token and a usable key, pin
    // the target to the resolved commit hash so the token, the preview and
    // the applied rollback all name the same commit, even if a branch moves.
    let mut key = if approval_token.is_none() {
        load_rollback_key(repo, key_file)?
    } else {
        None
    };
    // Every request of an auto-signing flow goes over a connection whose
    // peer uid is the key owner's. If one is not, give up on signing: warn,
    // carry on unsigned, and let the daemon refuse if it needs a token.
    let abandon = |e: &anyhow::Error| eprintln!("warning: not auto-signing the rollback: {e}");
    let target = match &key {
        Some(k) => match resolve_pinned_hash(repo, &target, k.owner_uid).await {
            Ok(hash) => hash,
            Err(e) if e.downcast_ref::<PeerMismatch>().is_some() => {
                abandon(&e);
                key = None;
                target
            }
            Err(e) => return Err(e),
        },
        None => target,
    };
    // ADR-0019: a data-loss token for --accept-data-loss, a reverse-migration
    // token otherwise.
    let scope = if accept_data_loss {
        agentic_core::approval::ApprovalScope::DataLoss
    } else {
        agentic_core::approval::ApprovalScope::ReverseMigration
    };
    let mint = |k: &LoadedKey| mint_with_key(&k.key, scope, &target, current_uid());
    // The apply is signed only if the preview reported that this plan needs
    // approval. Otherwise a forward migration landing after the preview
    // would be reversed under a token minted for a plan nobody saw; the
    // daemon's gate refuses it instead and the operator previews again.
    // `--yes` shows no plan, but the CLI still previews silently (when it
    // holds a key) to learn the flag.
    let mut preview_needs_approval = false;
    if !dry_run && (!yes || key.is_some()) {
        let preview = |token: Option<String>| Request::Rollback {
            target: target.clone(),
            dry_run: true,
            accept_data_loss,
            approval_token: token,
        };
        let reply = match &key {
            // A preview is a dry run, which verifies a token without
            // consuming it. A destructive preview is refused without one,
            // so it gets its own short-lived mint.
            Some(k) => {
                let token = if accept_data_loss {
                    Some(mint(k)?)
                } else {
                    None
                };
                match round_trip_pinned(repo, preview(None), k.owner_uid, token).await {
                    Err(e) if e.downcast_ref::<PeerMismatch>().is_some() => {
                        abandon(&e);
                        key = None;
                        round_trip(repo, preview(None)).await?
                    }
                    other => other?,
                }
            }
            None => round_trip(repo, preview(approval_token.clone())).await?,
        };
        let plan = match reply {
            Response::Rollback(p) => p,
            Response::Error {
                class,
                code,
                message,
                retryable,
            } => return Err(daemon_error(class, &code, &message, retryable)),
            other => return Err(anyhow!("unexpected response: {other:?}")),
        };
        // A destructive request always needs a token, even if an older
        // daemon's reply omits the field.
        preview_needs_approval = plan.approval_required || accept_data_loss;
        if !yes {
            // Show the plan, then confirm.
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
    }

    // An auto-minted token goes only on the confirmed, non-dry-run request
    // whose preview said it needs one, minted now so the time spent at the
    // prompt does not eat its window. With --dry-run there is no later
    // request, so that request is the preview and follows the preview rule.
    let apply_auto = match (&approval_token, &key) {
        (None, Some(k))
            if (!dry_run && preview_needs_approval) || (dry_run && accept_data_loss) =>
        {
            Some(AutoToken {
                token: mint(k)?,
                key_owner_uid: k.owner_uid,
            })
        }
        _ => None,
    };
    let resp = round_trip_signed(
        repo,
        Request::Rollback {
            target,
            dry_run,
            accept_data_loss,
            approval_token,
        },
        apply_auto,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_key_0600(path: &Path, bytes: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn a_group_or_other_accessible_key_is_refused_with_a_chmod_hint() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approval.key");
        write_key_0600(&path, &[7u8; 32]);
        for mode in [0o644, 0o640, 0o604, 0o660, 0o666] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let err = read_key(&path)
                .err()
                .unwrap_or_else(|| panic!("{mode:o} accepted"));
            assert!(
                format!("{err:#}").contains("chmod 600"),
                "{mode:o}: {err:#}"
            );
        }
        for mode in [0o600, 0o400] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(read_key(&path).is_ok(), "{mode:o}");
        }
    }

    /// A reader that never ends and counts what it hands out.
    struct Endless(std::cell::Cell<usize>);
    impl std::io::Read for &Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0.set(self.0.get() + buf.len());
            buf.fill(1);
            Ok(buf.len())
        }
    }

    #[test]
    fn key_read_stops_one_byte_past_the_key_length() {
        let endless = Endless(Default::default());
        let bytes = read_capped(&endless).unwrap();
        assert_eq!(bytes.len(), agentic_core::approval::APPROVAL_KEY_LEN + 1);
        assert!(endless.0.get() <= 4096, "read {} bytes", endless.0.get());
    }

    #[test]
    fn rollback_key_path_prefers_flag_then_default_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(rollback_key_path(dir.path(), None), None);
        let flag = PathBuf::from("/elsewhere/k");
        assert_eq!(
            rollback_key_path(dir.path(), Some(flag.clone())),
            Some(flag.clone())
        );
        std::fs::create_dir(dir.path().join(".agentic")).unwrap();
        let default = dir.path().join(".agentic/approval.key");
        std::fs::write(&default, [7u8; 32]).unwrap();
        assert_eq!(rollback_key_path(dir.path(), None), Some(default));
        assert_eq!(
            rollback_key_path(dir.path(), Some(flag.clone())),
            Some(flag)
        );
    }

    #[test]
    fn minted_token_verifies_for_the_commit_and_uid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("approval.key");
        write_key_0600(&path, &[7u8; 32]);
        let hash = "ab".repeat(32);
        let token = mint_token(
            &path,
            agentic_core::approval::ApprovalScope::DataLoss,
            &hash,
            1234,
        )
        .unwrap();
        let key = agentic_core::approval::ApprovalKey::from_bytes(&[7u8; 32]).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        agentic_core::approval::verify_token(&key, &hash, 1234, &token, now).unwrap();
        assert!(agentic_core::approval::verify_token(&key, &hash, 1235, &token, now).is_err());
        write_key_0600(&path, &[7u8; 5]);
        assert!(mint_token(
            &path,
            agentic_core::approval::ApprovalScope::DataLoss,
            &hash,
            1234
        )
        .is_err());
    }
}
