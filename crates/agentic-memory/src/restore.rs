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

use crate::postgres::TrackedTable;
use crate::segment::{Segment, SegmentManifest};
use crate::{Error, Result};

/// Restore the database state captured by `manifest` into `pool`.
///
/// `tables` bounds which tables we touch even if the manifest happens to
/// reference others. `store` resolves segment hashes to canonical bytes.
pub async fn restore_manifest<S: ObjectStore + ?Sized>(
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
        let qualified = quote_qualified(&table.name);

        tx.execute(format!("TRUNCATE TABLE {qualified}").as_str())
            .await?;

        let mut expected_rows: u64 = 0;
        for entry in manifest.entries.iter().filter(|e| e.table == table.name) {
            let seg = segments_by_hash
                .get(&entry.segment)
                .expect("pre-loaded above");
            expected_rows += seg.row_count;
            for row in &seg.rows {
                insert_row(&mut *tx, &qualified, row).await?;
            }
        }

        let (count,): (i64,) = sqlx::query_as(&format!("SELECT count(*) FROM {qualified}"))
            .fetch_one(&mut *tx)
            .await?;
        if count as u64 != expected_rows {
            return Err(Error::Backend(format!(
                "restore validation failed for {}: expected {} rows, got {}",
                table.name, expected_rows, count
            )));
        }
    }

    tx.commit().await?;
    Ok(())
}

/// Insert one row (as a JSON object) into `qualified`. Column order is
/// derived from the row's keys in BTreeMap order; values bind in the
/// same order.
async fn insert_row<'c, E>(executor: E, qualified: &str, row: &Json) -> Result<()>
where
    E: Executor<'c, Database = Postgres>,
{
    let obj = row
        .as_object()
        .ok_or_else(|| Error::Backend(format!("segment row is not a JSON object: {row}")))?;
    if obj.is_empty() {
        return Ok(());
    }

    let sorted: BTreeMap<&String, &Json> = obj.iter().collect();
    let mut columns = String::new();
    let mut placeholders = String::new();
    let mut args = PgArguments::default();

    for (i, (col, val)) in sorted.iter().enumerate() {
        validate_identifier(col)?;
        if i > 0 {
            columns.push_str(", ");
            placeholders.push_str(", ");
        }
        columns.push('"');
        columns.push_str(col);
        columns.push('"');
        placeholders.push('$');
        placeholders.push_str(&(i + 1).to_string());
        bind_json(&mut args, val)?;
    }

    let sql = format!("INSERT INTO {qualified} ({columns}) VALUES ({placeholders})");
    sqlx::query_with(&sql, args).execute(executor).await?;
    Ok(())
}

/// Bind one `serde_json::Value` to a `PgArguments`. Postgres coerces
/// most scalar types from text on insert, so we lean on that for MVP
/// shapes (`bigint`, `text`, `boolean`, `numeric`, `jsonb`). Arrays /
/// nested objects bind as `jsonb` text.
fn bind_json(args: &mut PgArguments, value: &Json) -> Result<()> {
    match value {
        Json::Null => args.add(Option::<&str>::None),
        Json::Bool(b) => args.add(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                args.add(i);
            } else if let Some(f) = n.as_f64() {
                args.add(f);
            } else {
                args.add(n.to_string());
            }
        }
        Json::String(s) => args.add(s.clone()),
        other => args.add(sqlx::types::Json(other.clone())),
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
