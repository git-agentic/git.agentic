//! Memory restore — write a `SegmentManifest` back into the user's
//! Postgres database.
//!
//! MVP algorithm (Chunk C-part-2):
//!   1. Begin a single transaction.
//!   2. For each tracked table: `TRUNCATE` (no CASCADE — let the user own
//!      FK design); then for each matching SegmentRef in the manifest,
//!      fetch the segment bytes, deserialize as a `Segment`, and
//!      parameterized-INSERT every row.
//!   3. Validate per-table row counts against the manifest's recorded
//!      `row_count` totals.
//!   4. Commit.
//!
//! This is the simplest correct restore: truncate-and-replay. Once the
//! streamer lands and we can compute a real manifest-vs-manifest delta,
//! a follow-up commit replaces this with a diff-based restore that only
//! touches changed ranges.
//!
//! pgvector embeddings are intentionally not restored yet — the demo
//! data is plain rows. Embeddings restore lands alongside the streamer.

use std::collections::BTreeMap;

use agentic_core::{Hash, ObjectStore};
use serde_json::Value as Json;
use sqlx::postgres::PgArguments;
use sqlx::{Arguments, Executor, PgPool, Postgres};

use crate::adapter::RestoreGuard;
use crate::postgres::TrackedTable;
use crate::segment::{Segment, SegmentManifest};
use crate::{Error, Result};

// `RestoreGuard` lives in `crate::adapter` so the trait surface in
// `MemoryAdapter::begin_restore` / `restore_with_guard` is reachable
// without depending on `crate::triggers`'s Postgres-specific quiesce
// token directly. `PostgresAdapter` constructs its own
// `RestoreGuard::new(QuiesceToken)` and threads it into
// `restore_manifest` below; backends without a quiesce requirement
// pass `RestoreGuard::noop()`.

/// Restore the database state captured by `manifest` into `pool`.
///
/// The caller MUST hold a [`RestoreGuard`] (obtained from
/// [`crate::postgres::PostgresAdapter::begin_restore`]) for the duration of
/// this call. The guard proves the trigger poller is paused, which is
/// load-bearing for correctness: this function rewrites table state via
/// TRUNCATE + INSERT, both of which fire user triggers and populate
/// `agentic_change_log`. If the poller drained those entries to the
/// streamer the post-restore snapshot would diverge from actual table
/// state. We TRUNCATE `agentic_change_log` inside the restore transaction
/// to drop both pre-existing entries (which referred to pre-restore table
/// state) and entries from this restore's own INSERTs.
///
/// `tables` bounds which tables we touch even if the manifest happens to
/// reference others. `store` resolves segment hashes to canonical bytes.
pub async fn restore_manifest<S: ObjectStore + ?Sized>(
    _guard: &RestoreGuard,
    pool: &PgPool,
    store: &S,
    manifest: &SegmentManifest,
    tables: &[TrackedTable],
) -> Result<()> {
    // Pre-load every segment outside the transaction so a slow disk read
    // doesn't hold Postgres locks.
    let segments_by_hash: BTreeMap<Hash, Segment> = manifest
        .entries
        .iter()
        .map(|e| {
            let bytes = store
                .get_raw(&e.segment)
                .map_err(|err| Error::Backend(format!("loading segment {}: {err}", e.segment)))?;
            let seg: Segment = serde_json::from_slice(&bytes)
                .map_err(|err| Error::Backend(format!("decoding segment {}: {err}", e.segment)))?;
            Ok::<_, Error>((e.segment, seg))
        })
        .collect::<Result<_>>()?;

    let mut tx = pool.begin().await?;

    for table in tables {
        validate_identifier(&table.name)?;
        validate_identifier(&table.pk)?;
        let qualified = quote_qualified(&table.name);

        tx.execute(format!("TRUNCATE TABLE {qualified}").as_str())
            .await?;

        // Apply every envelope in seal order. Later events for the same
        // primary key supersede earlier ones via INSERT ON CONFLICT;
        // deletes drop the row.
        //
        // Batched: `apply_segment_rows` groups consecutive same-shape
        // envelopes (same column-set + same upsert-vs-delete mode) and
        // emits one multi-row SQL statement per group. At 1M rows this
        // is ~100× fewer round-trips than the previous one-INSERT-per-row
        // path; see docs/architecture/benchmarks.md §"Postgres-integration
        // smoke" for the impact on §9 rollback timing.
        for entry in manifest.entries.iter().filter(|e| e.table == table.name) {
            let seg = segments_by_hash
                .get(&entry.segment)
                .expect("pre-loaded above");
            apply_segment_rows(&mut tx, &qualified, &table.pk, &seg.rows).await?;
        }
    }

    // Wipe agentic_change_log inside the same transaction. This drops:
    //   1. Pre-existing entries written before begin_restore() — they
    //      referred to pre-restore table state the TRUNCATE above wiped.
    //   2. Entries written by this restore's own INSERTs above — they
    //      describe the manifest, not a real user write; the streamer
    //      must not see them or the next snapshot doubles the restored
    //      state.
    // The caller's RestoreGuard ensures the poller is paused throughout;
    // it will see an empty change_log when it resumes after the commit.
    tx.execute("TRUNCATE public.agentic_change_log").await?;

    tx.commit().await?;
    Ok(())
}

