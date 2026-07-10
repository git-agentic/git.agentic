//! Rollback orchestrator.
//!
//! Implements `agentic rollback <ref>` end-to-end:
//!
//!   1. Resolve target ref → target Commit object.
//!   2. If the target's `schema_version` differs from the live database's,
//!      run reverse SQL migrations via `MemoryAdapter::apply_reverse_migrations`
//!      (steps loaded by `crate::migrate::load_steps`).
//!   3. Restore memory: load the target's SegmentManifest, hand it to
//!      the adapter's `restore_with_guard` which TRUNCATEs each tracked
//!      table and re-INSERTs from the captured segments inside one
//!      transaction. The poller is paused for the whole window
//!      (audit §A1).
//!   4. Write prompt blobs back to disk under `<repo>/prompts/`. Any
//!      prompt file on disk that isn't in the target tree is removed.
//!   5. Tools / model: noted in the plan but not pushed to disk in MVP.
//!      Pinning into `.agentic/config.toml` lands when the operator-
//!      facing pin store does.
//!   6. Forward-record: create a new Commit C' whose dimensions match
//!      target A but whose parent is the current branch tip C. Update
//!      the branch ref to C'. History preserves the rollback action.
//!
//! Audit §S3 / §A4: this module replaces the previous monolithic
//! `rollback.rs`. The three responsibilities now live in
//! sibling files:
//!
//! * `loaders.rs` — typed `ObjectStore` readers (Commit, Tree, Blob,
//!   SegmentManifest).
//! * `writeback.rs` — FS write-back (`restore_prompts`, `sweep_orphans`)
//!   and Commit-tree readers (`read_text_blobs`, `read_model_text`).
//! * this file (`mod.rs`) — phase orchestration: validate the target
//!   shape, run schema migrations, restore memory, restore prompts,
//!   forward-record.

mod loaders;
mod writeback;

use std::path::PathBuf;

use agentic_core::approval::{verify_token, ApprovalRejection};
use agentic_core::commit::{stage_and_commit, CommitInputs};
use agentic_core::refs::HeadRef;
use agentic_core::{Commit, Hash};
use agentic_memory::adapter::SnapshotHandle;
use agentic_memory::MemoryAdapter;
use anyhow::{anyhow, Context};

use crate::migrate;
use crate::server::DaemonState;
use agentic_proto::RollbackOutput;

use loaders::{load_commit, load_manifest};
use writeback::{read_model_text, read_text_blobs, restore_prompts};

pub struct RollbackArgs {
    pub target: String,
    pub dry_run: bool,
    pub accept_data_loss: bool,
    /// Operator approval token for the ADR-0014 destructive-rollback gate.
    /// Only consulted when `accept_data_loss` is `true`.
    pub approval_token: Option<String>,
    /// Filesystem root of the repo (where `prompts/` lives).
    pub repo: PathBuf,
}

/// Rejection from the ADR-0014 destructive-rollback approval gate. Every
/// variant maps to a `RollbackForcedDataLoss` audit `decision` (emitted by
/// [`approval_gate`] before the error is returned) and is classified on the
/// wire as non-retryable `Validation` — the caller must obtain a valid
/// operator approval; retrying the identical request can never succeed.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    #[error(
        "accept_data_loss=true requires an operator approval token (ADR-0014), but this \
         daemon has no --approval-key-file configured — failing closed. Destructive rollback \
         is unavailable until an approval key is provisioned."
    )]
    KeyNotConfigured,
    #[error(
        "accept_data_loss=true cannot be authorized because the connection has no attested \
         peer UID (the daemon is running with --insecure-allow-any-uid). An approval token \
         binds to a specific worker UID (ADR-0014 Decision 2), so destructive rollback is \
         refused in this mode."
    )]
    NoPeerUid,
    #[error(
        "accept_data_loss=true requires an approval_token, but none was supplied (ADR-0014). \
         Have the operator mint one with `agentic rollback --approval <commit> --uid <uid> \
         --key-file <path>`."
    )]
    TokenRequired,
    #[error("approval token rejected: {0}")]
    Rejected(#[from] ApprovalRejection),
}

