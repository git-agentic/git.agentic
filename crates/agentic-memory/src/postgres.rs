//! Postgres + pgvector adapter — the MVP's only first-class memory backend.
//!
//! Phase 1 (this file): bulk segment build from a snapshot of current state.
//!   * `init()` validates pgvector, installs the schema-version helper and
//!     migrations table, and best-effort-creates the logical replication
//!     slot.
//!   * `bootstrap()` (called by `snapshot()`) does a single cursor scan of
//!     every tracked table and emits sealed segments into the provided
//!     ObjectStore, building a `SegmentManifest` snapshot of the current
//!     state.
//!
//! Phase 2 (`decoder.rs`, follow-up commit): logical-decoding stream that
//! keeps the segment store hot as the user's agent writes.
//!
//! Phase 3 (`snapshot.rs`, follow-up commit): atomic snapshot via
//! `pg_advisory_xact_lock` + CoW of the active head segment.

use std::sync::Arc;

use agentic_core::{FsObjectStore, Hash, ObjectKind, ObjectStore};
use serde_json::Value as Json;
use sqlx::PgPool;
use tokio::task::JoinHandle;

use crate::adapter::{MemoryAdapter, SnapshotHandle};
use crate::segment::{Segment, SegmentManifest, SegmentRef, DEFAULT_SEGMENT_TARGET_BYTES};
use crate::streamer::{self, StreamerHandle};
use crate::triggers;
use crate::{Error, Result};

/// One tracked table's identity plus its primary-key column.
#[derive(Clone, Debug)]
pub struct TrackedTable {
    pub name: String,
    /// Primary-key column name (single-column PKs only for MVP).
    pub pk: String,
}

/// Configuration passed to `PostgresAdapter::connect`.
#[derive(Clone, Debug)]
pub struct PgConfig {
    pub url: String,
    pub tables: Vec<TrackedTable>,
    /// Target sealed-segment size in bytes. Defaults to 64 MiB.
    pub segment_target_bytes: usize,
    /// Logical replication slot name. One per repo.
    pub replication_slot: String,
}

impl PgConfig {
    pub fn new(url: impl Into<String>, tables: Vec<TrackedTable>) -> Self {
        Self {
            url: url.into(),
            tables,
            segment_target_bytes: DEFAULT_SEGMENT_TARGET_BYTES,
            replication_slot: "agentic_slot".into(),
        }
    }
}

pub struct PostgresAdapter {
    pool: PgPool,
    cfg: PgConfig,
    store: Arc<FsObjectStore>,
    /// Whether `init()` confirmed logical decoding is usable. False on
    /// managed Postgres without `wal_level=logical`; the trigger fallback
    /// (in `triggers.rs`) runs instead.
    logical_decoding_available: bool,
    /// Streamer handle. Set after `init()`; `snapshot()` goes through
    /// `streamer.take_snapshot` to produce O(delta)-sized manifests.
    streamer: Option<StreamerHandle>,
    /// Streamer + poller tasks. Held so they stay alive for the
    /// adapter's lifetime; dropped when the adapter is dropped.
    #[allow(dead_code)]
    streamer_join: Option<JoinHandle<()>>,
    #[allow(dead_code)]
    poller_join: Option<JoinHandle<()>>,
}

impl PostgresAdapter {
    pub async fn connect(cfg: PgConfig, store: Arc<FsObjectStore>) -> Result<Self> {
        let pool = PgPool::connect(&cfg.url).await?;
        Ok(Self {
            pool,
            cfg,
            store,
            logical_decoding_available: false,
            streamer: None,
            streamer_join: None,
            poller_join: None,
        })
    }

    pub fn logical_decoding_available(&self) -> bool {
        self.logical_decoding_available
    }

