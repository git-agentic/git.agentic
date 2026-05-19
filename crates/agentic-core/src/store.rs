//! On-disk content-addressed object store.
//!
//! MVP layout (`.agentic/objects/<ab>/<remaining-62-hex>.zst`):
//!   - Objects are zstd-compressed on disk.
//!   - First two hex chars of the BLAKE3 hash form the shard directory.
//!   - Filename is the remaining 62 hex chars plus `.zst`.
//!
//! Week-1 implementation: blob put/get on a local directory. Week-2+ adds
//! tree/commit support and ref management. The `ObjectStore` trait is the
//! seam where remote (S3) backends will plug in later (v1.1).

use crate::hash::Hash;
use crate::object::{Object, ObjectKind};
use crate::{Error, Result};

use std::path::{Path, PathBuf};

/// The store contract. MVP implementer is `FsObjectStore`; future
/// implementers include S3, GCS, and a network-replicated store.
pub trait ObjectStore: Send + Sync {
    fn put(&self, object: &Object) -> Result<Hash>;
    fn put_raw(&self, kind: ObjectKind, bytes: &[u8]) -> Result<Hash>;
    fn get(&self, hash: &Hash) -> Result<Object>;
    fn has(&self, hash: &Hash) -> bool;
}

/// Local-filesystem object store. The MVP implementation.
pub struct FsObjectStore {
    root: PathBuf,
}

impl FsObjectStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path_for(&self, hash: &Hash) -> PathBuf {
        let (prefix, rest) = hash.shard();
        self.root.join(prefix).join(format!("{rest}.zst"))
    }

    fn write_at(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write to a temp file in the same directory and atomically rename,
        // so a crash mid-write never leaves a torn object in the store.
        let tmp = path.with_extension("tmp");
        let compressed = zstd::stream::encode_all(bytes, 3)?;
        std::fs::write(&tmp, &compressed)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn read_at(&self, path: &Path) -> Result<Vec<u8>> {
        let compressed = std::fs::read(path)?;
        let bytes = zstd::stream::decode_all(&compressed[..])?;
        Ok(bytes)
    }
}

impl ObjectStore for FsObjectStore {
    fn put(&self, object: &Object) -> Result<Hash> {
        let bytes = serde_json::to_vec(object)?;
        let hash = object.hash();
        let path = self.path_for(&hash);
        if !path.exists() {
            self.write_at(&path, &bytes)?;
        }
        Ok(hash)
    }

    fn put_raw(&self, _kind: ObjectKind, bytes: &[u8]) -> Result<Hash> {
        let hash = Hash::of(bytes);
        let path = self.path_for(&hash);
        if !path.exists() {
            self.write_at(&path, bytes)?;
        }
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> Result<Object> {
        let path = self.path_for(hash);
        if !path.exists() {
            return Err(Error::NotFound(*hash));
        }
        let bytes = self.read_at(&path)?;
        let object: Object = serde_json::from_slice(&bytes)?;

        // Integrity check: did we get back what the address says we should?
        let computed = object.hash();
        if &computed != hash {
            return Err(Error::IntegrityError {
                declared: *hash,
                computed,
            });
        }
        Ok(object)
    }

    fn has(&self, hash: &Hash) -> bool {
        self.path_for(hash).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{Blob, Object};

    #[test]
    fn put_and_get_roundtrip_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();

        let b = Blob::new(b"hello, agentic".to_vec());
        let obj = Object::Blob(b.clone());

        let hash = store.put(&obj).unwrap();
        assert!(store.has(&hash));

        let fetched = store.get(&hash).unwrap();
        match fetched {
            Object::Blob(got) => assert_eq!(got, b),
            _ => panic!("expected Blob"),
        }
    }

    #[test]
    fn missing_object_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let h = Hash::of(b"never-stored");
        match store.get(&h) {
            Err(Error::NotFound(h2)) => assert_eq!(h, h2),
            _ => panic!("expected NotFound"),
        }
    }
}
