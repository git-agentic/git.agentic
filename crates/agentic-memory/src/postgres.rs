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

use agentic_core::{Hash, ObjectKind, ObjectStore};
use serde_json::Value as Json;
use sqlx::PgPool;
use tokio::task::JoinHandle;

use crate::adapter::{MemoryAdapter, RestoreGuard, SnapshotHandle};
use crate::segment::{Segment, SegmentManifest, SegmentRef, DEFAULT_SEGMENT_TARGET_BYTES};
use crate::streamer::{self, StreamerHandle};
use crate::triggers::{self, Quiesceable};
use crate::{Error, Result};

/// Postgres `pg_advisory_lock` key for the snapshot-coordination lock.
/// Held for the duration of every `snapshot()` call so concurrent
/// snapshots (across daemons / processes) serialise instead of
/// interleaving their drain + seal phases.
///
/// The value is the ASCII bytes of `"agentic_"` packed big-endian into
/// an i64 — stable across builds and recognisable in `pg_locks`.
const SNAPSHOT_ADVISORY_LOCK_KEY: i64 = 0x6167_656e_7469_635f;

/// One tracked table's identity plus its primary-key column.
#[derive(Clone, Debug)]
pub struct TrackedTable {
    pub name: String,
    /// Primary-key column name (single-column PKs only for MVP).
    pub pk: String,
}

/// Configuration passed to `PostgresAdapter::connect`.
#[derive(Clone)]
pub struct PgConfig {
    pub url: String,
    pub tables: Vec<TrackedTable>,
    /// Target sealed-segment size in bytes. Defaults to 64 MiB.
    pub segment_target_bytes: usize,
    /// Logical replication slot name. One per repo.
    pub replication_slot: String,
    /// How often the trigger poller wakes up to drain `agentic_change_log`.
    /// Defaults to [`triggers::DEFAULT_POLL_INTERVAL`] (100 ms). Tests can
    /// extend this to prevent the poller from draining writes between
    /// setup steps.
    pub poll_interval: std::time::Duration,
}

impl PgConfig {
    pub fn new(url: impl Into<String>, tables: Vec<TrackedTable>) -> Self {
        Self {
            url: url.into(),
            tables,
            segment_target_bytes: DEFAULT_SEGMENT_TARGET_BYTES,
            replication_slot: "agentic_slot".into(),
            poll_interval: triggers::DEFAULT_POLL_INTERVAL,
        }
    }
}

impl std::fmt::Debug for PgConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgConfig")
            .field("url", &redact_password_in_url(&self.url))
            .field("tables", &self.tables)
            .field("segment_target_bytes", &self.segment_target_bytes)
            .field("replication_slot", &self.replication_slot)
            .field("poll_interval", &self.poll_interval)
            .finish()
    }
}

/// Replace the password segment of a Postgres URL with `***` for safe
/// formatting in logs and error chains. Falls back to a fully-redacted
/// placeholder if the URL cannot be parsed — fail-secure.
fn redact_password_in_url(raw: &str) -> String {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return "<redacted: unparseable url>".to_string();
    };
    let Some((userinfo, hostpart)) = rest.split_once('@') else {
        return raw.to_string();
    };
    let userinfo_redacted = match userinfo.split_once(':') {
        Some((user, _password)) => format!("{user}:***"),
        None => userinfo.to_string(),
    };
    format!("{scheme}://{userinfo_redacted}@{hostpart}")
}

pub struct PostgresAdapter {
    pool: PgPool,
    cfg: PgConfig,
    store: Arc<dyn ObjectStore + Send + Sync>,
    /// Whether `init()` confirmed logical decoding is usable. False on
    /// managed Postgres without `wal_level=logical`; the trigger fallback
    /// (in `triggers.rs`) runs instead.
    logical_decoding_available: bool,
    /// Streamer handle. Set after `init()`; `snapshot()` goes through
    /// `streamer.take_snapshot` to produce O(delta)-sized manifests.
    streamer: Option<StreamerHandle>,
    /// Streamer task join handle. Retained so the adapter can later
    /// await or abort the background task if needed; dropping the
    /// handle would only detach the task, not stop it.
    #[allow(dead_code)]
    streamer_join: Option<JoinHandle<()>>,
    /// Trigger-poller handle. Exposes [`triggers::Quiesceable`] so the
    /// rollback path can pause draining for the duration of a restore.
    /// Held for the adapter's lifetime.
    poller_handle: Option<triggers::PollerHandle>,
}

