//! Streamer — accumulates change events into sealed segments.
//!
//! ## Design
//!
//! One async task owns all streamer state. Producers (the trigger poller,
//! or a future logical-decoding decoder) push `ChangeEvent`s into a tokio
//! mpsc channel. The task drains the channel, applies each event to the
//! per-table active head segment, and seals the head when it crosses the
//! configured byte threshold. Sealed segments are persisted via the
//! shared `ObjectStore` and their `SegmentRef`s appended to a per-table
//! sealed list.
//!
//! Snapshots go through `StreamerHandle::take_snapshot`: a single RPC
//! through the channel that asks the task to seal every non-empty active
//! head, then returns a `SegmentManifest` whose entries are the union of
//! all sealed `SegmentRef`s. This is the atomic-snapshot primitive that
//! replaces `bootstrap`-as-snapshot once the trigger poller is running.
//!
//! ## Semantics
//!
//! For MVP we treat the active head as an append-only log of row
//! observations. Two events with the same primary key produce two rows
//! in the head; restore via `INSERT ... ON CONFLICT DO UPDATE` collapses
//! them so the latest write wins. Compaction (rewriting older segments
//! to drop superseded rows) lands in a follow-up.

use std::collections::BTreeMap;
use std::sync::Arc;

use agentic_core::{ObjectKind, ObjectStore};
use serde_json::Value as Json;
use tokio::sync::{mpsc, oneshot};

use crate::segment::{Segment, SegmentManifest, SegmentRef};
use crate::{Error, Result};

/// Default channel depth for change-event delivery. Producers see
/// backpressure when the streamer falls behind; in MVP we just block —
/// the trigger poller calls `send().await`.
pub const DEFAULT_CHANNEL_DEPTH: usize = 1024;

/// One row mutation observed on a tracked table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeEvent {
    pub table: String,
    /// JSON object representing the post-mutation row. For `Delete` the
    /// object should contain at least the primary key column so restore
    /// knows what to drop.
    pub row: Json,
    pub op: Op,
    /// Schema version active at the moment the change was captured.
    pub schema_version: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Insert,
    Update,
    Delete,
}

/// Per-table in-memory streamer state.
struct TableHead {
    table: String,
    /// Active (unsealed) head segment.
    active: Segment,
    /// Sealed segment refs persisted in the object store, in seal order.
    sealed: Vec<SegmentRef>,
    /// Bytes threshold above which the active head gets sealed.
    seal_threshold_bytes: usize,
}

impl TableHead {
    fn new(table: String, schema_version: String, seal_threshold_bytes: usize) -> Self {
        Self {
            table: table.clone(),
            active: blank_segment(&table, &schema_version),
            sealed: Vec::new(),
            seal_threshold_bytes,
        }
    }

    fn apply(&mut self, ev: ChangeEvent, pk_col: &str) -> Result<()> {
        if self.active.schema_version != ev.schema_version && self.active.rows.is_empty() {
            self.active.schema_version = ev.schema_version.clone();
        }
        // PK must be present and non-NULL. Without it the segment's
        // pk_lo/pk_hi anchor would degrade to Json::Null and corrupt
        // the manifest's range metadata — same failure mode that
        // `bootstrap_table` now rejects on the snapshot path. Surface
        // here as `Err`; the streamer loop logs + drops the event so
        // the channel keeps draining instead of crashing the task.
        let pk = match ev.row.get(pk_col) {
            Some(v) if !v.is_null() => v.clone(),
            Some(_) => {
                return Err(Error::Backend(format!(
                    "streamer event for table {:?}: PK column {:?} is NULL; \
                     refusing to anchor segment on a null PK",
                    self.table, pk_col
                )))
            }
            None => {
                return Err(Error::Backend(format!(
                    "streamer event for table {:?}: PK column {:?} is absent from event row; \
                     check TrackedTable.pk vs the trigger payload",
                    self.table, pk_col
                )))
            }
        };
        if self.active.rows.is_empty() {
            self.active.pk_lo = pk.clone();
        }
        self.active.pk_hi = pk;

        // Envelope encodes the op so restore knows whether to upsert or
        // drop. Wrapping the user row avoids colliding with their column
        // names.
        let envelope = serde_json::json!({
            "op": match ev.op {
                Op::Insert => "insert",
                Op::Update => "update",
                Op::Delete => "delete",
            },
            "row": ev.row,
        });
        self.active.rows.push(envelope);
        self.active.row_count = self.active.rows.len() as u64;
        Ok(())
    }

