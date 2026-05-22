//! Trigger-based change capture.
//!
//! Postgres's logical-decoding path is the right answer at scale, but
//! it requires `wal_level=logical` + replication-role privilege, which
//! managed providers often deny. The trigger fallback works on any
//! Postgres ≥ 12: each tracked table gets an `AFTER INSERT/UPDATE/DELETE`
//! row-level trigger that appends to `agentic_change_log`, and a polling
//! task drains the log into the streamer's mpsc channel.
//!
//! The trigger uses `AFTER … FOR EACH ROW` so the change is visible to
//! the log only after the user's transaction commits. The poller then
//! sees rows in commit order.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;
use sqlx::{PgPool, Row};

use crate::postgres::TrackedTable;
use crate::streamer::{ChangeEvent, Op, StreamerHandle};
use crate::{Error, Result};

/// A subsystem whose work can be paused for the lifetime of a returned token.
///
/// Implemented by the trigger poller so a restore window can quiesce
/// change-log draining while it rewrites table state. Audit anchor:
/// [`docs/ops/2026-05-21-agenticd-architectural-analysis.md#a1`].
#[async_trait::async_trait]
pub trait Quiesceable: Send + Sync {
    /// Pause the subsystem. The returned token holds the pause; dropping
    /// it resumes work. Multiple concurrent callers serialise — only one
    /// pause is in effect at a time, and the others wait.
    async fn pause(&self) -> QuiesceToken;
}

/// Proof-of-pause token. Held by [`crate::restore::RestoreGuard`] for the
/// duration of a restore. Wraps an `OwnedMutexGuard` so dropping the token
/// releases the pause.
pub struct QuiesceToken {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

/// Handle to the trigger poller spawned by [`spawn_poller`]. Pause it via
/// [`Quiesceable::pause`] to block change-log draining; abort the task with
/// [`PollerHandle::abort`].
pub struct PollerHandle {
    pause_lock: Arc<tokio::sync::Mutex<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl PollerHandle {
    /// Stop the poller task. Used in tests; production daemons keep the
    /// poller alive for the process lifetime.
    #[allow(dead_code)]
    pub fn abort(self) -> tokio::task::JoinHandle<()> {
        self.join.abort();
        self.join
    }
}

#[async_trait::async_trait]
impl Quiesceable for PollerHandle {
    async fn pause(&self) -> QuiesceToken {
        let guard = self.pause_lock.clone().lock_owned().await;
        QuiesceToken { _guard: guard }
    }
}

/// How often the poller wakes up to drain `agentic_change_log`.
/// Aggressive enough that snapshots see fresh data; light enough that
/// the user DB doesn't notice the polling.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How many rows the poller fetches per cycle.
pub const POLL_BATCH_SIZE: i64 = 1000;

/// Idempotently install the change-log table, capture function, and
/// per-table triggers. Safe to call on every adapter start.
///
/// We pin the change-log + capture function to `public` so the trigger
/// resolves the same table regardless of the caller's session
/// `search_path`. Without this, a user transaction running with
/// `search_path=<their_schema>,public` would write to
/// `<their_schema>.agentic_change_log` (often non-existent) instead of
/// our well-known one.
pub async fn install_triggers(pool: &PgPool, tables: &[TrackedTable]) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS public.agentic_change_log (
            id          bigserial   PRIMARY KEY,
            table_name  text        NOT NULL,
            op          text        NOT NULL CHECK (op IN ('insert','update','delete')),
            row         jsonb       NOT NULL,
            captured_at timestamptz NOT NULL DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE OR REPLACE FUNCTION public.agentic_capture() RETURNS trigger AS $body$
        BEGIN
            INSERT INTO public.agentic_change_log (table_name, op, row)
            VALUES (
                TG_TABLE_SCHEMA || '.' || TG_TABLE_NAME,
                lower(TG_OP),
                CASE TG_OP
                    WHEN 'DELETE' THEN to_jsonb(OLD)
                    ELSE to_jsonb(NEW)
                END
            );
            RETURN NULL;
        END;
        $body$ LANGUAGE plpgsql
        "#,
    )
    .execute(pool)
    .await?;