impl PostgresAdapter {
    pub async fn connect(cfg: PgConfig, store: Arc<dyn ObjectStore + Send + Sync>) -> Result<Self> {
        let pool = PgPool::connect(&cfg.url).await?;
        Ok(Self {
            pool,
            cfg,
            store,
            logical_decoding_available: false,
            streamer: None,
            streamer_join: None,
            poller_handle: None,
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
                name        text        NOT NULL UNIQUE,
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
        // Track the running encoded size incrementally rather than calling
        // `current.canonical_size()` on every iteration — that method
        // re-serialises the entire segment to JSON, which makes the
        // per-row loop O(n²) on segment size. For a 64 MiB segment_target
        // that turns 100K-row bootstraps into a >10-minute hang on a
        // laptop. The running counter is *one re-serialise per segment*
        // plus a per-row envelope-size add.
        //
        // Accuracy: the segment's encoded size has four moving parts —
        // `pk_lo` (set once on the first row), `pk_hi` (set on the
        // first row, then potentially reshaped on every later row when
        // the value's encoded text form changes — unbounded for
        // variable-length text PKs), `row_count` (JSON integer grows by
        // a digit at 10, 100, 1000, …), and the row envelopes themselves
        // (incremented exactly). We rebaseline once after both pk_lo
        // and pk_hi are set on the first row, then track the pk_hi
        // delta explicitly across subsequent rows so a text PK that
        // grows from "a" to "zzzzzzzz…" within one segment can't drift
        // the counter unboundedly. `row_count` digit growth and the
        // per-row `+ 1` JSON-array comma over-count remain bounded by
        // tens of bytes per segment — negligible against
        // `segment_target_bytes` (default 64 MiB).
        //
        // Per-row envelope-bytes add includes `+ 1` for the JSON-array
        // comma; the very first row of a segment doesn't actually have
        // a leading comma (`[X]`, not `[,X]`), so the counter
        // intentionally over-counts by 1 byte per segment. Safe
        // direction: seals one byte early at worst, never late.
        //
        // Allocation hygiene: envelope encoding reuses a single Vec
        // buffer across iterations to avoid a fresh allocation per row.
        // At 1M+ rows the per-row alloc adds up; reusing the buffer
        // keeps the bootstrap loop's allocator pressure minimal.
        //
        // Maintenance caveat: if `bootstrap_table` is ever extended to
        // populate `embeddings` or `metadata` mid-segment, the running
        // counter would silently under-count those fields. Rebaseline
        // again at the change site, or grow this comment to call them
        // out.
        let mut running_bytes: usize = current.canonical_size();
        // Encoded length of pk_hi as it currently sits in `current`. We
        // track the delta across rows so subsequent pk_hi rewrites are
        // accounted for without re-serialising the whole segment.
        let mut prev_pk_hi_bytes: usize = encoded_len(&current.pk_hi);
        let mut envelope_buf: Vec<u8> = Vec::with_capacity(256);

        for row in &rows {
            let row_json = row_to_json(row, &table.name)?;
            // The PK column must be present and non-NULL — pk_lo/pk_hi
            // anchor every segment's row range and a NULL PK would
            // corrupt the manifest's range metadata. A missing column
            // means the live table schema diverged from what
            // `TrackedTable.pk` was configured with; either way the
            // operator needs the loud failure here, not a silently
            // null-anchored segment.
            let pk_value = match row_json.get(&table.pk) {
                Some(v) if !v.is_null() => v.clone(),
                Some(_) => {
                    return Err(Error::Backend(format!(
                        "primary-key column {:?} is NULL in table {:?}; \
                         snapshots require a non-NULL PK on every row",
                        table.pk, table.name
                    )))
                }
                None => {
                    return Err(Error::Backend(format!(
                        "primary-key column {:?} is absent from table {:?}'s \
                         row schema; check TrackedTable.pk against the live schema",
                        table.pk, table.name
                    )))
                }
            };

            let first_row_of_segment = !have_lo;
            if first_row_of_segment {
                current.pk_lo = pk_value.clone();
                have_lo = true;
            }
            current.pk_hi = pk_value.clone();
            let new_pk_hi_bytes = encoded_len(&current.pk_hi);
            if first_row_of_segment {
                // Rebaseline now that BOTH `pk_lo` and `pk_hi` carry
                // their real values (blank_segment initialised both to
                // null, so the size delta from null → first PK value
                // applies on both ends of the first row). One
                // re-serialise per segment, not per row.
                running_bytes = current.canonical_size();
            } else if new_pk_hi_bytes != prev_pk_hi_bytes {
                // Apply the pk_hi delta. `saturating_add_signed` keeps
                // us out of underflow if pk_hi shrinks (rare but
                // possible if a row's PK is shorter than the previous).
                let delta = new_pk_hi_bytes as isize - prev_pk_hi_bytes as isize;
                running_bytes = running_bytes.saturating_add_signed(delta);
            }
            prev_pk_hi_bytes = new_pk_hi_bytes;

            // Wrap in the streamer's envelope shape so the restore code
            // path is uniform across bootstrap and delta segments.
            let envelope = serde_json::json!({"op": "insert", "row": row_json});
            // Account for the envelope's encoded bytes BEFORE pushing.
            // Reuse `envelope_buf` rather than allocating per row.
            //
            // INVARIANT: `envelope` is a `serde_json::Value` we just
            // constructed from `{"op": "insert", "row": <row_json>}`.
            // `row_json` is the output of `row_to_json`, which only
            // produces Value variants serde_json can encode. The same
            // discipline `Segment::to_canonical_bytes` already relies on
            // for its `.expect(...)`. Swallowing a serialise failure
            // would under-count `running_bytes`, prevent sealing, and
            // produce oversized segments silently — fail loudly.
            envelope_buf.clear();
            serde_json::to_writer(&mut envelope_buf, &envelope)
                .expect("envelope JSON encoding cannot fail; see INVARIANT above");
            let envelope_bytes = envelope_buf.len() + 1;
            running_bytes = running_bytes.saturating_add(envelope_bytes);

            current.rows.push(envelope);
            current.row_count = current.rows.len() as u64;

            if running_bytes >= self.cfg.segment_target_bytes {
                self.seal_segment(&mut current, schema_version, manifest)?;
                have_lo = false;
                // After seal the segment is reset to its blank shape
                // with pk_hi = null. Reset the prev-pk_hi tracker so
                // the next row's delta is computed against the new
                // baseline rather than carrying over the previous
                // segment's last pk_hi length.
                prev_pk_hi_bytes = encoded_len(&current.pk_hi);
                // `running_bytes` is left as-is; the
                // `first_row_of_segment` branch overwrites it on the
                // next iteration via `current.canonical_size()`.
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
        // `agentic_schema_version()` is a SQL function installed by
        // `install_helpers` that always returns exactly one row
        // (COALESCE → `'0.0.0'` when no migrations are recorded), so
        // zero rows here would mean the helper wasn't installed —
        // a precondition violation, not a "no migrations applied"
        // signal. `fetch_one` surfaces that loudly instead of
        // silently falling back to `"0.0.0"` and tagging snapshots
        // with the baseline (which would then pass the equality
        // check at restore and skip reverse migrations).
        let (v,): (String,) = sqlx::query_as("SELECT agentic_schema_version()")
            .fetch_one(&self.pool)
            .await?;
        Ok(v)
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

        let poller_handle = triggers::spawn_poller(
            self.pool.clone(),
            handle.clone(),
            self.cfg.poll_interval,
            self.cfg.tables.clone(),
        );

        self.streamer = Some(handle);
        self.streamer_join = Some(streamer_join);
        self.poller_handle = Some(poller_handle);
        Ok(())
    }

    /// Snapshot via the streamer. Three things have to be true for the
    /// resulting manifest to faithfully reflect database state at the
    /// moment of call:
    ///
    ///   1. **No two snapshots interleave.** The in-process streamer
    ///      task already serialises events and snapshot RPCs through a
    ///      single channel — that gives single-daemon atomicity. For
    ///      cross-process / multi-daemon coordination we hold a Postgres
    ///      advisory lock (`pg_advisory_lock`) for the whole snapshot
    ///      window.
    ///   2. **Every committed change has been forwarded to the streamer
    ///      before sealing.** `triggers::drain_to_completion` is the
    ///      synchronous fence that guarantees this.
    ///   3. **Active heads are sealed before reading the manifest.**
    ///      The streamer's `take_snapshot` RPC does this.
    ///
    /// The advisory lock is taken on a **dedicated** `PgConnection`
    /// (via `Connection::connect`, not via the pool). This is the
    /// cancellation-safety contract: if the snapshot future is dropped
    /// mid-flight, the connection drops with it, the Postgres session
    /// ends, and the lock releases automatically. A pooled connection
    /// would otherwise return to the pool still holding the lock and
    /// block every subsequent snapshot indefinitely.
    async fn snapshot(&self) -> Result<SnapshotHandle> {
        use sqlx::Connection;
        let handle = self
            .streamer
            .as_ref()
            .ok_or_else(|| Error::Backend("snapshot called before init".into()))?;

        // Dedicated session: dropped at end of scope (or on cancellation)
        // → Postgres ends the session → advisory lock released.
        let mut conn = sqlx::postgres::PgConnection::connect(&self.cfg.url).await?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(SNAPSHOT_ADVISORY_LOCK_KEY)
            .execute(&mut conn)
            .await?;

        let result = async {
            triggers::drain_to_completion(&self.pool, handle, &self.cfg.tables).await?;
            let schema_version = self.current_schema_version_inner().await?;
            let manifest = handle.take_snapshot(&schema_version).await?;
            Ok::<_, Error>(SnapshotHandle {
                manifest,
                schema_version,
            })
        }
        .await;

        // `pg_advisory_unlock` returns false if the session didn't hold
        // the lock — surface that explicitly so lock-state bugs are
        // diagnosable. We log rather than fail so a release miss doesn't
        // mask a real snapshot error (and the dedicated connection
        // dropping below releases anything we missed anyway).
        match sqlx::query_scalar::<_, Option<bool>>("SELECT pg_advisory_unlock($1)")
            .bind(SNAPSHOT_ADVISORY_LOCK_KEY)
            .fetch_one(&mut conn)
            .await
        {
            Ok(Some(true)) => {}
            Ok(other) => tracing::warn!(
                returned = ?other,
                "pg_advisory_unlock returned non-true — session may not have held the lock"
            ),
            Err(e) => tracing::warn!(error = %e, "releasing snapshot advisory lock"),
        }

        result
    }

    async fn restore(&self, target: &SnapshotHandle) -> Result<()> {
        // The trait method is the convenience entry-point: it pauses the
        // trigger poller via `begin_restore`, then delegates to the
        // guard-taking method. Callers that need to make the quiesce
        // window explicit (e.g. `agenticd`'s rollback path) call
        // `begin_restore` + `restore_with_guard` directly.
        let guard = self.begin_restore().await?;
        self.restore_with_guard(&guard, target).await
    }

    async fn current_schema_version(&self) -> Result<String> {
        self.current_schema_version_inner().await
    }

    async fn migrations_after(&self, target_name: &str) -> Result<Vec<String>> {
        if target_name == "0.0.0" {
            let rows: Vec<(String,)> =
                sqlx::query_as("SELECT name FROM agentic_migrations ORDER BY id DESC")
                    .fetch_all(&self.pool)
                    .await?;
            return Ok(rows.into_iter().map(|(n,)| n).collect());
        }

        // Check the target exists so we don't silently return zero rows.
        let exists: Option<(i32,)> =
            sqlx::query_as("SELECT id FROM agentic_migrations WHERE name = $1")
                .bind(target_name)
                .fetch_optional(&self.pool)
                .await?;
        if exists.is_none() {
            return Err(Error::Other(anyhow::anyhow!(
                "target schema_version {:?} was never recorded in agentic_migrations; \
                 cannot determine which migrations to reverse",
                target_name
            )));
        }

        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM agentic_migrations \
             WHERE id > (SELECT id FROM agentic_migrations WHERE name = $1) \
             ORDER BY id DESC",
        )
        .bind(target_name)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(n,)| n).collect())
    }

    /// Begin a restore window. The Postgres backend pauses its trigger
    /// poller so user-side TRUNCATE+INSERT during restore isn't
    /// re-streamed; the abstract [`RestoreGuard`] holds the
    /// resume-on-drop token. Audit anchor:
    /// [§A1](../../../../docs/ops/2026-05-21-agenticd-architectural-analysis.md#a1).
    /// Errors if the adapter has not been initialised (no poller running).
    async fn begin_restore(&self) -> Result<RestoreGuard> {
        let poller = self.poller_handle.as_ref().ok_or_else(|| {
            Error::Backend(
                "begin_restore called before init() — no trigger poller is running".into(),
            )
        })?;
        let token = poller.pause().await;
        Ok(RestoreGuard::new(token))
    }

    async fn restore_with_guard(
        &self,
        guard: &RestoreGuard,
        target: &SnapshotHandle,
    ) -> Result<()> {
        let live = self.current_schema_version_inner().await?;
        if live != target.schema_version {
            return Err(Error::SchemaMismatch {
                live,
                target: target.schema_version.clone(),
            });
        }
        crate::restore::restore_manifest(
            guard,
            &self.pool,
            self.store.as_ref(),
            &target.manifest,
            &self.cfg.tables,
        )
        .await
    }
}

impl PostgresAdapter {
    /// Begin a Postgres transaction for an atomic reverse-migration sequence.
    ///
    /// The caller threads the returned `Transaction` through one or more
    /// [`Self::apply_down_migration_tx`] calls and finishes with `tx.commit()`.
    /// Dropping the transaction without committing rolls back every step.
    ///
    /// Not a trait method in v1.0: sqlx 0.8's `Executor<'c>` HRTBs don't
    /// unify across async_trait's boxed-future elision when a
    /// `Transaction<'_, Postgres>` is borrowed across awaits inside the
    /// elaborated `Pin<Box<dyn Future + Send + '_>>`. `agenticd::migrate::run_reverse`
    /// uses these inherent methods directly. When a second real backend
    /// lands we revisit the trait shape with the right abstraction.
    pub async fn begin_reverse_tx(&self) -> Result<sqlx::Transaction<'_, sqlx::Postgres>> {
        Ok(self.pool.begin().await?)
    }