    fn seal_if_nonempty<S: ObjectStore + ?Sized>(
        &mut self,
        store: &S,
    ) -> Result<Option<SegmentRef>> {
        if self.active.rows.is_empty() {
            return Ok(None);
        }
        let bytes = self.active.to_canonical_bytes();
        let hash = store
            .put_raw(ObjectKind::Segment, &bytes)
            .map_err(|e| Error::Backend(format!("persisting segment: {e}")))?;
        let r = SegmentRef {
            table: self.active.table.clone(),
            pk_lo: self.active.pk_lo.clone(),
            pk_hi: self.active.pk_hi.clone(),
            segment: hash,
            row_count: self.active.row_count,
        };
        self.sealed.push(r.clone());
        let sv = self.active.schema_version.clone();
        self.active = blank_segment(&self.table, &sv);
        Ok(Some(r))
    }

    fn maybe_seal_on_threshold<S: ObjectStore + ?Sized>(&mut self, store: &S) -> Result<()> {
        if self.active.canonical_size() >= self.seal_threshold_bytes {
            self.seal_if_nonempty(store)?;
        }
        Ok(())
    }
}

/// Messages the streamer task accepts.
enum Cmd {
    Event(ChangeEvent),
    /// "Seed me with these sealed segments as the starting baseline."
    /// Sent once at adapter-init time so bootstrap segments anchor the
    /// streamer's view of the world.
    SeedSealed(Vec<SegmentRef>),
    /// "Seal every non-empty head and return a manifest."
    TakeSnapshot {
        schema_version: String,
        reply: oneshot::Sender<Result<SegmentManifest>>,
    },
}

/// Cheap clonable handle for producers and the snapshot path.
#[derive(Clone)]
pub struct StreamerHandle {
    tx: mpsc::Sender<Cmd>,
}

impl StreamerHandle {
    pub async fn send_event(&self, ev: ChangeEvent) -> Result<()> {
        self.tx
            .send(Cmd::Event(ev))
            .await
            .map_err(|_| Error::StreamerShutdown)
    }

    pub async fn seed_sealed(&self, refs: Vec<SegmentRef>) -> Result<()> {
        self.tx
            .send(Cmd::SeedSealed(refs))
            .await
            .map_err(|_| Error::StreamerShutdown)
    }

    pub async fn take_snapshot(&self, schema_version: &str) -> Result<SegmentManifest> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::TakeSnapshot {
                schema_version: schema_version.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| Error::StreamerShutdown)?;
        rx.await.map_err(|_| Error::StreamerShutdown)?
    }
}

