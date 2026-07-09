//! In-memory test fixture implementing [`MemoryAdapter`].
//!
//! This is the second backend the [§A9](../../../../docs/ops/2026-05-21-agenticd-architectural-analysis.md#a9)
//! follow-up calls for — not a production backend, just a Rule-of-Three
//! check that the trait surface compiles and rounds-trips for something
//! other than Postgres. The implementation stores per-table rows in
//! `HashMap<String, Vec<serde_json::Value>>` and serialises snapshots
//! through the canonical `SegmentManifest` / `Segment` shapes the
//! object store carries.
//!
//! The fixture models schema migrations just enough for the daemon's
//! reverse-migration path: `apply_migration` records a forward
//! migration (the live schema version is the newest applied name, or
//! `"0.0.0"`), `migrations_after` walks that bookkeeping, and
//! `apply_reverse_migrations` pops entries all-or-nothing, ignoring
//! each step's `sql`.
//!
//! What this fixture **does not** model:
//!
//! - Concurrent writes. The internal `Mutex` serialises every call —
//!   fine for tests, would be a contention point in a real backend.
//! - Embeddings. Snapshots only round-trip `rows`; the `embeddings`
//!   field on `Segment` is dropped on restore.
//!
//! When a real second backend (Mem0 / Zep / Letta) lands, this fixture
//! stays — it's the lightest possible conformance check for new trait
//! methods, no Postgres or Docker required.

use std::collections::HashMap;
use std::sync::Arc;

use agentic_core::{ObjectKind, ObjectStore};
use serde_json::Value as Json;
use tokio::sync::Mutex;

use crate::adapter::{MemoryAdapter, MigrationStep, RestoreGuard, SnapshotHandle};
use crate::segment::{Segment, SegmentManifest, SegmentRef};
use crate::{Error, Result};

/// Per-table row store. The `Vec<Json>` is the table's rows in
/// insertion order; the fixture doesn't model primary keys or
/// uniqueness — callers seed whatever shape their test wants and read
/// it back after restore.
#[derive(Default, Debug, Clone)]
pub struct InMemoryTable {
    pub rows: Vec<Json>,
}

/// In-memory [`MemoryAdapter`] fixture. Holds an `Arc<dyn ObjectStore>`
/// for snapshot byte storage, just like the Postgres adapter, so the
/// `SegmentManifest` round-trip exercises the same content-addressed
/// path that production uses.
pub struct InMemoryAdapter {
    store: Arc<dyn ObjectStore + Send + Sync>,
    state: Mutex<InMemoryState>,
}

#[derive(Default)]
struct InMemoryState {
    schema_version: String,
    tables: HashMap<String, InMemoryTable>,
    /// Applied forward migrations, oldest → newest. The live
    /// `schema_version` is the last entry (or `"0.0.0"` when empty),
    /// mirroring the Postgres convention where the schema version is
    /// the most recent `agentic_migrations` stem.
    applied_migrations: Vec<String>,
}

impl InMemoryAdapter {
    pub fn new(store: Arc<dyn ObjectStore + Send + Sync>) -> Self {
        Self {
            store,
            state: Mutex::new(InMemoryState {
                schema_version: "0.0.0".to_string(),
                tables: HashMap::new(),
                applied_migrations: Vec::new(),
            }),
        }
    }

    /// Test helper: set the schema version directly. Production
    /// adapters would derive this from the live DB schema.
    pub async fn set_schema_version(&self, version: impl Into<String>) {
        self.state.lock().await.schema_version = version.into();
    }

    /// Test helper: record a forward migration as applied and set the
    /// live schema version to its name (the Postgres convention).
    pub async fn apply_migration(&self, name: impl Into<String>) {
        let mut state = self.state.lock().await;
        let name = name.into();
        state.applied_migrations.push(name.clone());
        state.schema_version = name;
    }

    /// Test helper: append rows to a table. Used by the trait
    /// conformance test to populate state before `snapshot`.
    pub async fn insert_rows(&self, table: impl Into<String>, rows: Vec<Json>) {
        let mut state = self.state.lock().await;
        state
            .tables
            .entry(table.into())
            .or_default()
            .rows
            .extend(rows);
    }