    /// Execute one reverse migration step against the caller's transaction:
    /// run `sql`, then `DELETE FROM agentic_migrations WHERE name = $1`.
    ///
    /// Both statements run on the same Postgres connection as the caller's
    /// outer transaction, so a `Drop` or `tx.rollback()` undoes every step
    /// in the sequence, not just the failing one.
    ///
    /// DDL in Postgres is transactional for most statements. Non-transactional
    /// DDL (`CREATE INDEX CONCURRENTLY`, etc.) will error inside the
    /// transaction, which is the correct failure mode — the migration file
    /// must be rewritten.
    pub async fn apply_down_migration_tx<'c>(
        &self,
        tx: &mut sqlx::Transaction<'c, sqlx::Postgres>,
        name: &str,
        sql: &str,
    ) -> Result<()> {
        sqlx::raw_sql(sql)
            .execute(&mut **tx)
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("executing down migration {name:?}: {e}")))?;
        let delete_result = sqlx::query("DELETE FROM agentic_migrations WHERE name = $1")
            .bind(name)
            .execute(&mut **tx)
            .await?;
        let deleted_rows = delete_result.rows_affected();
        if deleted_rows != 1 {
            return Err(Error::Other(anyhow::anyhow!(
                "expected to delete exactly 1 agentic_migrations row for down migration {name:?}, deleted {deleted_rows}"
            )));
        }
        Ok(())
    }
}