    /// Validate the database meets MVP preconditions. Fails loudly if
    /// pgvector is missing; we intentionally don't auto-install it
    /// (CREATE EXTENSION requires superuser).
    async fn validate_pgvector(&self) -> Result<()> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT extname FROM pg_extension WHERE extname = 'vector'")
                .fetch_optional(&self.pool)
                .await?;
        if row.is_none() {
            return Err(Error::Backend(
                "pgvector extension is not installed; run \
                 `CREATE EXTENSION vector;` as a superuser before \
                 `agentic init`"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Install `agentic_schema_version()` and the `agentic_migrations`
    /// table. Both are idempotent. sqlx's prepared-statement path rejects
    /// multi-statement queries, so we send them one at a time.
    async fn install_helpers(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS agentic_migrations (
                id          serial      PRIMARY KEY,
                name        text        NOT NULL,
                applied_at  timestamptz NOT NULL DEFAULT now()
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            CREATE OR REPLACE FUNCTION agentic_schema_version()
            RETURNS text
            LANGUAGE sql
            AS $$
                SELECT coalesce(
                    (SELECT name FROM agentic_migrations ORDER BY id DESC LIMIT 1),
                    '0.0.0'
                );
            $$
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Try to create the logical replication slot. On managed Postgres
    /// without `wal_level=logical` we fall back to trigger capture.
    async fn ensure_replication_slot(&mut self) -> Result<()> {
        let row: (String,) = sqlx::query_as("SHOW wal_level")
            .fetch_one(&self.pool)
            .await?;
        if row.0 != "logical" {
            tracing::warn!(
                wal_level = row.0,
                "wal_level != logical; falling back to trigger capture"
            );
            self.logical_decoding_available = false;
            return Ok(());
        }

        let existing: Option<(String,)> =
            sqlx::query_as("SELECT slot_name FROM pg_replication_slots WHERE slot_name = $1")
                .bind(&self.cfg.replication_slot)
                .fetch_optional(&self.pool)
                .await?;
        if existing.is_some() {
            self.logical_decoding_available = true;
            return Ok(());
        }

        let created = sqlx::query("SELECT pg_create_logical_replication_slot($1, 'pgoutput')")
            .bind(&self.cfg.replication_slot)
            .execute(&self.pool)
            .await;
        match created {
            Ok(_) => {
                self.logical_decoding_available = true;
                Ok(())
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not create replication slot; trigger fallback");
                self.logical_decoding_available = false;
                Ok(())
            }
        }
    }

    /// Read every tracked table cursor-style and emit sealed segments.
    /// Returns a manifest pinning the segments by `(table, pk_lo..pk_hi)`.
    pub async fn bootstrap(&self) -> Result<SegmentManifest> {
        let schema_version = self.current_schema_version_inner().await?;
        let mut manifest = SegmentManifest::new(schema_version.clone());

        for table in &self.cfg.tables {
            self.bootstrap_table(table, &schema_version, &mut manifest)
                .await?;
        }
        Ok(manifest)
    }

    async fn bootstrap_table(
        &self,
        table: &TrackedTable,
        schema_version: &str,
        manifest: &mut SegmentManifest,
    ) -> Result<()> {
        validate_identifier(&table.name)?;
        validate_identifier(&table.pk)?;

        let sql = format!(
            "SELECT * FROM {} ORDER BY {}",
            quote_qualified(&table.name),
            quote_ident(&table.pk),
        );

        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;

        let mut current = blank_segment(&table.name, schema_version);
        let mut have_lo = false;

        for row in &rows {
            let row_json = row_to_json(row);
            let pk_value = row_json.get(&table.pk).cloned().unwrap_or(Json::Null);

            if !have_lo {
                current.pk_lo = pk_value.clone();
                have_lo = true;
            }
            current.pk_hi = pk_value.clone();
            // Wrap in the streamer's envelope shape so the restore code
            // path is uniform across bootstrap and delta segments.
            current
                .rows
                .push(serde_json::json!({"op": "insert", "row": row_json}));
            current.row_count = current.rows.len() as u64;

            if current.canonical_size() >= self.cfg.segment_target_bytes {
                self.seal_segment(&mut current, schema_version, manifest)?;
                have_lo = false;
            }
        }

        if !current.rows.is_empty() {
            self.seal_segment(&mut current, schema_version, manifest)?;
        }
        Ok(())
    }

    fn seal_segment(
        &self,
        current: &mut Segment,
        schema_version: &str,
        manifest: &mut SegmentManifest,
    ) -> Result<()> {
        // Persist segment bytes via the object store. Segments live outside
        // the typed `Object` enum (snapshot-model §1.3); we use `put_raw`
        // keyed by raw BLAKE3 of the canonical JSON.
        let bytes = current.to_canonical_bytes();
        let segment_hash: Hash = self
            .store
            .put_raw(ObjectKind::Segment, &bytes)
            .map_err(|e| Error::Backend(format!("persisting segment: {e}")))?;

        manifest.push(SegmentRef {
            table: current.table.clone(),
            pk_lo: current.pk_lo.clone(),
            pk_hi: current.pk_hi.clone(),
            segment: segment_hash,
            row_count: current.row_count,
        });

        *current = blank_segment(&current.table, schema_version);
        Ok(())
    }

    async fn current_schema_version_inner(&self) -> Result<String> {
        let row: Option<(String,)> = sqlx::query_as("SELECT agentic_schema_version()")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0).unwrap_or_else(|| "0.0.0".to_string()))
    }
}

#[async_trait::async_trait]
impl MemoryAdapter for PostgresAdapter {
    async fn init(&mut self) -> Result<()> {
        self.validate_pgvector().await?;
        self.install_helpers().await?;
        self.ensure_replication_slot().await?;

        // Trigger-based change capture is the portable path; even when
        // logical decoding is available the trigger fallback still works
        // and gives us a uniform event source for the streamer.
        triggers::install_triggers(&self.pool, &self.cfg.tables).await?;

        // One-shot baseline: bootstrap-scan every tracked table into
        // sealed segments. The resulting SegmentRefs anchor the
        // streamer's view of the world; every subsequent change rides
        // on top as delta segments.
        let baseline = self.bootstrap().await?;
        let schema_version = baseline.schema_version.clone();

        let (handle, streamer_join) = streamer::spawn(
            self.store.clone(),
            self.cfg.tables.clone(),
            self.cfg.segment_target_bytes,
            schema_version.clone(),
        );
        handle.seed_sealed(baseline.entries).await?;

        let poller_join = triggers::spawn_poller(
            self.pool.clone(),
            handle.clone(),
            triggers::DEFAULT_POLL_INTERVAL,
            self.cfg.tables.clone(),
        );

        self.streamer = Some(handle);
        self.streamer_join = Some(streamer_join);
        self.poller_join = Some(poller_join);
        Ok(())
    }

    /// Snapshot via the streamer. Before asking the streamer to seal
    /// active heads we synchronously drain `agentic_change_log` so the
    /// snapshot reflects every change that committed before this call.
    /// Without that fence we could miss events captured between the
    /// poller's last tick and this snapshot.
    async fn snapshot(&self) -> Result<SnapshotHandle> {
        let handle = self
            .streamer
            .as_ref()
            .ok_or_else(|| Error::Backend("snapshot called before init".into()))?;
        triggers::drain_to_completion(&self.pool, handle, &self.cfg.tables).await?;
        let schema_version = self.current_schema_version_inner().await?;
        let manifest = handle.take_snapshot(&schema_version).await?;
        Ok(SnapshotHandle {
            manifest,
            schema_version,
        })
    }

    async fn restore(&self, target: &SnapshotHandle) -> Result<()> {
        // Schema-version gate: the migration runner lands in a follow-up.
        // For Chunk C-part-2 we fail loudly if schema versions diverge,
        // matching ADR-0002 Decision 5's "destructive migration"
        // honesty — the operator must hand-write the reverse before
        // rollback can proceed.
        let live = self.current_schema_version_inner().await?;
        if live != target.schema_version {
            return Err(Error::SchemaMismatch {
                live,
                target: target.schema_version.clone(),
            });
        }
        crate::restore::restore_manifest(
            &self.pool,
            self.store.as_ref(),
            &target.manifest,
            &self.cfg.tables,
        )
        .await
    }

    async fn current_schema_version(&self) -> Result<String> {
        self.current_schema_version_inner().await
    }
}

fn blank_segment(table: &str, schema_version: &str) -> Segment {
    Segment {
        table: table.to_string(),
        schema_version: schema_version.to_string(),
        pk_lo: Json::Null,
        pk_hi: Json::Null,
        row_count: 0,
        rows: Vec::new(),
        embeddings: Vec::new(),
        metadata: Default::default(),
    }
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

fn quote_ident(s: &str) -> String {
    // We already validated the character set, so this is safe to inline.
    format!("\"{s}\"")
}

fn quote_qualified(s: &str) -> String {
    if let Some((schema, table)) = s.split_once('.') {
        format!("\"{schema}\".\"{table}\"")
    } else {
        format!("\"{s}\"")
    }
}

/// Convert one sqlx Postgres row to a JSON object keyed by column name.
/// Dispatches on the column's Postgres type name so the JSON value's type
/// matches what restore will need to bind back (a bigint round-trips as
/// `Json::Number`, not `Json::String`).
fn row_to_json(row: &sqlx::postgres::PgRow) -> Json {
    use sqlx::postgres::PgRow;
    use sqlx::types::JsonValue;
    use sqlx::{Column, Row, TypeInfo};

    fn json_for_column(row: &PgRow, idx: usize, ty: &str) -> Json {
        // NULL short-circuit.
        if let Ok(None) = row.try_get::<Option<JsonValue>, _>(idx) {
            return Json::Null;
        }

        match ty {
            "INT8" | "BIGINT" | "INT4" | "INT" | "INTEGER" | "INT2" | "SMALLINT" | "OID" => row
                .try_get::<i64, _>(idx)
                .map(|i| Json::Number(i.into()))
                .unwrap_or(Json::Null),
            "FLOAT4" | "REAL" | "FLOAT8" | "DOUBLE PRECISION" => row
                .try_get::<f64, _>(idx)
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Json::Number)
                .unwrap_or(Json::Null),
            "BOOL" | "BOOLEAN" => row
                .try_get::<bool, _>(idx)
                .map(Json::Bool)
                .unwrap_or(Json::Null),
            "JSON" | "JSONB" => row.try_get::<JsonValue, _>(idx).unwrap_or(Json::Null),
            // text / varchar / char / uuid / timestamptz / date / time
            // and anything else we don't special-case: round-trip as text.
            _ => row
                .try_get::<String, _>(idx)
                .map(Json::String)
                .unwrap_or(Json::Null),
        }
    }

    let mut map = serde_json::Map::new();
    for (idx, col) in row.columns().iter().enumerate() {
        let name = col.name().to_string();
        let ty = col.type_info().name().to_uppercase();
        map.insert(name, json_for_column(row, idx, &ty));
    }
    Json::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_identifier_accepts_simple_names() {
        assert!(validate_identifier("episodes").is_ok());
        assert!(validate_identifier("user_facts").is_ok());
        assert!(validate_identifier("schema.table").is_ok());
        assert!(validate_identifier("EpisodesV2").is_ok());
    }

    #[test]
    fn validate_identifier_rejects_injection() {
        assert!(validate_identifier("episodes; DROP TABLE x;").is_err());
        assert!(validate_identifier("foo bar").is_err());
        assert!(validate_identifier("\"quoted\"").is_err());
        assert!(validate_identifier("").is_err());
    }

    #[test]
    fn quote_qualified_handles_schema_prefix() {
        assert_eq!(
            quote_qualified("public.episodes"),
            "\"public\".\"episodes\""
        );
        assert_eq!(quote_qualified("episodes"), "\"episodes\"");
    }
}