    /// Test helper: read a table's rows. Used after `restore` to
    /// assert the round-trip matches.
    pub async fn rows_of(&self, table: &str) -> Vec<Json> {
        self.state
            .lock()
            .await
            .tables
            .get(table)
            .map(|t| t.rows.clone())
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl MemoryAdapter for InMemoryAdapter {
    async fn init(&mut self) -> Result<()> {
        Ok(())
    }

    async fn snapshot(&self) -> Result<SnapshotHandle> {
        let state = self.state.lock().await;
        let mut manifest = SegmentManifest::new(state.schema_version.clone());
        // Deterministic order so identical state yields identical manifest hashes.
        let mut tables: Vec<(&String, &InMemoryTable)> = state.tables.iter().collect();
        tables.sort_by(|a, b| a.0.cmp(b.0));
        for (table_name, table) in tables {
            if table.rows.is_empty() {
                continue;
            }
            let segment = Segment {
                table: table_name.clone(),
                schema_version: state.schema_version.clone(),
                pk_lo: Json::Null,
                pk_hi: Json::Null,
                row_count: table.rows.len() as u64,
                rows: table.rows.clone(),
                embeddings: Vec::new(),
                metadata: Default::default(),
            };
            let bytes = serde_json::to_vec(&segment)
                .map_err(|e| Error::Backend(format!("serialising segment: {e}")))?;
            let segment_hash = self
                .store
                .put_raw(ObjectKind::Segment, &bytes)
                .map_err(Error::Core)?;
            manifest.push(SegmentRef {
                table: table_name.clone(),
                pk_lo: Json::Null,
                pk_hi: Json::Null,
                segment: segment_hash,
                row_count: table.rows.len() as u64,
            });
        }
        Ok(SnapshotHandle {
            manifest,
            schema_version: state.schema_version.clone(),
        })
    }

    async fn restore(&self, target: &SnapshotHandle) -> Result<()> {
        let guard = self.begin_restore().await?;
        self.restore_with_guard(&guard, target).await
    }

    async fn current_schema_version(&self) -> Result<String> {
        Ok(self.state.lock().await.schema_version.clone())
    }

    async fn migrations_after(&self, target_name: &str) -> Result<Vec<String>> {
        let state = self.state.lock().await;
        if target_name == "0.0.0" {
            return Ok(state.applied_migrations.iter().rev().cloned().collect());
        }
        if let Some(pos) = state
            .applied_migrations
            .iter()
            .position(|n| n == target_name)
        {
            return Ok(state.applied_migrations[pos + 1..]
                .iter()
                .rev()
                .cloned()
                .collect());
        }
        if target_name == state.schema_version {
            return Ok(Vec::new());
        }
        Err(Error::Other(anyhow::anyhow!(
            "InMemoryAdapter::migrations_after: target schema_version {target_name:?} \
             is not the baseline ('0.0.0'), not a recorded migration, and not the live \
             schema ({:?}); reversing an unknown target is unsafe",
            state.schema_version
        )))
    }

    async fn begin_restore(&self) -> Result<RestoreGuard> {
        // Nothing to quiesce — no trigger poller, no write streamer.
        Ok(RestoreGuard::noop())
    }

    async fn restore_with_guard(
        &self,
        _guard: &RestoreGuard,
        target: &SnapshotHandle,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.schema_version != target.schema_version {
            return Err(Error::SchemaMismatch {
                live: state.schema_version.clone(),
                target: target.schema_version.clone(),
            });
        }
        // Replay manifest: clear every tracked table that appears in
        // the manifest, then write its rows back from the segment bytes.
        let mut new_tables: HashMap<String, InMemoryTable> = HashMap::new();
        for entry in &target.manifest.entries {
            let bytes = self.store.get_raw(&entry.segment).map_err(Error::Core)?;
            let segment: Segment = serde_json::from_slice(&bytes)
                .map_err(|e| Error::Backend(format!("decoding segment: {e}")))?;
            new_tables
                .entry(entry.table.clone())
                .or_default()
                .rows
                .extend(segment.rows);
        }
        state.tables = new_tables;
        Ok(())
    }

    /// The fixture reverses by name alone — `step.sql` is ignored
    /// (there is no SQL engine here). All-or-nothing is achieved by
    /// validating every step against the applied list before mutating
    /// anything.
    async fn apply_reverse_migrations(&self, steps: &[MigrationStep]) -> Result<()> {
        if steps.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        // Validate first: steps must peel the applied list newest-first.
        {
            let mut expected = state.applied_migrations.iter().rev();
            for step in steps {
                match expected.next() {
                    Some(applied) if *applied == step.name => {}
                    Some(applied) => {
                        return Err(Error::Other(anyhow::anyhow!(
                            "InMemoryAdapter::apply_reverse_migrations: step {:?} does not \
                             match the most recent unreversed migration {:?}; state unchanged",
                            step.name,
                            applied
                        )));
                    }
                    None => {
                        return Err(Error::Other(anyhow::anyhow!(
                            "InMemoryAdapter::apply_reverse_migrations: step {:?} has no \
                             corresponding applied migration; state unchanged",
                            step.name
                        )));
                    }
                }
            }
        }
        // Commit: pop the reversed migrations, re-derive the live version.
        let keep = state.applied_migrations.len() - steps.len();
        state.applied_migrations.truncate(keep);
        state.schema_version = state
            .applied_migrations
            .last()
            .cloned()
            .unwrap_or_else(|| "0.0.0".to_string());
        Ok(())
    }
}

// Sanity: the type is dyn-compatible, so `Arc<dyn MemoryAdapter>`
// works for the daemon-state retype tracked separately in §A9.
const _: fn() = || {
    fn assert_dyn<T: MemoryAdapter + ?Sized>() {}
    assert_dyn::<dyn MemoryAdapter>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use agentic_core::FsObjectStore;

    fn fixture() -> InMemoryAdapter {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore + Send + Sync> =
            Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());
        // Leak the tempdir so the store path stays valid for the test
        // (the fixture doesn't take ownership of it).
        let _ = Box::leak(Box::new(dir));
        InMemoryAdapter::new(store)
    }

    #[tokio::test]
    async fn snapshot_then_restore_round_trips_table_rows() {
        let adapter = fixture();
        adapter.set_schema_version("v1").await;
        adapter
            .insert_rows(
                "messages",
                vec![
                    serde_json::json!({"id": 1, "body": "alpha"}),
                    serde_json::json!({"id": 2, "body": "bravo"}),
                ],
            )
            .await;

        // Capture a snapshot.
        let handle = adapter.snapshot().await.unwrap();
        assert_eq!(handle.schema_version, "v1");
        assert_eq!(handle.manifest.entries.len(), 1);

        // Mutate live state (simulates the contamination the demo
        // creates between commit and rollback).
        adapter
            .insert_rows(
                "messages",
                vec![serde_json::json!({"id": 99, "body": "contaminated"})],
            )
            .await;
        assert_eq!(adapter.rows_of("messages").await.len(), 3);

        // Restore wipes the contamination.
        adapter.restore(&handle).await.unwrap();
        let restored = adapter.rows_of("messages").await;
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0]["body"], "alpha");
        assert_eq!(restored[1]["body"], "bravo");
    }

    #[tokio::test]
    async fn restore_rejects_schema_mismatch() {
        let adapter = fixture();
        adapter.set_schema_version("v1").await;
        let handle = adapter.snapshot().await.unwrap();
        // Advance the live schema past the snapshot's version.
        adapter.set_schema_version("v2").await;

        let err = adapter.restore(&handle).await.unwrap_err();
        match err {
            Error::SchemaMismatch { live, target } => {
                assert_eq!(live, "v2");
                assert_eq!(target, "v1");
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn migrations_after_baseline_returns_empty() {
        // The "0.0.0" baseline always returns empty — the fixture
        // hasn't applied anything past it.
        let adapter = fixture();
        adapter.set_schema_version("v3").await;
        assert!(adapter.migrations_after("0.0.0").await.unwrap().is_empty());
        // The live version also returns empty (nothing applied after it).
        assert!(adapter.migrations_after("v3").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn migrations_after_errors_on_unknown_target() {
        // Trait contract: unknown non-baseline targets must error so
        // migration-path code doesn't silently get an empty plan and
        // skip every reverse step. The fixture's bookkeeping is
        // intentionally narrow, but the error path is real.
        let adapter = fixture();
        adapter.set_schema_version("v3").await;
        let err = adapter
            .migrations_after("v1_does_not_exist")
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not the baseline") && msg.contains("not the live schema"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn begin_restore_returns_noop_guard() {
        let adapter = fixture();
        // The fixture has nothing to quiesce; just confirm the call
        // doesn't error and the guard can be dropped freely.
        let guard = adapter.begin_restore().await.unwrap();
        drop(guard);
    }

    #[tokio::test]
    async fn restore_guard_drops_inner_payload_on_drop() {
        // Pins the RAII chain that makes the quiesce window work:
        // dropping the guard drops the payload, which fires the
        // payload's Drop. Without this property, Postgres's poller
        // would never resume and TRUNCATE+INSERT during restore
        // would happen against a paused-but-then-leaked token.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct Sentinel {
            counter: Arc<AtomicUsize>,
        }
        impl Drop for Sentinel {
            fn drop(&mut self) {
                self.counter.fetch_add(1, Ordering::SeqCst);
            }
        }
        let counter = Arc::new(AtomicUsize::new(0));
        let guard = RestoreGuard::new(Sentinel {
            counter: Arc::clone(&counter),
        });
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        drop(guard);
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "guard's Drop must have run the payload's Drop"
        );
    }

    #[tokio::test]
    async fn snapshot_restore_round_trips_multiple_tables() {
        // The single-table tests above don't exercise the sort-before-
        // hash path or multi-segment manifest assembly. Two tables in
        // non-alphabetical insertion order, restored, all rows
        // accounted for.
        let adapter = fixture();
        adapter.set_schema_version("v1").await;
        adapter
            .insert_rows(
                "zeta",
                vec![serde_json::json!({"id": 1}), serde_json::json!({"id": 2})],
            )
            .await;
        adapter
            .insert_rows("alpha", vec![serde_json::json!({"id": 100})])
            .await;

        let handle = adapter.snapshot().await.unwrap();
        // Manifest entries are sorted by (table, pk_lo) regardless of
        // insertion order — "alpha" before "zeta".
        assert_eq!(handle.manifest.entries.len(), 2);
        assert_eq!(handle.manifest.entries[0].table, "alpha");
        assert_eq!(handle.manifest.entries[1].table, "zeta");

        // Contaminate then restore.
        adapter
            .insert_rows("zeta", vec![serde_json::json!({"id": 999})])
            .await;
        adapter
            .insert_rows("alpha", vec![serde_json::json!({"id": 999})])
            .await;
        adapter.restore(&handle).await.unwrap();

        assert_eq!(adapter.rows_of("zeta").await.len(), 2);
        assert_eq!(adapter.rows_of("alpha").await.len(), 1);
        assert_eq!(adapter.rows_of("alpha").await[0]["id"], 100);
    }

    #[tokio::test]
    async fn callable_through_arc_dyn_trait() {
        // Pin the dyn-trait contract: the daemon's planned
        // `Arc<dyn MemoryAdapter>` retype works because every trait
        // method takes `&self` and the adapter is `Send + Sync`.
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn ObjectStore + Send + Sync> =
            Arc::new(FsObjectStore::open(dir.path().join("objects")).unwrap());
        let adapter: Arc<dyn MemoryAdapter> = Arc::new(InMemoryAdapter::new(store));
        let _ = adapter.current_schema_version().await.unwrap();
        let _ = adapter.migrations_after("0.0.0").await.unwrap();
        let _ = adapter.begin_restore().await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_hash_is_stable_for_identical_state() {
        let a = fixture();
        let b = fixture();
        a.set_schema_version("v1").await;
        b.set_schema_version("v1").await;
        let rows = vec![serde_json::json!({"id": 1}), serde_json::json!({"id": 2})];
        a.insert_rows("t", rows.clone()).await;
        b.insert_rows("t", rows).await;

        let ha = a.snapshot().await.unwrap();
        let hb = b.snapshot().await.unwrap();
        // Manifest hashes match — same rows, same schema, same
        // canonical bytes. This is the property the broken-prompt
        // demo's diff machinery depends on.
        assert_eq!(ha.manifest.hash(), hb.manifest.hash());
    }

    #[tokio::test]
    async fn reverse_migrations_pop_applied_and_update_schema() {
        let adapter = fixture();
        adapter.apply_migration("001_init").await;
        adapter.apply_migration("002_add_embeddings").await;
        adapter.apply_migration("003_widen_body").await;
        assert_eq!(
            adapter.current_schema_version().await.unwrap(),
            "003_widen_body"
        );

        // migrations_after returns newest-first — the order they reverse.
        let names = adapter.migrations_after("001_init").await.unwrap();
        assert_eq!(names, vec!["003_widen_body", "002_add_embeddings"]);

        let steps: Vec<MigrationStep> = names
            .iter()
            .map(|n| MigrationStep {
                name: n.clone(),
                sql: String::new(), // the fixture ignores SQL
            })
            .collect();
        adapter.apply_reverse_migrations(&steps).await.unwrap();
        assert_eq!(adapter.current_schema_version().await.unwrap(), "001_init");
        assert!(adapter
            .migrations_after("001_init")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn reverse_migrations_all_or_nothing_on_bad_step() {
        let adapter = fixture();
        adapter.apply_migration("001_init").await;
        adapter.apply_migration("002_x").await;
        // First step valid, second bogus — NOTHING may change.
        let steps = vec![
            MigrationStep {
                name: "002_x".into(),
                sql: String::new(),
            },
            MigrationStep {
                name: "999_nope".into(),
                sql: String::new(),
            },
        ];
        let err = adapter.apply_reverse_migrations(&steps).await.unwrap_err();
        assert!(format!("{err:#}").contains("999_nope"));
        assert_eq!(adapter.current_schema_version().await.unwrap(), "002_x");
        assert_eq!(
            adapter.migrations_after("0.0.0").await.unwrap(),
            vec!["002_x", "001_init"]
        );
    }

    #[tokio::test]
    async fn reverse_migrations_to_baseline_and_empty_noop() {
        let adapter = fixture();
        // Empty steps: no-op even with nothing applied.
        adapter.apply_reverse_migrations(&[]).await.unwrap();
        adapter.apply_migration("001_init").await;
        let steps = vec![MigrationStep {
            name: "001_init".into(),
            sql: String::new(),
        }];
        adapter.apply_reverse_migrations(&steps).await.unwrap();
        assert_eq!(adapter.current_schema_version().await.unwrap(), "0.0.0");
    }
}
