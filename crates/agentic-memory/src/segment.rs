//! Segment objects: content-addressed, immutable chunks of memory rows.
//!
//! A segment is a slice of one memory table containing up to ~64MB of rows
//! plus their embeddings. Segments are sealed once full and never modified;
//! changes to existing rows are recorded by sealing a fresh segment that
//! supersedes the old key range in the next manifest.
//!
//! Per `docs/architecture/snapshot-model.md` §1.3, segments live outside
//! the typed `Object` enum: they are addressed by raw BLAKE3 of their
//! canonical JSON and stored via `FsObjectStore::put_raw`. This keeps
//! `agentic-core` independent of memory-backend specifics.

use std::collections::BTreeMap;

use agentic_core::Hash;
use serde::{Deserialize, Serialize};

/// Default sealed-segment target size. The ADR's stated default is 64 MiB
/// of serialized row payload; we measure against the canonical-JSON byte
/// length so the threshold is deterministic across backends.
pub const DEFAULT_SEGMENT_TARGET_BYTES: usize = 64 * 1024 * 1024;

/// One vector embedding attached to a row in the segment. The `row_idx`
/// indexes into `Segment::rows`. We keep embeddings out of the row payload
/// so a backend that doesn't store vectors as columns can attach them
/// cleanly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Embedding {
    pub row_idx: u32,
    /// Vector dimensions as f32 (matches pgvector).
    pub vector: Vec<f32>,
}

/// A single content-addressed chunk of a memory table.
///
/// Field order matters: serde-json preserves insertion order, and the
/// canonical JSON byte-string is what gets BLAKE3-hashed for the segment
/// address. Don't reorder without a wire-compat plan.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Segment {
    pub table: String,
    /// Memory schema version active when this segment was sealed. Embedding
    /// the schema in each segment lets restore detect schema-drift across
    /// snapshots without consulting the parent manifest.
    pub schema_version: String,
    /// Inclusive lower bound of the primary-key range covered.
    pub pk_lo: serde_json::Value,
    /// Inclusive upper bound of the primary-key range covered.
    pub pk_hi: serde_json::Value,
    pub row_count: u64,
    /// Opaque per-row payload. Concrete schema lives in each backend.
    pub rows: Vec<serde_json::Value>,
    /// Embeddings keyed back into `rows`. Empty for tables without vectors.
    #[serde(default)]
    pub embeddings: Vec<Embedding>,
    /// Arbitrary backend-specific metadata: index tip, last LSN, etc.
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl Segment {
    pub fn hash(&self) -> Hash {
        let bytes = self.to_canonical_bytes();
        Hash::of(&bytes)
    }

    /// Canonical JSON bytes used both for hashing and for persistence.
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("Segment serialization cannot fail")
    }

    /// How many bytes this segment would occupy in canonical JSON form.
    /// Used by the streamer to decide when to seal the active head.
    pub fn canonical_size(&self) -> usize {
        self.to_canonical_bytes().len()
    }
}

impl Eq for Segment {}
impl PartialEq for Segment {
    fn eq(&self, other: &Self) -> bool {
        // Compare via canonical JSON so embedding-vector NaN doesn't poison
        // equality and so insertion order in `metadata` doesn't matter.
        self.to_canonical_bytes() == other.to_canonical_bytes()
    }
}

/// One entry in a segment manifest: a key range mapped to a segment hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentRef {
    pub table: String,
    pub pk_lo: serde_json::Value,
    pub pk_hi: serde_json::Value,
    pub segment: Hash,
    /// Row count for cardinality estimates without loading the segment.
    pub row_count: u64,
}

/// The full manifest for a memory snapshot: an ordered list of segment refs
/// covering every tracked table, plus the schema version at snapshot time.
/// Itself content-addressed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentManifest {
    pub schema_version: String,
    /// Sorted by (table, pk_lo) so identical state yields identical hashes
    /// regardless of segment-write order.
    pub entries: Vec<SegmentRef>,
}

impl SegmentManifest {
    pub fn new(schema_version: impl Into<String>) -> Self {
        Self {
            schema_version: schema_version.into(),
            entries: Vec::new(),
        }
    }

    /// Insert a ref, keeping `entries` sorted by `(table, pk_lo)`.
    pub fn push(&mut self, r: SegmentRef) {
        let key_of = |x: &SegmentRef| (x.table.clone(), pk_sort_key(&x.pk_lo));
        let target = key_of(&r);
        let pos = self
            .entries
            .iter()
            .position(|x| key_of(x) >= target)
            .unwrap_or(self.entries.len());
        self.entries.insert(pos, r);
    }

