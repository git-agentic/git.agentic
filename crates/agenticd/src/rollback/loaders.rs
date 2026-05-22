//! Typed object-store readers shared across rollback orchestration and
//! filesystem write-back. Each loader fetches one well-known object kind
//! (Commit, Tree, Blob) or a serialized segment manifest. They're pure
//! `ObjectStore` consumers — no rollback or write-back logic lives here.
//!
//! Audit §S3 / §A4: the previous `rollback.rs` mixed phase orchestration,
//! object loaders, and filesystem write-back in one file. This module
//! owns the loader half of that split.

use agentic_core::{Blob, Commit, Hash, Object, Tree};
use agentic_memory::segment::SegmentManifest;
use anyhow::{anyhow, Context};

use crate::server::DaemonState;

pub(super) fn load_commit(state: &DaemonState, hash: &Hash) -> anyhow::Result<Commit> {
    match state.store.get(hash)? {
        Object::Commit(c) => Ok(*c),
        other => Err(anyhow!(
            "expected commit at {}, got {:?}",
            hash,
            other.kind()
        )),
    }
}

pub(super) fn load_tree(state: &DaemonState, hash: &Hash) -> anyhow::Result<Tree> {
    match state.store.get(hash)? {
        Object::Tree(t) => Ok(t),
        other => Err(anyhow!("expected tree at {}, got {:?}", hash, other.kind())),
    }
}

pub(super) fn load_blob(state: &DaemonState, hash: &Hash) -> anyhow::Result<Blob> {
    match state.store.get(hash)? {
        Object::Blob(b) => Ok(b),
        other => Err(anyhow!("expected blob at {}, got {:?}", hash, other.kind())),
    }
}

pub(super) fn load_manifest(state: &DaemonState, hash: &Hash) -> anyhow::Result<SegmentManifest> {
    let bytes = state.store.get_raw(hash)?;
    SegmentManifest::from_canonical_bytes(&bytes)
        .with_context(|| format!("decoding manifest {hash}"))
}