impl ApprovalError {
    /// The `RollbackForcedDataLoss` audit `decision` string (Decision 5).
    fn audit_decision(&self) -> &'static str {
        match self {
            ApprovalError::KeyNotConfigured => "rejected_no_key_configured",
            ApprovalError::NoPeerUid => "rejected_no_peer_uid",
            ApprovalError::TokenRequired => "rejected_no_token",
            ApprovalError::Rejected(r) => r.audit_decision(),
        }
    }
}

/// The redacted `token_prefix` for the audit event (Decision 5): the first
/// 8 hex chars of the HMAC tag — the part after the `<unix_ts>:` prefix.
/// That's what an operator matches against their issuance log; the leading
/// timestamp digits would not correlate. A partial leak is bounded, and the
/// operator holds the full token. A malformed token with no `:` falls back
/// to the first 8 chars of whatever was supplied, so the rejection is still
/// traceable.
fn token_prefix_for_audit(token: &str) -> String {
    let hmac_part = token.split_once(':').map(|(_, tag)| tag).unwrap_or(token);
    hmac_part.chars().take(8).collect()
}

/// Emit the ADR-0014 Decision 5 audit event for one `accept_data_loss=true`
/// attempt. Fires on EVERY branch — accepted and every rejection — so an
/// attacker probing for the gate shows up and operators can alert on
/// `decision = "rejected_*"` rates. `warn` for rejections, `info` for the
/// accepted path.
fn audit_forced_data_loss(
    peer_uid: Option<u32>,
    target_commit_hash: &str,
    decision: &str,
    token: Option<&str>,
) {
    let token_prefix = token.map(token_prefix_for_audit);
    if decision == "accepted" {
        tracing::info!(
            target: "agenticd::audit",
            event = "RollbackForcedDataLoss",
            audit_event_version = 1u32,
            ?peer_uid,
            target_commit_hash,
            decision,
            ?token_prefix,
        );
    } else {
        tracing::warn!(
            target: "agenticd::audit",
            event = "RollbackForcedDataLoss",
            audit_event_version = 1u32,
            ?peer_uid,
            target_commit_hash,
            decision,
            ?token_prefix,
        );
    }
}

/// The ADR-0014 approval gate. Called only when `accept_data_loss == true`,
/// before any object-store or Postgres work and before the dry-run branch,
/// so a rejected request has zero side effects. Emits exactly one audit
/// event and returns `Ok(())` only when a valid token is present.
fn approval_gate(
    state: &DaemonState,
    target_hash: &Hash,
    peer_uid: Option<u32>,
    approval_token: Option<&str>,
) -> Result<(), ApprovalError> {
    let commit_hash = target_hash.to_hex(); // lowercase 64-hex (Decision 2)
    let reject = |err: ApprovalError| -> ApprovalError {
        audit_forced_data_loss(peer_uid, &commit_hash, err.audit_decision(), approval_token);
        err
    };

    // (a) No key configured → fail closed (Decision 4). No override.
    let Some(key) = state.approval_key.as_ref() else {
        return Err(reject(ApprovalError::KeyNotConfigured));
    };
    // (b) No attested peer UID (insecure mode) → can't bind the token to a
    // worker, so refuse. ADR-0012 × ADR-0014 interaction; the demo never
    // hits this (Decision 4).
    let Some(uid) = peer_uid else {
        return Err(reject(ApprovalError::NoPeerUid));
    };
    // (c) Key present but no token supplied.
    let Some(token) = approval_token else {
        return Err(reject(ApprovalError::TokenRequired));
    };
    // (d) Verify.
    if let Err(rej) = verify_token(key, &commit_hash, uid, token, now_unix_seconds()) {
        return Err(reject(ApprovalError::Rejected(rej)));
    }
    // (e) Valid — audit the accepted path too.
    audit_forced_data_loss(peer_uid, &commit_hash, "accepted", approval_token);
    Ok(())
}