/// Encoded JSON byte length of a single `Value`. Used by the bootstrap
/// loop to track `pk_hi` size deltas across rows without re-serialising
/// the whole segment.
///
/// INVARIANT: callers pass either `Json::Null` (from `blank_segment`) or
/// a value extracted from a `row_to_json` output, which only produces
/// Value variants `serde_json` can encode — the same discipline
/// `Segment::to_canonical_bytes` and the envelope-encoding site above
/// rely on. A silent fallback to a default would let `prev_pk_hi_bytes`
/// drift, `running_bytes` lose accuracy, and oversized segments seal
/// — exactly the failure mode the envelope-site `.expect()` was added
/// to prevent. Fail loudly here too.
fn encoded_len(v: &Json) -> usize {
    serde_json::to_vec(v)
        .expect("Json value JSON encoding cannot fail; see INVARIANT above")
        .len()
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
///
/// Decode failures propagate as `Error::Backend` carrying the column name
/// and Postgres type — the rollback contract is "what the snapshot stored
/// is what we restore", and silently substituting `Json::Null` for a
/// failed decode would let a contaminated snapshot land in the object
/// store with no signal to the operator. The pre-revisit behaviour
/// (`.unwrap_or(Json::Null)` on every arm) was flagged in PR #88's
/// review as a data-corruption-class bug.
fn row_to_json(row: &sqlx::postgres::PgRow, table: &str) -> Result<Json> {
    use sqlx::postgres::PgRow;
    use sqlx::types::JsonValue;
    use sqlx::{Column, Row, TypeInfo};

    // Table name is carried into the error so multi-table schemas can be
    // diagnosed without grepping logs to figure out which table the
    // decode failure came from.
    fn decode_err(table: &str, col: &str, ty: &str, e: sqlx::Error) -> Error {
        Error::Backend(format!(
            "table {table:?} column {col:?} (Postgres type {ty}) failed to decode: {e}"
        ))
    }

    fn json_for_column(row: &PgRow, idx: usize, table: &str, col: &str, ty: &str) -> Result<Json> {
        // NULL short-circuit. `try_get::<Option<_>, _>` distinguishes the
        // "value is NULL" case (Ok(None)) from a decode failure
        // (Err(_)). Only Ok(None) is treated as NULL; an actual decode
        // error continues into the typed branch where it surfaces.
        if matches!(row.try_get::<Option<JsonValue>, _>(idx), Ok(None)) {
            return Ok(Json::Null);
        }

        match ty {
            // ── integers — decode through the right native width, then
            // widen into i64 for JSON Number. sqlx 0.8's decoding is
            // strict: an `id int` (INT4 on the wire) won't decode
            // through `i64`. Picking the right typed read per Postgres
            // type avoids snapshot regressing on common schemas now
            // that decode errors propagate.
            "INT8" | "BIGINT" => row
                .try_get::<i64, _>(idx)
                .map(|i| Json::Number(i.into()))
                .map_err(|e| decode_err(table, col, ty, e)),
            "INT4" | "INT" | "INTEGER" => row
                .try_get::<i32, _>(idx)
                .map(|i| Json::Number(i64::from(i).into()))
                .map_err(|e| decode_err(table, col, ty, e)),
            "INT2" | "SMALLINT" => row
                .try_get::<i16, _>(idx)
                .map(|i| Json::Number(i64::from(i).into()))
                .map_err(|e| decode_err(table, col, ty, e)),
            "OID" => row
                .try_get::<sqlx::postgres::types::Oid, _>(idx)
                .map(|o| Json::Number(i64::from(o.0).into()))
                .map_err(|e| decode_err(table, col, ty, e)),
            // ── floats — JSON can't represent NaN/±Inf, error out ────
            "FLOAT4" | "REAL" => {
                let raw: f32 = row
                    .try_get(idx)
                    .map_err(|e| decode_err(table, col, ty, e))?;
                serde_json::Number::from_f64(f64::from(raw))
                    .map(Json::Number)
                    .ok_or_else(|| {
                        Error::Backend(format!(
                            "table {table:?} column {col:?} ({ty}) is non-finite ({raw}); \
                             JSON cannot represent NaN/Inf"
                        ))
                    })
            }
            "FLOAT8" | "DOUBLE PRECISION" => {
                let raw: f64 = row
                    .try_get(idx)
                    .map_err(|e| decode_err(table, col, ty, e))?;
                serde_json::Number::from_f64(raw)
                    .map(Json::Number)
                    .ok_or_else(|| {
                        Error::Backend(format!(
                            "table {table:?} column {col:?} ({ty}) is non-finite ({raw}); \
                             JSON cannot represent NaN/Inf"
                        ))
                    })
            }
            // ── bool ────────────────────────────────────────────────
            "BOOL" | "BOOLEAN" => row
                .try_get::<bool, _>(idx)
                .map(Json::Bool)
                .map_err(|e| decode_err(table, col, ty, e)),
            // ── nested JSON pass-through ────────────────────────────
            "JSON" | "JSONB" => row
                .try_get::<JsonValue, _>(idx)
                .map_err(|e| decode_err(table, col, ty, e)),
            // ── UUID via sqlx::types::Uuid; stringify ────────────────
            "UUID" => row
                .try_get::<sqlx::types::Uuid, _>(idx)
                .map(|u| Json::String(u.to_string()))
                .map_err(|e| decode_err(table, col, ty, e)),
            // ── chrono date/time types; stringify ───────────────────
            "TIMESTAMPTZ" => row
                .try_get::<sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>, _>(idx)
                .map(|t| Json::String(t.to_rfc3339()))
                .map_err(|e| decode_err(table, col, ty, e)),
            "TIMESTAMP" => row
                .try_get::<sqlx::types::chrono::NaiveDateTime, _>(idx)
                .map(|t| Json::String(t.to_string()))
                .map_err(|e| decode_err(table, col, ty, e)),
            "DATE" => row
                .try_get::<sqlx::types::chrono::NaiveDate, _>(idx)
                .map(|d| Json::String(d.to_string()))
                .map_err(|e| decode_err(table, col, ty, e)),
            "TIME" => row
                .try_get::<sqlx::types::chrono::NaiveTime, _>(idx)
                .map(|t| Json::String(t.to_string()))
                .map_err(|e| decode_err(table, col, ty, e)),
            // ── BYTEA — hex-encode (Postgres' bytea_output = hex
            // convention) so JSON can carry it round-trippably ──────
            "BYTEA" => row
                .try_get::<Vec<u8>, _>(idx)
                .map(|b| Json::String(format!("\\x{}", hex_encode(&b))))
                .map_err(|e| decode_err(table, col, ty, e)),
            // ── text family — TEXT/VARCHAR/BPCHAR/NAME/CHAR/CITEXT —
            // and any unrecognised type: try String. NUMERIC falls
            // here today; without sqlx's `bigdecimal` or `rust_decimal`
            // features it won't decode and snapshots on NUMERIC-bearing
            // schemas will fail loudly. Documented as a v1.1 follow-up.
            _ => row
                .try_get::<String, _>(idx)
                .map(Json::String)
                .map_err(|e| decode_err(table, col, ty, e)),
        }
    }

    let mut map = serde_json::Map::new();
    for (idx, col) in row.columns().iter().enumerate() {
        let name = col.name().to_string();
        let ty = col.type_info().name().to_uppercase();
        let value = json_for_column(row, idx, table, &name, &ty)?;
        map.insert(name, value);
    }
    Ok(Json::Object(map))
}

/// Hex-encode bytes for the BYTEA JSON representation. Matches
/// Postgres' default `bytea_output = hex` so the round-trip is
/// `\x<hex>` both ways.
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // INVARIANT: write! into a String never returns Err — String's
        // fmt::Write impl is infallible (the underlying Vec<u8> grows).
        write!(&mut s, "{b:02x}").expect("write to String cannot fail");
    }
    s
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

#[cfg(test)]
mod debug_redaction_tests {
    use super::*;

    #[test]
    fn pgconfig_debug_redacts_password() {
        let cfg = PgConfig::new(
            "postgres://agentic:super-secret-pw@localhost:54322/agentic",
            Vec::new(),
        );
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("super-secret-pw"),
            "Debug output must redact the password; got: {dbg}"
        );
        assert!(
            dbg.contains("***"),
            "Debug output must mark the redacted segment with ***; got: {dbg}"
        );
        // Sanity: other URL pieces remain visible so debugging is still useful.
        assert!(
            dbg.contains("localhost") && dbg.contains("agentic"),
            "Debug output should preserve host and db name; got: {dbg}"
        );
    }

    #[test]
    fn pgconfig_debug_handles_url_without_password() {
        let cfg = PgConfig::new("postgres://agentic@localhost:54322/agentic", Vec::new());
        let dbg = format!("{cfg:?}");
        assert!(dbg.contains("localhost"));
        assert!(!dbg.contains("***"));
    }

    #[test]
    fn pgconfig_debug_handles_malformed_url() {
        let cfg = PgConfig::new("not-a-valid-url", Vec::new());
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("not-a-valid-url"));
    }
}