    for t in tables {
        validate_identifier(&t.name)?;
        let qualified = quote_qualified(&t.name);
        let trigger_name = trigger_name_for(&t.name);

        sqlx::query(&format!(
            "DROP TRIGGER IF EXISTS {trigger_name} ON {qualified}"
        ))
        .execute(pool)
        .await?;

        sqlx::query(&format!(
            "CREATE TRIGGER {trigger_name} \
             AFTER INSERT OR UPDATE OR DELETE ON {qualified} \
             FOR EACH ROW EXECUTE FUNCTION public.agentic_capture()"
        ))
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Spawn the poller task. Drains `agentic_change_log` on
/// `interval`-spaced ticks, forwards rows as `ChangeEvent`s to the
/// streamer, then deletes the drained range.
///
/// The returned [`PollerHandle`] implements [`Quiesceable`]: callers in a
/// restore path acquire a [`QuiesceToken`] via `pause().await` and hold it
/// for the restore window. The poller's per-tick acquisition of the same
/// internal mutex blocks until the token is dropped, so no change-log row
/// is forwarded to the streamer while the token is alive.
pub fn spawn_poller(
    pool: PgPool,
    streamer: StreamerHandle,
    interval: Duration,
    tables: Vec<TrackedTable>,
) -> PollerHandle {
    // Trigger captures `schema.table`; the streamer was configured
    // with whatever string the caller put in `PgConfig.tables`. Both
    // forms must resolve to the SAME canonical key — the configured
    // `TrackedTable.name`, which is what `streamer_loop` uses as the
    // `heads` map key. The previous shape had `key_of`'s VALUE be the
    // bare name and `bare_lookup`'s VALUE be the configured name; an
    // exact-match hit on a schema-qualified config returned the bare
    // name, missed the streamer's head, and dropped the event with
    // no signal — that bug is now closed.
    let (key_of, bare_lookup) = build_table_resolvers(&tables);

    let pause_lock = Arc::new(tokio::sync::Mutex::new(()));
    let pause_lock_for_task = pause_lock.clone();

    let join = tokio::spawn(async move {
        tracing::info!(
            interval_ms = interval.as_millis() as u64,
            "trigger poller started"
        );
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            // Acquire the pause-lock around each drain. While a restore
            // holds it via QuiesceToken, this awaits until the restore
            // completes — ensuring no change-log row written during the
            // restore window reaches the streamer.
            let _permit = pause_lock_for_task.lock().await;
            match drain_once(&pool, &streamer, &key_of, &bare_lookup, DrainMode::Lenient).await {
                Ok(DrainOutcome { processed: 0, .. }) => {}
                Ok(DrainOutcome {
                    processed,
                    forwarded,
                }) => tracing::info!(
                    processed,
                    forwarded,
                    skipped = processed - forwarded,
                    "agentic_change_log drained"
                ),
                Err(e) => tracing::warn!(error = %e, "change-log drain failed"),
            }
        }
    });

    PollerHandle { pause_lock, join }
}

/// Synchronously drain every row in `agentic_change_log`, forwarding
/// each to the streamer. Used by `snapshot()` as a strict correctness
/// fence: the streamer must see every committed change before sealing.
///
/// Strict semantics — runs every batch under [`DrainMode::Strict`]: a
/// per-batch *pre-validation pass* checks each row's decode + op +
/// tracked-table resolution before any side effect. If validation
/// fails on any row, the function returns `Err` *without* forwarding
/// any of that batch's rows to the streamer and *without* deleting
/// any rows from `agentic_change_log`. The offending row stays in the
/// log so a retry sees the same problem (until the operator fixes the
/// invariant). The poller's skip-and-delete mode would have made the
/// row vanish after one error, leaving a permanently-missed event
/// even though the user's row is still in their table.
///
/// Operator recovery from a skip-blocked snapshot: inspect the
/// surrounding error! logs to identify the offending row(s), fix the
/// underlying invariant (restore CHECK constraint, add missing
/// TrackedTable, etc.), retry the snapshot.
pub async fn drain_to_completion(
    pool: &PgPool,
    streamer: &StreamerHandle,
    tables: &[TrackedTable],
) -> Result<u64> {
    let (key_of, bare_lookup) = build_table_resolvers(tables);
    let mut total_forwarded: u64 = 0;
    loop {
        let DrainOutcome {
            processed,
            forwarded,
        } = drain_once(pool, streamer, &key_of, &bare_lookup, DrainMode::Strict).await?;
        if processed == 0 {
            break;
        }
        total_forwarded += forwarded;
    }
    Ok(total_forwarded)
}

/// How `drain_once` reacts to row-level bad data.
///
/// - `Lenient` is the poller's mode: log + skip + DELETE the bad row
///   so the loop doesn't poison-pill itself at 100 ms ticks. The
///   trade-off is that skipped rows are permanently dropped from the
///   change log.
/// - `Strict` is the snapshot fence's mode: pre-validate every row
///   in the batch before any forward or DELETE. On the first
///   skip-worthy row, return `Err` with no side effects; the row
///   stays in the log for the operator to address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DrainMode {
    Lenient,
    Strict,
}

/// Per-batch drain result. `processed` counts every row removed from
/// `agentic_change_log` (forwarded + skipped in `Lenient` mode; equal
/// to `forwarded` in `Strict` mode because skips abort the batch).
/// `forwarded` counts only the rows actually sent to the streamer as
/// `ChangeEvent`s.
struct DrainOutcome {
    processed: u64,
    forwarded: u64,
}

async fn drain_once(
    pool: &PgPool,
    streamer: &StreamerHandle,
    key_of: &std::collections::BTreeMap<String, String>,
    bare_lookup: &std::collections::BTreeMap<String, String>,
    mode: DrainMode,
) -> Result<DrainOutcome> {
    let rows = sqlx::query(
        "SELECT id, table_name, op, row \
         FROM public.agentic_change_log \
         ORDER BY id \
         LIMIT $1",
    )
    .bind(POLL_BATCH_SIZE)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        return Ok(DrainOutcome {
            processed: 0,
            forwarded: 0,
        });
    }
    // Any error here — function absent (helper not installed),
    // transient DB failure, etc. — propagates instead of silently
    // falling back to "0.0.0", which would stamp every event in the
    // batch with the baseline and surface later as a misleading
    // SchemaMismatch at restore. This is a systemic failure: bubble
    // it to the caller (`drain_to_completion` propagates with `?` to
    // `snapshot()`; the poller's loop logs and retries on the next
    // tick).
    // Preserve the underlying `sqlx::Error` as the anyhow source so
    // logs / error chains can show the exact decode/connection cause.
    // Was: `.map_err(|e| Error::Backend(format!("...: {e}")))` which
    // flattened to `String` and discarded `sqlx::Error`'s type.
    let schema_version: String = sqlx::query_scalar("SELECT agentic_schema_version()")
        .fetch_one(pool)
        .await
        .map_err(|e| {
            anyhow::Error::new(e)
                .context("drain: failed to fetch live schema_version via agentic_schema_version()")
        })?;