/// Current wall-clock in Unix seconds, for the approval TTL check.
fn now_unix_seconds() -> u64 {
    // INVARIANT: the system clock is after the Unix epoch on any real
    // deployment; `duration_since(UNIX_EPOCH)` only errors for a clock set
    // before 1970. The `unwrap_or(0)` degrades that impossible case to
    // "epoch", which makes every non-epoch token read as expired — the
    // safe (reject) direction for a security gate.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub async fn execute(
    state: std::sync::Arc<DaemonState>,
    args: RollbackArgs,
    peer_uid: Option<u32>,
) -> anyhow::Result<RollbackOutput> {
    let target_hash = state
        .refs
        .resolve(&args.target)?
        .ok_or_else(|| anyhow!("ref not found: {}", args.target))?;

    // ADR-0014 destructive-rollback approval gate. Evaluated before any
    // object-store or Postgres work and before the dry-run branch, so a
    // rejected request has zero side effects. Only destructive rollbacks
    // (`accept_data_loss = true`) pass through it.
    if args.accept_data_loss {
        approval_gate(
            &state,
            &target_hash,
            peer_uid,
            args.approval_token.as_deref(),
        )
        .map_err(anyhow::Error::new)?;
    }

    let target = load_commit(&state, &target_hash)?;
    validate_target_shape(&target, &target_hash)?;

    let mut plan: Vec<String> = Vec::new();

    let branch = match state.refs.read_head()? {
        Some(HeadRef::Branch(b)) => b,
        Some(HeadRef::Detached(_)) => {
            return Err(anyhow!(
                "rollback from a detached HEAD is not supported (Chunk C+1)"
            ));
        }
        None => {
            return Err(anyhow!(
                "repo has no HEAD; commit something before rolling back"
            ))
        }
    };
    let current_tip: Option<Hash> = state.refs.read_branch(&branch)?;

    plan.push(format!(
        "target commit {} ({})",
        target_hash.short(),
        target.message
    ));
    if let Some(c) = current_tip {
        plan.push(format!(
            "current branch tip {} → forward-record over it",
            c.short()
        ));
    }

    // -- Schema migrations + memory restore ----------------------------------
    // No adapter lock: `Arc<dyn MemoryAdapter>` methods take `&self`, and
    // exclusivity against concurrent commits comes from the daemon's
    // commit_lock (audit §C9 / §A9).
    if let Some(ref target_schema) = target.schema_version {
        let adapter: std::sync::Arc<dyn MemoryAdapter> =
            std::sync::Arc::clone(state.memory.as_ref().ok_or_else(|| {
                anyhow!("target commit has a schema_version but no memory backend is attached")
            })?);

        // Phase 1: query the backend for the live schema version and the
        // pending migration names.
        //
        // NOTE: the live-vs-target comparison here is a planning step
        // (decides whether migrations are needed and how many), not a
        // duplicate of the gate that `restore_with_guard` performs
        // against the post-migration live state (audit §S5). The
        // reverse-migration sequence is atomic inside the backend
        // (apply_reverse_migrations), so partial failures don't leave
        // intermediate live versions.
        let live_schema = adapter
            .current_schema_version()
            .await
            .context("reading live schema version")?;
        let migration_names = if live_schema != *target_schema {
            adapter
                .migrations_after(target_schema)
                .await
                .context("querying pending reverse migrations")?
        } else {
            Vec::new()
        };

        if live_schema != *target_schema {
            plan.push(format!(
                "reverse schema migrations: {live_schema} → {target_schema}"
            ));
            // Phase 2: synchronous filesystem I/O — no adapter call in
            // flight. `accept_data_loss` is forwarded so
            // `check_irreversible` can honor the operator's opt-in for
            // IRREVERSIBLE-marked migrations.
            let steps = migrate::load_steps(
                state.refs.agentic_dir(),
                &migration_names,
                args.accept_data_loss,
            )
            .context("loading reverse migration files")?;

            // Phase 3: execute — atomic inside the backend.
            if !args.dry_run {
                adapter
                    .apply_reverse_migrations(&steps)
                    .await
                    .context("running reverse migrations")?;
            }
        } else {
            plan.push(format!(
                "schema already at {target_schema} — no migrations needed"
            ));
        }

        // -- Memory ----------------------------------------------------------
        if let Some(manifest_hash) = target.memory_snapshot {
            plan.push(format!(
                "restore memory from manifest {}",
                manifest_hash.short()
            ));
            if !args.dry_run {
                let manifest = load_manifest(&state, &manifest_hash)?;
                let handle = SnapshotHandle {
                    manifest,
                    schema_version: target_schema.clone(),
                };
                // Pause the backend's data capture for the restore
                // window, then call the guard-taking restore method so
                // the quiesce discipline is visible at the call site.
                // The capture resumes when `guard` is dropped.
                // Audit anchor: §A1 / [R1] — without this the demo's
                // atomicity claim is silently false.
                let guard = adapter
                    .begin_restore()
                    .await
                    .context("pausing data capture for restore window")?;
                adapter
                    .restore_with_guard(&guard, &handle)
                    .await
                    .context("restoring memory snapshot")?;
                drop(guard);
            }
        } else {
            plan.push("no memory snapshot in target — skipping memory data restore".into());
        }
    } else {
        plan.push(
            "no schema_version in target — skipping schema migration and memory restore".into(),
        );
    }

    // -- Prompts -------------------------------------------------------------
    if let Some(prompts_hash) = target.prompts {
        plan.push(format!(
            "rewrite {} from target prompts tree {}",
            args.repo.join("prompts").display(),
            prompts_hash.short()
        ));
        if !args.dry_run {
            restore_prompts(&state, &args.repo, &prompts_hash)?;
        }
    } else {
        plan.push("no prompts in target — skipping prompts rewrite".into());
    }

    // -- Tools / model -------------------------------------------------------
    if target.tools.is_some() {
        plan.push(
            "tools manifest present in target; pinning into .agentic/config.toml is a follow-up"
                .into(),
        );
    }
    if target.model.is_some() {
        plan.push("model version captured in target — operator must redeploy if it changed".into());
    }

    if args.dry_run {
        return Ok(RollbackOutput {
            planned_steps: plan,
            executed: false,
            new_head_hash: None,
        });
    }

    // -- Forward-record ------------------------------------------------------
    // Build a fresh Commit whose dimensions match the target but whose
    // parent is the current branch tip. History shows C → C' even though
    // C' contains A's state.
    let prompts_payload = read_text_blobs(&state, target.prompts)?;
    let tools_payload = read_text_blobs(&state, target.tools)?;
    let model_text = read_model_text(&state, &target)?;

    let inputs = CommitInputs {
        author: "agentic-rollback".to_string(),
        message: format!("Rollback to {}", target_hash.short()),
        parent: current_tip,
        code_sha: target.code_sha.clone(),
        prompts: prompts_payload,
        tools: tools_payload,
        model: model_text,
        memory_snapshot: target.memory_snapshot,
        schema_version: target.schema_version.clone(),
        intent: None,
        plan: None,
        transcript: None,
        evals: None,
        cost_cents: 0,
        peer_uid,
        exempt_entropy_prefixes: state.exempt_entropy_prefixes.clone(),
    };
    let out = stage_and_commit(state.store.as_ref(), &state.refs, &branch, inputs)
        .context("forward-recording rollback commit")?;
    plan.push(format!(
        "forward-recorded rollback as {} on {}",
        out.commit_hash.short(),
        out.branch
    ));

    Ok(RollbackOutput {
        planned_steps: plan,
        executed: true,
        new_head_hash: Some(out.commit_hash.to_hex()),
    })
}

