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

use std::time::Duration;

use serde_json::Value as Json;
use sqlx::{PgPool, Row};

use crate::postgres::TrackedTable;
use crate::streamer::{ChangeEvent, Op, StreamerHandle};
use crate::{Error, Result};

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
pub fn spawn_poller(
    pool: PgPool,
    streamer: StreamerHandle,
    interval: Duration,
    tables: Vec<TrackedTable>,
) -> tokio::task::JoinHandle<()> {
    // Trigger captures `schema.table`; the streamer was configured with
    // whatever string the caller put in PgConfig.tables. Map both ways
    // so events route correctly regardless of which form is in play.
    let key_of: std::collections::BTreeMap<String, String> = tables
        .iter()
        .map(|t| (t.name.clone(), bare_name(&t.name)))
        .collect();
    let bare_lookup: std::collections::BTreeMap<String, String> = tables
        .iter()
        .map(|t| (bare_name(&t.name), t.name.clone()))
        .collect();

    tokio::spawn(async move {
        tracing::info!(
            interval_ms = interval.as_millis() as u64,
            "trigger poller started"
        );
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            match drain_once(&pool, &streamer, &key_of, &bare_lookup).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(drained = n, "agentic_change_log drained"),
                Err(e) => tracing::warn!(error = %e, "change-log drain failed"),
            }
        }
    })
}

/// Synchronously drain every row in `agentic_change_log`, forwarding
/// each to the streamer. Used by `snapshot()` as a fence so the
/// streamer sees every committed change before sealing.
pub async fn drain_to_completion(
    pool: &PgPool,
    streamer: &StreamerHandle,
    tables: &[TrackedTable],
) -> Result<u64> {
    let key_of: std::collections::BTreeMap<String, String> = tables
        .iter()
        .map(|t| (t.name.clone(), bare_name(&t.name)))
        .collect();
    let bare_lookup: std::collections::BTreeMap<String, String> = tables
        .iter()
        .map(|t| (bare_name(&t.name), t.name.clone()))
        .collect();
    let mut total: u64 = 0;
    loop {
        let n = drain_once(pool, streamer, &key_of, &bare_lookup).await?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}

async fn drain_once(
    pool: &PgPool,
    streamer: &StreamerHandle,
    key_of: &std::collections::BTreeMap<String, String>,
    bare_lookup: &std::collections::BTreeMap<String, String>,
) -> Result<u64> {
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
        return Ok(0);
    }
    let schema_version: String = sqlx::query_scalar("SELECT agentic_schema_version()")
        .fetch_one(pool)
        .await
        .unwrap_or_else(|_| "0.0.0".to_string());

    let mut max_id: i64 = 0;
    let mut count: u64 = 0;
    for row in &rows {
        let id: i64 = row.try_get("id")?;
        let table_name: String = row.try_get("table_name")?;
        let op_str: String = row.try_get("op")?;
        let row_json: Json = row
            .try_get::<sqlx::types::JsonValue, _>("row")
            .unwrap_or(Json::Null);

        let key = key_of
            .get(&table_name)
            .cloned()
            .or_else(|| bare_lookup.get(&bare_name(&table_name)).cloned())
            .unwrap_or_else(|| table_name.clone());

        let op = match op_str.as_str() {
            "insert" => Op::Insert,
            "update" => Op::Update,
            "delete" => Op::Delete,
            _ => continue,
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
        count += 1;
    }

    sqlx::query("DELETE FROM public.agentic_change_log WHERE id <= $1")
        .bind(max_id)
        .execute(pool)
        .await?;
    Ok(count)
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
}