    // In `Strict` mode, walk every row in the batch FIRST and refuse
    // to proceed if any would skip. No streamer send_event, no DELETE
    // — pure inspection. This is the snapshot fence's contract: if
    // we can't forward every change to the streamer, don't seal a
    // snapshot that's missing them, AND don't delete the offending
    // rows so the operator can fix the invariant and retry.
    if mode == DrainMode::Strict {
        for row in &rows {
            let id: i64 = row.try_get("id")?;
            let table_name: String = row.try_get("table_name").map_err(|e| {
                Error::Backend(format!(
                    "drain (strict): change_log id={id} table_name failed to decode: {e}"
                ))
            })?;
            let op_str: String = row.try_get("op").map_err(|e| {
                Error::Backend(format!(
                    "drain (strict): change_log id={id} table={table_name:?} op failed to decode: {e}"
                ))
            })?;
            if let Err(e) = row.try_get::<sqlx::types::JsonValue, _>("row") {
                return Err(Error::Backend(format!(
                    "drain (strict): change_log id={id} table={table_name:?} `row` JSONB failed \
                     to decode: {e}; refusing snapshot fence (row preserved in log for retry)"
                )));
            }
            if key_of
                .get(&table_name)
                .or_else(|| bare_lookup.get(&bare_name(&table_name)))
                .is_none()
            {
                return Err(Error::Backend(format!(
                    "drain (strict): change_log id={id} references untracked table {table_name:?}; \
                     either add it to TrackedTable config or remove its capture trigger, then retry"
                )));
            }
            if !matches!(op_str.as_str(), "insert" | "update" | "delete") {
                return Err(Error::Backend(format!(
                    "drain (strict): change_log id={id} table={table_name:?} has unknown op {op_str:?}; \
                     CHECK constraint may have been dropped or capture trigger rewritten"
                )));
            }
        }
    }

