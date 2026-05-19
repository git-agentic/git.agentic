//! Segment objects: content-addressed, immutable chunks of memory rows.
//!
//! A segment is a slice of one memory table containing up to ~64MB of rows
//! plus their embeddings. Segments are sealed once full and never modified;
//! changes to existing rows are recorded by sealing a fresh segment that
//! supersedes the old key range in the next manifest.

use agentic_core::Hash;
use serde::{Deserialize, Serialize};

/// A single content-addressed chunk of a memory table.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Segment {
    pub table: String,
    /// Inclusive lower bound of the primary-key range.
    pub pk_lo: serde_json::Value,
    /// Inclusive upper bound of the primary-key range.
    pub pk_hi: serde_json::Value,
    pub row_count: u64,
    /// Opaque per-row payload. Concrete schema lives in each backend.
    pub rows: Vec<serde_json::Value>,
}

impl Segment {
    pub fn hash(&self) -> Hash {
        let bytes = serde_json::to_vec(self).expect("Segment serialization cannot fail");
        Hash::of(&bytes)
    }
}

/// One entry in a segment manifest: a key range mapped to a segment hash.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SegmentRef {
    pub table: String,
    pub pk_lo: serde_json::Value,
    pub pk_hi: serde_json::Value,
    pub segment: Hash,
}

/// The full manifest for a memory snapshot: an ordered list of segment refs
/// covering every tracked table. Itself content-addressed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SegmentManifest {
    pub entries: Vec<SegmentRef>,
}

impl SegmentManifest {
    pub fn hash(&self) -> Hash {
        let bytes = serde_json::to_vec(self).expect("Manifest serialization cannot fail");
        Hash::of(&bytes)
    }
}