/// Reject target Commits whose `(memory_snapshot, schema_version)` pair is
/// in a shape the daemon should not have produced.
///
/// Audit B9 found that rollback silently skipped the memory restore branch
/// when `target.memory_snapshot.is_some()` and `target.schema_version.is_none()`
/// because the outer `if let Some(ref target_schema)` gated both the schema
/// migration and the memory-restore branches together. The commit-write code
/// path (`crates/agenticd/src/server.rs`) only produces commits with both
/// fields `Some` together or both `None` together — the mixed state is
/// unreachable through v1.0's normal write paths. Rather than silently
/// skipping (the bug) or pretending to restore with a fabricated
/// `schema_version` (option ii in source.md §Q2), we reject the state loudly
/// and direct the operator to file a v1.1 issue if they reached it through a
/// custom SDK or a legacy commit.
///
/// See `docs/plans/a8-reverse-migration/artifacts/implementation-decision-log.md#d-1`
/// for the full rationale and rejected alternatives.
fn validate_target_shape(target: &Commit, target_hash: &Hash) -> anyhow::Result<()> {
    if target.memory_snapshot.is_some() && target.schema_version.is_none() {
        return Err(anyhow!(
            "target commit {} has memory_snapshot but no schema_version; \
             this state should not be reachable through normal commit-write paths \
             (see ADR-0002 and docs/plans/a8-reverse-migration/source.md §Q2). \
             Refusing rollback. If you reached this through a custom SDK or a \
             legacy commit, please file a v1.1 issue.",
            target_hash.short()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_hash() -> Hash {
        // Any concrete Hash works for shape validation — the validator
        // doesn't read the object store.
        Hash::of(b"a8-test-fixture")
    }

    fn commit_with(memory_snapshot: Option<Hash>, schema_version: Option<String>) -> Commit {
        Commit {
            parent: None,
            author: "test".into(),
            timestamp: chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
            message: "test".into(),
            code_sha: None,
            prompts: None,
            tools: None,
            model: None,
            memory_snapshot,
            schema_version,
            intent: None,
            plan: None,
            transcript: None,
            evals: None,
            cost_cents: 0,
            signatures: Vec::new(),
            peer_uid: None,
        }
    }

    // AC2 — the malformed (memory_snapshot=Some, schema_version=None) shape
    // is rejected loudly. See docs/plans/a8-reverse-migration/source.md §Q2.
    #[test]
    fn validate_rejects_memory_without_schema() {
        let target = commit_with(Some(empty_hash()), None);
        let err = validate_target_shape(&target, &empty_hash()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("memory_snapshot but no schema_version"),
            "error message should name the contradiction; got: {msg}"
        );
        assert!(
            msg.contains("Q2") || msg.contains("v1.1"),
            "error message should point at the planning rationale / v1.1 work item; got: {msg}"
        );
    }

    // Sanity / regression: every other shape passes the validation.
    #[test]
    fn validate_accepts_both_some() {
        let target = commit_with(Some(empty_hash()), Some("003_add_embeddings".into()));
        assert!(validate_target_shape(&target, &empty_hash()).is_ok());
    }

    #[test]
    fn validate_accepts_both_none() {
        let target = commit_with(None, None);
        assert!(validate_target_shape(&target, &empty_hash()).is_ok());
    }

    #[test]
    fn validate_accepts_schema_without_memory() {
        // This shape is what a code-only commit with a schema baseline looks
        // like; rollback to it is well-defined (no memory restore needed).
        let target = commit_with(None, Some("002_baseline".into()));
        assert!(validate_target_shape(&target, &empty_hash()).is_ok());
    }

    // ADR-0014 Decision 5: the audited token_prefix is the HMAC tag's first
    // 8 hex chars (after the `<ts>:`), not the leading timestamp digits.
    #[test]
    fn token_prefix_uses_the_hmac_tag_not_the_timestamp() {
        assert_eq!(
            token_prefix_for_audit("1783665769:5c108bdc6825b47518600804"),
            "5c108bdc",
            "prefix must come from the hmac tag, not the timestamp"
        );
        // Malformed (no colon): best-effort first 8 of whatever was supplied.
        assert_eq!(token_prefix_for_audit("garbage-token"), "garbage-");
        // Short tag: whatever's there, without panicking.
        assert_eq!(token_prefix_for_audit("100:ab"), "ab");
    }
}