    // Within the per-row loop (lenient mode, or strict-after-validation),
    // ROW-level bad data (decode failure, unknown op, untracked table)
    // is logged loudly and the row is SKIPPED — `max_id` still advances
    // past it so the bulk DELETE at the end cleans it up. Returning Err
    // from inside this loop would (a) leave already-forwarded rows in
    // change_log so the next tick re-fetches them (duplicate events,
    // segment bloat), and (b) make the offending row a permanent poison
    // pill because cleanup never runs. Systemic errors above this loop
    // (the schema_version fetch, the initial query) still propagate. In
    // strict mode the pre-validation pass above guarantees no row will
    // skip here.
    let mut max_id: i64 = 0;
    let mut processed: u64 = 0;
    let mut forwarded: u64 = 0;
    for row in &rows {
        // `id` is the inescapable case: without it `max_id` can't
        // advance, so the same skip-and-continue pattern below isn't
        // available. A failure here means the change_log table's
        // schema has been damaged in a way that we genuinely can't
        // recover from row-by-row; propagate and let the poller
        // surface it on its next tick.
        let id: i64 = row.try_get("id")?;

        // table_name / op_str / row_json decode failures all use the
        // same skip-and-continue pattern: log at error! (the CHECK
        // constraint or NOT NULL must have been dropped, or the
        // column type altered — all invariant breaks), advance
        // max_id past the bad row so the bulk DELETE cleans it up,
        // and bump `processed` so termination still works.
        let table_name: String = match row.try_get("table_name") {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    change_log_id = id,
                    error = %e,
                    "drain: failed to decode change_log.table_name; skipping. \
                     NOT NULL or column type on `table_name` may have been altered."
                );
                max_id = max_id.max(id);
                processed += 1;
                continue;
            }
        };
        let op_str: String = match row.try_get("op") {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    change_log_id = id,
                    table = %table_name,
                    error = %e,
                    "drain: failed to decode change_log.op; skipping. \
                     NOT NULL or column type on `op` may have been altered."
                );
                max_id = max_id.max(id);
                processed += 1;
                continue;
            }
        };

        // JSONB decode failure on the change_log row payload — wire
        // mismatch, malformed JSON, type mismatch. Skip with a loud
        // error log so the operator sees the problem without
        // poisoning the poller.
        let row_json: Json = match row.try_get::<sqlx::types::JsonValue, _>("row") {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    change_log_id = id,
                    table = %table_name,
                    error = %e,
                    "drain: failed to decode `row` JSONB; skipping (will be dropped \
                     by the bulk DELETE at end of this batch if drain completes)"
                );
                max_id = max_id.max(id);
                processed += 1;
                continue;
            }
        };

        // Map the trigger's `<schema>.<table>` form to whatever the
        // caller registered in `TrackedTable.name`. If neither form
        // matches, the row was written by a non-tracked table that
        // somehow has our capture trigger — forwarding it would
        // anchor a segment under a key no head exists for. Skip with
        // warn (this is a config gap, not tampering).
        let key = match key_of
            .get(&table_name)
            .cloned()
            .or_else(|| bare_lookup.get(&bare_name(&table_name)).cloned())
        {
            Some(k) => k,
            None => {
                tracing::warn!(
                    table = %table_name,
                    change_log_id = id,
                    "drain: change_log row references untracked table; skipping"
                );
                max_id = max_id.max(id);
                processed += 1;
                continue;
            }
        };

        let op = match op_str.as_str() {
            "insert" => Op::Insert,
            "update" => Op::Update,
            "delete" => Op::Delete,
            // An op outside the CHECK-constrained set
            // ("insert"/"update"/"delete") shouldn't be reachable —
            // the change_log CHECK constraint rejects it. If we see
            // one anyway it signals tampering (constraint dropped,
            // trigger function rewritten). Log at `error!` level —
            // louder than the untracked-table `warn!` because the
            // root cause is invariant-breaking — then skip + advance
            // max_id. Returning Err here would create a poison-pill
            // loop in the poller.
            other => {
                tracing::error!(
                    change_log_id = id,
                    table = %table_name,
                    op = %other,
                    "drain: change_log row has unknown op outside the CHECK constraint; \
                     skipping. The CHECK on agentic_change_log.op may have been dropped \
                     or the capture trigger rewritten."
                );
                max_id = max_id.max(id);
                processed += 1;
                continue;
            }
        };
        streamer
            .send_event(ChangeEvent {
                table: key,
                row: row_json,
                op,
                schema_version: schema_version.clone(),
            })
            .await?;
        max_id = max_id.max(id);
        processed += 1;
        forwarded += 1;
    }

    // Bulk-delete everything we processed (forwarded OR skipped).
    // `max_id` is the max id we saw in this batch — the `ORDER BY
    // id` (ASC) in the SELECT plus the per-row `max_id.max(id)`
    // updates mean it's also the last id we saw. Any rows still in
    // the log are strictly newer than max_id and will be picked up
    // on the next tick.
    sqlx::query("DELETE FROM public.agentic_change_log WHERE id <= $1")
        .bind(max_id)
        .execute(pool)
        .await?;
    if forwarded != processed {
        tracing::debug!(
            forwarded,
            skipped = processed - forwarded,
            "drain batch: some rows were skipped (see warn/error logs above)"
        );
    }
    // `processed` (not `forwarded`) drives `drain_to_completion`'s
    // termination — a batch of only-skipped rows must NOT falsely
    // terminate the loop while more tracked rows are still in the
    // log. `forwarded` is reported separately so the snapshot fence
    // can detect skips and refuse to seal an incomplete snapshot.
    Ok(DrainOutcome {
        processed,
        forwarded,
    })
}