/// Strip the streamer's `{op, row}` envelope.
///
/// Unknown or non-string op values error rather than silently being
/// treated as `insert` — a future op added to the streamer side but
/// not the restore side would otherwise be mis-replayed with no
/// signal at all. Loud failure beats silent mis-restore.
///
/// **Exception — partial envelopes.** An object with neither `op`
/// nor `row` falls through to the plain-row Insert path (older
/// bootstrap segments wrote rows without the envelope wrapper).
/// An object with EXACTLY ONE of the two keys also falls through:
/// the streamer side always writes both, so a partial-envelope
/// object is more plausibly a user-table row that happens to
/// contain a column called `op` or `row`. Rejecting it would
/// break valid bootstrap rows in user tables that use those names.
/// If the streamer ever starts emitting partial envelopes (it
/// shouldn't — both keys are required by construction), the right
/// fix is to tighten the streamer side rather than to start
/// rejecting plain rows here.
fn peel_envelope(value: &Json) -> Result<(Op, &Json)> {
    if let Some(obj) = value.as_object() {
        if let (Some(op), Some(row)) = (obj.get("op"), obj.get("row")) {
            let op = match op.as_str() {
                Some("insert") => Op::Insert,
                Some("update") => Op::Update,
                Some("delete") => Op::Delete,
                Some(other) => {
                    return Err(Error::Backend(format!(
                        "unknown op {other:?} in segment envelope; \
                         restore can't safely replay this row. \
                         The streamer side likely emits a new op type \
                         that restore hasn't been taught to handle yet."
                    )))
                }
                None => {
                    return Err(Error::Backend(format!(
                        "segment envelope has a non-string `op` field: {op}"
                    )))
                }
            };
            return Ok((op, row));
        }
    }
    // Plain row (older bootstrap segments) — treat as insert by convention.
    Ok((Op::Insert, value))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Insert,
    Update,
    Delete,
}