    pub fn hash(&self) -> Hash {
        Hash::of(&self.to_canonical_bytes())
    }

    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("Manifest serialization cannot fail")
    }

    /// Inverse of [`Self::to_canonical_bytes`]. Decodes the raw bytes
    /// the object store carries for a `SegmentManifest`. Centralising
    /// this here (rather than letting consumers call
    /// `serde_json::from_slice` directly) keeps the wire-format
    /// assumption inside one type — a future switch to MessagePack
    /// changes one method, not every call site.
    ///
    /// Audit §A10 / [issue #44](https://github.com/git-agentic/git.agentic/issues/44).
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// Map a JSON primary-key value to a comparable sort key. Numbers and
/// strings cover the realistic cases (bigint, uuid, text). Anything else
/// falls back to the canonical JSON string for a stable but coarse order.
fn pk_sort_key(v: &serde_json::Value) -> PkKey {
    use serde_json::Value::*;
    match v {
        Number(n) => n
            .as_i64()
            .map(PkKey::Int)
            .or_else(|| n.as_f64().map(|f| PkKey::Float(f.to_bits())))
            .unwrap_or_else(|| PkKey::Text(n.to_string())),
        String(s) => PkKey::Text(s.clone()),
        other => PkKey::Text(other.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PkKey {
    Int(i64),
    /// Raw f64 bits so NaN doesn't poison Ord. Used only for sort stability,
    /// not for actual numeric comparison.
    Float(u64),
    Text(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_seg(table: &str) -> Segment {
        Segment {
            table: table.into(),
            schema_version: "1".into(),
            pk_lo: json!(0),
            pk_hi: json!(0),
            row_count: 0,
            rows: vec![],
            embeddings: vec![],
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn identical_segments_hash_identically() {
        let a = Segment {
            table: "episodes".into(),
            schema_version: "3.1.2".into(),
            pk_lo: json!(1),
            pk_hi: json!(100),
            row_count: 2,
            rows: vec![json!({"id":1,"text":"a"}), json!({"id":2,"text":"b"})],
            embeddings: vec![Embedding {
                row_idx: 0,
                vector: vec![0.1, 0.2],
            }],
            metadata: BTreeMap::new(),
        };
        let b = a.clone();
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn metadata_insertion_order_does_not_change_hash() {
        let mut m1 = BTreeMap::new();
        m1.insert("a".to_string(), json!(1));
        m1.insert("z".to_string(), json!(9));
        let mut m2 = BTreeMap::new();
        m2.insert("z".to_string(), json!(9));
        m2.insert("a".to_string(), json!(1));

        let mut s1 = empty_seg("t");
        s1.metadata = m1;
        let mut s2 = empty_seg("t");
        s2.metadata = m2;
        assert_eq!(s1.hash(), s2.hash());
    }

    #[test]
    fn manifest_push_is_sorted() {
        let mut m = SegmentManifest::new("1");
        m.push(SegmentRef {
            table: "b".into(),
            pk_lo: json!(0),
            pk_hi: json!(10),
            segment: Hash::of(b"b0"),
            row_count: 1,
        });
        m.push(SegmentRef {
            table: "a".into(),
            pk_lo: json!(0),
            pk_hi: json!(10),
            segment: Hash::of(b"a0"),
            row_count: 1,
        });
        m.push(SegmentRef {
            table: "a".into(),
            pk_lo: json!(100),
            pk_hi: json!(200),
            segment: Hash::of(b"a1"),
            row_count: 1,
        });
        assert_eq!(m.entries[0].table, "a");
        assert_eq!(m.entries[0].pk_lo, json!(0));
        assert_eq!(m.entries[1].table, "a");
        assert_eq!(m.entries[1].pk_lo, json!(100));
        assert_eq!(m.entries[2].table, "b");
    }

    #[test]
    fn manifest_hash_is_insertion_order_independent() {
        let mut m1 = SegmentManifest::new("1");
        let mut m2 = SegmentManifest::new("1");
        let r_a = SegmentRef {
            table: "a".into(),
            pk_lo: json!(0),
            pk_hi: json!(10),
            segment: Hash::of(b"a"),
            row_count: 1,
        };
        let r_b = SegmentRef {
            table: "b".into(),
            pk_lo: json!(0),
            pk_hi: json!(10),
            segment: Hash::of(b"b"),
            row_count: 1,
        };
        m1.push(r_a.clone());
        m1.push(r_b.clone());
        m2.push(r_b);
        m2.push(r_a);
        assert_eq!(m1.hash(), m2.hash());
    }

    #[test]
    fn manifest_canonical_bytes_round_trip() {
        // Audit §A10 / #44: `from_canonical_bytes` is the inverse of
        // `to_canonical_bytes`, so the hash survives a round-trip
        // through bytes. This pins the wire-format contract that the
        // object store and the rollback loaders depend on.
        let mut m = SegmentManifest::new("003_add_embeddings");
        m.push(SegmentRef {
            table: "messages".into(),
            pk_lo: json!(0),
            pk_hi: json!(99),
            segment: Hash::of(b"seg-a"),
            row_count: 100,
        });
        m.push(SegmentRef {
            table: "messages".into(),
            pk_lo: json!(100),
            pk_hi: json!(199),
            segment: Hash::of(b"seg-b"),
            row_count: 100,
        });
        let bytes = m.to_canonical_bytes();
        let decoded = SegmentManifest::from_canonical_bytes(&bytes)
            .expect("canonical bytes must round-trip cleanly");
        assert_eq!(decoded, m);
        assert_eq!(decoded.hash(), m.hash());
    }

    #[test]
    fn manifest_from_canonical_bytes_rejects_corrupt_input() {
        // Garbage bytes must surface as an Err rather than panicking.
        let bad = b"{not-json";
        assert!(
            SegmentManifest::from_canonical_bytes(bad).is_err(),
            "corrupt bytes must return Err, not panic or succeed"
        );
        // Well-formed JSON of the wrong shape also fails: schema_version
        // is required, entries is required.
        let wrong_shape = br#"{"not_a_manifest": true}"#;
        assert!(SegmentManifest::from_canonical_bytes(wrong_shape).is_err());
    }

    #[test]
    fn canonical_size_grows_with_rows() {
        let mut s = empty_seg("t");
        let baseline = s.canonical_size();
        for i in 0..100u32 {
            s.rows.push(json!({"id": i, "payload": "x".repeat(64)}));
        }
        s.row_count = s.rows.len() as u64;
        s.pk_hi = json!(99);
        assert!(s.canonical_size() > baseline + 1000);
    }
}