fn validate_identifier(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(Error::Backend("empty identifier".into()));
    }
    for c in s.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '.') {
            return Err(Error::Backend(format!(
                "invalid character in identifier: {s:?}"
            )));
        }
    }
    Ok(())
}

fn quote_qualified(s: &str) -> String {
    if let Some((schema, table)) = s.split_once('.') {
        format!("\"{schema}\".\"{table}\"")
    } else {
        format!("\"{s}\"")
    }
}

fn trigger_name_for(table: &str) -> String {
    let bare = bare_name(table);
    format!("agentic_capture_{bare}")
}

fn bare_name(s: &str) -> String {
    s.rsplit_once('.')
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| s.to_string())
}

/// Build the two lookup maps the drain loop uses to resolve a
/// trigger-emitted `table_name` to the streamer's head key.
///
/// Both maps hold the **configured** `TrackedTable.name` as the
/// VALUE — the streamer keys its `heads` map by that string, so the
/// resolved key must match it exactly. `key_of` handles the
/// exact-match case (configured form == what the trigger emitted);
/// `bare_lookup` handles the cross-form case (configured `"episodes"`
/// vs trigger `"public.episodes"` and vice versa).
///
/// Bare-name ambiguity is detected here: if two configured tables
/// share a bare name (`schema_a.episodes` and `schema_b.episodes`),
/// the bare-name fallback can't safely pick one. The colliding
/// entry is dropped from `bare_lookup` and a `tracing::warn!` names
/// both, so subsequent drain calls only resolve via exact match for
/// either of the two — never silently misroute to whichever happens
/// to win a `BTreeMap` overwrite.
fn build_table_resolvers(
    tables: &[TrackedTable],
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
) {
    let key_of: std::collections::BTreeMap<String, String> = tables
        .iter()
        .map(|t| (t.name.clone(), t.name.clone()))
        .collect();

    // First pass: count how many configured names share each bare
    // form. Anything with count > 1 must NOT be reachable via the
    // bare-name fallback — the resolver wouldn't know which schema's
    // head to route the event to.
    let mut bare_counts: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for t in tables {
        bare_counts
            .entry(bare_name(&t.name))
            .or_default()
            .push(t.name.clone());
    }
    let mut bare_lookup: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for (bare, owners) in bare_counts {
        if owners.len() == 1 {
            bare_lookup.insert(bare, owners.into_iter().next().expect("len == 1"));
        } else {
            tracing::warn!(
                bare = %bare,
                owners = ?owners,
                "build_table_resolvers: multiple TrackedTable entries share the bare name; \
                 bare-name fallback disabled for this name — events from the trigger must \
                 emit the fully-qualified configured form to route correctly"
            );
            // Intentionally do NOT insert into bare_lookup; the drain
            // loop will treat ambiguous trigger emissions as untracked.
        }
    }

    (key_of, bare_lookup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_name_strips_schema() {
        assert_eq!(trigger_name_for("episodes"), "agentic_capture_episodes");
        assert_eq!(
            trigger_name_for("public.episodes"),
            "agentic_capture_episodes"
        );
    }

    #[test]
    fn bare_name_handles_qualified_and_bare() {
        assert_eq!(bare_name("episodes"), "episodes");
        assert_eq!(bare_name("public.episodes"), "episodes");
    }

    fn tt(name: &str) -> TrackedTable {
        TrackedTable {
            name: name.into(),
            pk: "id".into(),
        }
    }

    #[test]
    fn build_table_resolvers_canonical_keys() {
        // VALUE of both maps must be the configured TrackedTable.name
        // so the streamer's `heads` map (keyed by t.name) finds the
        // head. Pre-fix `key_of`'s VALUE was bare_name(t.name) which
        // routed schema-qualified configs to the wrong head.
        let (key_of, bare_lookup) = build_table_resolvers(&[tt("public.episodes")]);
        assert_eq!(
            key_of.get("public.episodes"),
            Some(&"public.episodes".to_string()),
            "exact-match value must be the configured name (not the bare form)"
        );
        assert_eq!(
            bare_lookup.get("episodes"),
            Some(&"public.episodes".to_string()),
            "bare-name fallback must resolve to the configured name"
        );
    }

    #[test]
    fn build_table_resolvers_drops_ambiguous_bare_names() {
        // Two configured tables sharing a bare name (across schemas)
        // must NOT be resolvable via the bare-name fallback — there's
        // no way to know which schema's head to route to. Exact match
        // still works for both.
        let (key_of, bare_lookup) =
            build_table_resolvers(&[tt("schema_a.episodes"), tt("schema_b.episodes")]);
        assert!(
            key_of.contains_key("schema_a.episodes") && key_of.contains_key("schema_b.episodes"),
            "exact match remains unaffected by bare-name ambiguity"
        );
        assert!(
            !bare_lookup.contains_key("episodes"),
            "bare 'episodes' must NOT resolve when two schemas claim it; \
             pre-fix one would silently overwrite the other"
        );
    }

    #[test]
    fn build_table_resolvers_keeps_unambiguous_bare_names() {
        // Single bare-name owner: bare-name fallback works as before.
        let (_, bare_lookup) =
            build_table_resolvers(&[tt("public.episodes"), tt("public.summaries")]);
        assert_eq!(
            bare_lookup.get("episodes"),
            Some(&"public.episodes".to_string())
        );
        assert_eq!(
            bare_lookup.get("summaries"),
            Some(&"public.summaries".to_string())
        );
    }
}