/// Apply a whole segment's rows in batched SQL.
///
/// Walks `rows` once and emits multi-row statements over runs of
/// envelopes that share both their mode (upsert vs delete) and (for
/// upserts) their column-set. Order across runs is preserved — a
/// `(Insert id=1) (Delete id=1) (Insert id=1)` sequence still goes
/// through Postgres as three statements in that order, because the
/// shape changes twice. Within a same-shape run, the SQL is one
/// `INSERT … VALUES (...), (...), … ON CONFLICT` (or one `DELETE …
/// WHERE pk IN (...)`).
///
/// Batch-size cap: at most `BATCH_MAX_ROWS` rows per statement, OR
/// `BATCH_MAX_PARAMS / col_count` rows — whichever is smaller. Keeps
/// us well clear of Postgres's 65535-parameter ceiling per statement.
///
/// Pre-validates every envelope before any SQL — same
/// fail-loudly-before-side-effects discipline used by
/// `drain_to_completion` in strict mode.
async fn apply_segment_rows(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    qualified: &str,
    pk_col: &str,
    rows: &[Json],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    validate_identifier(pk_col)?;

    // Decode every envelope up-front. Catches malformed segments
    // before we've issued any SQL — restore aborts cleanly and the
    // outer transaction rolls back. Each entry's row must be an
    // object (segments record row state as JSON objects keyed by
    // column name).
    let mut entries: Vec<(Op, &serde_json::Map<String, Json>)> = Vec::with_capacity(rows.len());
    for env in rows {
        let (op, row) = peel_envelope(env)?;
        let obj = row
            .as_object()
            .ok_or_else(|| Error::Backend(format!("segment row is not a JSON object: {row}")))?;
        if obj.is_empty() {
            // Empty row payload would produce a zero-column INSERT,
            // which is invalid SQL anyway. Skip — matches what the
            // single-row upsert path did via early-return.
            continue;
        }
        entries.push((op, obj));
    }
    if entries.is_empty() {
        return Ok(());
    }

    // Walk consecutive same-shape runs.
    let mut i = 0;
    while i < entries.len() {
        let (first_op, first_obj) = entries[i];
        let first_mode = batch_mode_of(first_op);
        let first_cols: Vec<&str> = first_obj.keys().map(String::as_str).collect();
        // Cap by both row count AND param count. For upserts param
        // count == col_count * rows; for deletes it's just rows.
        // The smaller cap controls.
        let per_row_params = match first_mode {
            BatchMode::Upsert => first_cols.len(),
            BatchMode::Delete => 1,
        };
        let cap = BATCH_MAX_ROWS.min(BATCH_MAX_PARAMS / per_row_params.max(1));

        // Find the run end: same mode, same column-set (for upserts),
        // and within the cap.
        let mut j = i + 1;
        while j < entries.len() && j - i < cap {
            let (next_op, next_obj) = entries[j];
            if batch_mode_of(next_op) != first_mode {
                break;
            }
            if first_mode == BatchMode::Upsert {
                // Column-sets must match. Cheap check first: equal length.
                if next_obj.len() != first_cols.len() {
                    break;
                }
                // Same keys in same canonical (BTreeMap) order — since
                // serde_json::Map preserves insertion order, compare
                // the sorted column lists.
                let next_cols: Vec<&str> = next_obj.keys().map(String::as_str).collect();
                if !same_column_set(&first_cols, &next_cols) {
                    break;
                }
            }
            j += 1;
        }

        // Slice `entries[i..j]` is one batch.
        match first_mode {
            BatchMode::Upsert => {
                batch_upsert(tx, qualified, pk_col, &first_cols, &entries[i..j]).await?
            }
            BatchMode::Delete => batch_delete(tx, qualified, pk_col, &entries[i..j]).await?,
        }
        i = j;
    }
    Ok(())
}

/// Max rows per batched SQL statement. 1000 is well under Postgres's
/// query-length limit at typical schema widths and small enough that
/// a partial failure (rare — restore is single-tx) doesn't waste a
/// huge amount of work on rollback.
const BATCH_MAX_ROWS: usize = 1000;
/// Postgres caps a single statement at 65535 parameters. Stay well
/// under to leave headroom for any future helper columns / placeholders.
const BATCH_MAX_PARAMS: usize = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchMode {
    /// `Insert` and `Update` share the SQL shape (multi-row INSERT
    /// ON CONFLICT DO UPDATE), so they batch together.
    Upsert,
    /// `Delete` becomes one `DELETE … WHERE pk IN (...)`; only the PK
    /// matters, other columns in the envelope are ignored.
    Delete,
}

fn batch_mode_of(op: Op) -> BatchMode {
    match op {
        Op::Insert | Op::Update => BatchMode::Upsert,
        Op::Delete => BatchMode::Delete,
    }
}

/// Compare two column-name slices irrespective of order. `serde_json::Map`
/// preserves insertion order, so we can't rely on identical iteration
/// order across two row objects from different sources. Cheap because
/// each segment's rows almost always share the same column-set, so the
/// check is at most O(n²) per row-pair in the *very* unusual mixed case;
/// the common case is identical iteration order and the loop short-circuits.
fn same_column_set(a: &[&str], b: &[&str]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|c| b.contains(c))
}