/// Spawn the streamer task. Returns a handle and a `JoinHandle` for the
/// task — callers keep both alive for the daemon's lifetime.
pub fn spawn<S>(
    store: Arc<S>,
    tables: Vec<crate::postgres::TrackedTable>,
    seal_threshold_bytes: usize,
    initial_schema_version: String,
) -> (StreamerHandle, tokio::task::JoinHandle<()>)
where
    S: ObjectStore + Send + Sync + 'static + ?Sized,
{
    let (tx, mut rx) = mpsc::channel::<Cmd>(DEFAULT_CHANNEL_DEPTH);

    let mut heads: BTreeMap<String, TableHead> = tables
        .iter()
        .map(|t| {
            (
                t.name.clone(),
                TableHead::new(
                    t.name.clone(),
                    initial_schema_version.clone(),
                    seal_threshold_bytes,
                ),
            )
        })
        .collect();
    let pk_for: BTreeMap<String, String> = tables.into_iter().map(|t| (t.name, t.pk)).collect();

    let join = tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Cmd::Event(ev) => {
                    let Some(head) = heads.get_mut(&ev.table) else {
                        tracing::warn!(
                            table = %ev.table,
                            "ignoring event for untracked table"
                        );
                        continue;
                    };
                    // PK lookup must succeed — `pk_for` is built from
                    // the same `tables` set as `heads`, so missing
                    // would be a bookkeeping bug not a data issue.
                    // Drop the event with a loud log if it ever fires;
                    // empty-string PK would silently anchor segments.
                    let Some(pk) = pk_for.get(&ev.table).cloned() else {
                        tracing::error!(
                            table = %ev.table,
                            "no PK column registered for tracked table; \
                             dropping event to avoid null-anchored segment"
                        );
                        continue;
                    };
                    if let Err(e) = head.apply(ev, &pk) {
                        // Surfaces null/absent PK in the event row, etc.
                        // Log loudly but keep the streamer alive — one
                        // bad event shouldn't crash the whole task.
                        tracing::error!(error = %format!("{e:#}"), "dropping streamer event");
                        continue;
                    }
                    if let Err(e) = head.maybe_seal_on_threshold(store.as_ref()) {
                        tracing::error!(error = %e, "sealing active head");
                    }
                }
                Cmd::SeedSealed(refs) => {
                    for r in refs {
                        if let Some(head) = heads.get_mut(&r.table) {
                            head.sealed.push(r);
                        }
                    }
                }
                Cmd::TakeSnapshot {
                    schema_version,
                    reply,
                } => {
                    let result = (|| {
                        for head in heads.values_mut() {
                            head.seal_if_nonempty(store.as_ref())?;
                        }
                        let mut m = SegmentManifest::new(schema_version);
                        for head in heads.values() {
                            for r in &head.sealed {
                                m.push(r.clone());
                            }
                        }
                        Ok::<_, Error>(m)
                    })();
                    let _ = reply.send(result);
                }
            }
        }
    });

    (StreamerHandle { tx }, join)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgres::TrackedTable;
    use agentic_core::FsObjectStore;
    use serde_json::json;

    fn one_table() -> Vec<TrackedTable> {
        vec![TrackedTable {
            name: "episodes".into(),
            pk: "id".into(),
        }]
    }

    #[tokio::test]
    async fn snapshot_is_empty_when_no_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FsObjectStore::open(dir.path()).unwrap());
        let (h, _join) = spawn(store, one_table(), 1024, "1".into());
        let m = h.take_snapshot("1").await.unwrap();
        assert!(m.entries.is_empty());
        assert_eq!(m.schema_version, "1");
    }

    #[tokio::test]
    async fn events_accumulate_and_seal_on_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FsObjectStore::open(dir.path()).unwrap());
        let (h, _join) = spawn(store, one_table(), 1024 * 1024, "1".into());
        for i in 1..=5i64 {
            h.send_event(ChangeEvent {
                table: "episodes".into(),
                row: json!({"id": i, "text": format!("row-{i}")}),
                op: Op::Insert,
                schema_version: "1".into(),
            })
            .await
            .unwrap();
        }
        let m = h.take_snapshot("1").await.unwrap();
        assert_eq!(m.entries.len(), 1, "one sealed segment for the table");
        assert_eq!(m.entries[0].row_count, 5);
        assert_eq!(m.entries[0].pk_lo, json!(1));
        assert_eq!(m.entries[0].pk_hi, json!(5));
    }

    #[tokio::test]
    async fn small_threshold_seals_multiple_segments() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FsObjectStore::open(dir.path()).unwrap());
        let (h, _join) = spawn(store, one_table(), 256, "1".into());
        for i in 1..=8i64 {
            h.send_event(ChangeEvent {
                table: "episodes".into(),
                row: json!({"id": i, "text": "x".repeat(64)}),
                op: Op::Insert,
                schema_version: "1".into(),
            })
            .await
            .unwrap();
        }
        let m = h.take_snapshot("1").await.unwrap();
        assert!(m.entries.len() >= 2, "expected multiple sealed segments");
        let total_rows: u64 = m.entries.iter().map(|r| r.row_count).sum();
        assert_eq!(total_rows, 8);
    }

    #[tokio::test]
    async fn seed_sealed_anchors_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FsObjectStore::open(dir.path()).unwrap());
        let (h, _join) = spawn(store, one_table(), 1024 * 1024, "1".into());
        let baseline = SegmentRef {
            table: "episodes".into(),
            pk_lo: json!(0),
            pk_hi: json!(99),
            segment: agentic_core::Hash::of(b"baseline"),
            row_count: 100,
        };
        h.seed_sealed(vec![baseline.clone()]).await.unwrap();
        let m = h.take_snapshot("1").await.unwrap();
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].segment, baseline.segment);
    }
}