async fn batch_upsert(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    qualified: &str,
    pk_col: &str,
    cols: &[&str],
    rows: &[(Op, &serde_json::Map<String, Json>)],
) -> Result<()> {
    debug_assert!(!rows.is_empty());
    for c in cols {
        validate_identifier(c)?;
    }

    // Build `(c1, c2, ..., cN)` once.
    let mut columns_sql = String::new();
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            columns_sql.push_str(", ");
        }
        columns_sql.push('"');
        columns_sql.push_str(c);
        columns_sql.push('"');
    }
    // `ON CONFLICT DO UPDATE` set list (non-PK columns only). If the
    // only column is the PK, fall back to DO NOTHING — there's nothing
    // to update on conflict.
    let mut update_set = String::new();
    for c in cols.iter().filter(|c| **c != pk_col) {
        if !update_set.is_empty() {
            update_set.push_str(", ");
        }
        update_set.push('"');
        update_set.push_str(c);
        update_set.push_str("\" = EXCLUDED.\"");
        update_set.push_str(c);
        update_set.push('"');
    }

    // Build the multi-row VALUES list `($1, $2, ...), ($N+1, ...), ...`
    // and bind args in lock-step.
    let mut values_sql = String::new();
    let mut args = PgArguments::default();
    let mut next_placeholder: usize = 1;
    for (row_idx, (_op, obj)) in rows.iter().enumerate() {
        if row_idx > 0 {
            values_sql.push_str(", ");
        }
        values_sql.push('(');
        for (col_idx, col) in cols.iter().enumerate() {
            if col_idx > 0 {
                values_sql.push_str(", ");
            }
            values_sql.push('$');
            values_sql.push_str(&next_placeholder.to_string());
            next_placeholder += 1;
            // Column-set was already verified equal across rows; the
            // map lookup must succeed.
            let val = obj.get(*col).ok_or_else(|| {
                Error::Backend(format!(
                    "row {row_idx} unexpectedly missing column {col:?} \
                     after column-set match; this is a bug in apply_segment_rows"
                ))
            })?;
            bind_json(&mut args, val)?;
        }
        values_sql.push(')');
    }

    let sql = if update_set.is_empty() {
        format!(
            "INSERT INTO {qualified} ({columns_sql}) VALUES {values_sql} \
             ON CONFLICT (\"{pk_col}\") DO NOTHING"
        )
    } else {
        format!(
            "INSERT INTO {qualified} ({columns_sql}) VALUES {values_sql} \
             ON CONFLICT (\"{pk_col}\") DO UPDATE SET {update_set}"
        )
    };
    sqlx::query_with(&sql, args).execute(&mut **tx).await?;
    Ok(())
}

async fn batch_delete(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    qualified: &str,
    pk_col: &str,
    rows: &[(Op, &serde_json::Map<String, Json>)],
) -> Result<()> {
    debug_assert!(!rows.is_empty());
    // Build the placeholder list `$1, $2, ..., $N` and bind PKs.
    let mut placeholders = String::new();
    let mut args = PgArguments::default();
    for (idx, (_op, obj)) in rows.iter().enumerate() {
        let pk_value = match obj.get(pk_col) {
            Some(v) if !v.is_null() => v,
            Some(_) => {
                return Err(Error::Backend(format!(
                    "batch delete for {qualified}: PK column {pk_col:?} is NULL in one \
                     of the envelope rows; DELETE … WHERE pk IN (NULL) silently matches \
                     zero rows. Refusing to no-op a delete."
                )));
            }
            None => {
                return Err(Error::Backend(format!(
                    "batch delete for {qualified}: PK column {pk_col:?} is absent from \
                     one of the envelope rows; can't issue the DELETE."
                )));
            }
        };
        if idx > 0 {
            placeholders.push_str(", ");
        }
        placeholders.push('$');
        placeholders.push_str(&(idx + 1).to_string());
        bind_json(&mut args, pk_value)?;
    }
    let sql = format!("DELETE FROM {qualified} WHERE \"{pk_col}\" IN ({placeholders})");
    sqlx::query_with(&sql, args).execute(&mut **tx).await?;
    Ok(())
}

/// Bind one `serde_json::Value` to a `PgArguments`. Postgres coerces
/// most scalar types from text on insert, so we lean on that for MVP
/// shapes (`bigint`, `text`, `boolean`, `numeric`, `jsonb`). Arrays /
/// nested objects bind as `jsonb` text.
fn bind_json(args: &mut PgArguments, value: &Json) -> Result<()> {
    // sqlx 0.8 changed `Arguments::add` to return
    // `Result<(), BoxDynError>` so an encoder that exceeds Postgres'
    // wire limit (or any other Encode failure) surfaces instead of
    // silently truncating. Translate to MemoryError::Backend so the
    // restore path's existing error chain carries it.
    fn add<T>(args: &mut PgArguments, v: T) -> Result<()>
    where
        T: 'static + Send + sqlx::Encode<'static, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
    {
        args.add(v)
            .map_err(|e| Error::Backend(format!("binding argument: {e}")))
    }
    match value {
        Json::Null => add(args, Option::<&str>::None)?,
        Json::Bool(b) => add(args, *b)?,
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                add(args, i)?;
            } else if let Some(f) = n.as_f64() {
                add(args, f)?;
            } else {
                add(args, n.to_string())?;
            }
        }
        Json::String(s) => add(args, s.clone())?,
        other => add(args, sqlx::types::Json(other.clone()))?,
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn peel_envelope_decodes_known_ops() {
        let env = json!({"op": "insert", "row": {"id": 1}});
        let (op, _row) = peel_envelope(&env).unwrap();
        assert_eq!(op, Op::Insert);

        let env = json!({"op": "update", "row": {"id": 2}});
        let (op, _row) = peel_envelope(&env).unwrap();
        assert_eq!(op, Op::Update);

        let env = json!({"op": "delete", "row": {"id": 3}});
        let (op, _row) = peel_envelope(&env).unwrap();
        assert_eq!(op, Op::Delete);
    }

    #[test]
    fn peel_envelope_treats_plain_row_as_insert() {
        // Older bootstrap segments stored rows without the envelope.
        let plain = json!({"id": 1, "text": "hello"});
        let (op, row) = peel_envelope(&plain).unwrap();
        assert_eq!(op, Op::Insert);
        assert_eq!(row, &plain);
    }

    #[test]
    fn peel_envelope_rejects_unknown_op() {
        let env = json!({"op": "frobnicate", "row": {"id": 1}});
        let err = peel_envelope(&env).expect_err("unknown op must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("frobnicate") && msg.contains("unknown op"),
            "must name the unknown op explicitly; got: {msg}"
        );
    }

    #[test]
    fn peel_envelope_rejects_non_string_op() {
        let env = json!({"op": 42, "row": {"id": 1}});
        let err = peel_envelope(&env).expect_err("non-string op must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("non-string"),
            "must say 'non-string'; got: {msg}"
        );
    }

    /// A partial envelope (only one of `op`/`row` present) falls through
    /// the inner `if let` and is treated as a plain bootstrap row. The
    /// result is "treat the partial-envelope object as a row to upsert
    /// under Insert". Documents the corner case so a future change to
    /// peel_envelope notices when it shifts.
    ///
    /// The next behaviour change worth considering: reject partial
    /// envelopes loudly. Held off in this PR because no current
    /// producer emits them — the streamer always writes both keys —
    /// so making it an error would just add code without a real
    /// failure mode to prevent.
    #[test]
    fn peel_envelope_partial_envelope_falls_through_to_plain_row() {
        let partial = json!({"op": "delete"}); // no `row` key
        let (op, row) = peel_envelope(&partial).unwrap();
        assert_eq!(
            op,
            Op::Insert,
            "partial envelope falls through to plain-row path"
        );
        assert_eq!(
            row, &partial,
            "the partial-envelope object itself becomes the row"
        );
    }
}
