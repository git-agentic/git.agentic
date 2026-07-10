//! On-disk content-addressed object store.
//!
//! MVP layout (`.agentic/objects/<ab>/<remaining-62-hex>.zst`):
//!   - Objects are zstd-compressed on disk.
//!   - First two hex chars of the BLAKE3 hash form the shard directory.
//!   - Filename is the remaining 62 hex chars plus `.zst`.
//!
//! `FsObjectStore` is the local-filesystem implementation; `GcsObjectStore`
//! (see `gcs.rs`) is the remote backend introduced for the sidecar
//! `agenticd` topology (ADR-0004). The `ObjectStore` trait is the seam
//! where additional backends (e.g. S3, network-replicated) will plug in.

use crate::hash::Hash;
use crate::object::{Object, ObjectKind};
use crate::scanner::{Allowlist, Scanner};
use crate::{Error, Result};

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Fail if `computed` doesn't match the content address `declared`.
///
/// The single chokepoint for the content-addressed integrity guarantee.
/// Every read path — `get` (typed, `computed = object.hash()`) and
/// `get_raw` (raw, `computed = Hash::of(bytes)`) across both store
/// backends — routes its check through here so a corrupted-at-rest object
/// can never be returned or parsed. Prior to the 2026-07-09 audit
/// (finding #3) `get_raw` skipped this entirely, so restore and manifest
/// reads trusted bytes a bad disk, poisoned cache, or bucket writer could
/// have altered while keeping the object key — silently falsifying the
/// "atomic rollback is honest" guarantee.
pub(crate) fn check_integrity(declared: &Hash, computed: Hash) -> Result<()> {
    if &computed != declared {
        return Err(Error::IntegrityError {
            declared: *declared,
            computed,
        });
    }
    Ok(())
}

/// The store contract. MVP implementer is `FsObjectStore`; future
/// implementers include S3, GCS, and a network-replicated store.
pub trait ObjectStore: Send + Sync {
    fn put(&self, object: &Object) -> Result<Hash>;
    fn put_raw(&self, kind: ObjectKind, bytes: &[u8]) -> Result<Hash>;
    fn get(&self, hash: &Hash) -> Result<Object>;
    /// Read the raw uncompressed bytes that were originally `put_raw`'d.
    /// Used for object kinds (Segment, SegmentManifest) that live outside
    /// the typed `Object` enum.
    fn get_raw(&self, hash: &Hash) -> Result<Vec<u8>>;
    fn has(&self, hash: &Hash) -> bool;
}

/// Local-filesystem object store. The MVP implementation.
pub struct FsObjectStore {
    root: PathBuf,
    scanner: Arc<Scanner>,
    allowlist: Arc<Allowlist>,
}

impl FsObjectStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            scanner: Arc::new(Scanner::new()),
            allowlist: Arc::new(Allowlist::empty()),
        })
    }

    /// Replace the allowlist on this store. Builder-style so callers can
    /// chain `FsObjectStore::open(root)?.with_allowlist(al)`.
    pub fn with_allowlist(mut self, allowlist: Allowlist) -> Self {
        self.allowlist = Arc::new(allowlist);
        self
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
        // Scanner pre-hook (ADR-0013). Every user-controlled blob
        // (prompts, tools, model, intent, plan, transcript, evals) is
        // staged via `store.put(&Object::Blob(..))`, so the scanner
        // must run here too — not only in `put_raw`. Trees and Commits
        // contain hashes + metadata, not user data, so they are
        // skipped.
        if let Object::Blob(blob) = object {
            let hits = self.scanner.scan(&blob.bytes);
            if !hits.is_empty() {
                let h = object.hash();
                if !self.allowlist.contains(&h) {
                    return Err(Error::SecretDetected { hits });
                }
            }
        }

        let bytes = serde_json::to_vec(object)?;
        let hash = object.hash();
        let path = self.path_for(&hash);
        if !path.exists() {
            self.write_at(&path, &bytes)?;
        }
        Ok(hash)
    }

    fn put_raw(&self, _kind: ObjectKind, bytes: &[u8]) -> Result<Hash> {
        // Scanner pre-hook (ADR-0013). Reject blobs with high-precision
        // pattern matches or high-entropy runs unless the blob's hash is
        // in the configured allowlist. The reject happens BEFORE any
        // bytes touch disk.
        let hits = self.scanner.scan(bytes);
        if !hits.is_empty() {
            let h = Hash::of(bytes);
            if !self.allowlist.contains(&h) {
                return Err(Error::SecretDetected { hits });
            }
        }

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
        check_integrity(hash, object.hash())?;
        Ok(object)
    }

    fn get_raw(&self, hash: &Hash) -> Result<Vec<u8>> {
        let path = self.path_for(hash);
        if !path.exists() {
            return Err(Error::NotFound(*hash));
        }
        let bytes = self.read_at(&path)?;
        // Raw objects are addressed by `Hash::of(bytes)` (the `put_raw`
        // contract). Verify before returning so a tampered segment or
        // manifest on disk is rejected, not parsed. Audit finding #3.
        check_integrity(hash, Hash::of(&bytes))?;
        Ok(bytes)
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

    #[test]
    fn put_raw_rejects_blob_with_secret() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let blob = b"hello\nAKIAIOSFODNN7EXAMPLE\nworld";
        match store.put_raw(ObjectKind::Blob, blob) {
            Err(Error::SecretDetected { hits }) => {
                assert!(hits.iter().any(|h| matches!(
                    &h.kind,
                    crate::scanner::HitKind::Pattern(n) if n == "aws_access_key_id"
                )));
            }
            other => panic!("expected SecretDetected, got {other:?}"),
        }
        // Confirm no object was written.
        let h = Hash::of(blob);
        assert!(!store.has(&h));
    }

    #[test]
    fn put_raw_allowlist_suppresses_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let blob = b"hello\nAKIAIOSFODNN7EXAMPLE\nworld";
        let h = Hash::of(blob);
        let toml_text = format!(
            r#"
            [[ignore]]
            blob_hash = "{}"
        "#,
            h.to_hex()
        );
        let al = crate::scanner::Allowlist::from_toml(&toml_text).unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap().with_allowlist(al);
        let hash = store
            .put_raw(ObjectKind::Blob, blob)
            .expect("allowlisted blob should put cleanly");
        assert_eq!(hash, h);
        assert!(store.has(&h));
    }

    #[test]
    fn put_rejects_blob_object_with_secret() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let bytes = b"hello\nAKIAIOSFODNN7EXAMPLE\nworld".to_vec();
        let obj = Object::Blob(Blob::new(bytes.clone()));
        match store.put(&obj) {
            Err(Error::SecretDetected { hits }) => {
                assert!(hits.iter().any(|h| matches!(
                    &h.kind,
                    crate::scanner::HitKind::Pattern(n) if n == "aws_access_key_id"
                )));
            }
            other => panic!("expected SecretDetected, got {other:?}"),
        }
        // Confirm no object was written.
        let h = Hash::of(&bytes);
        assert!(!store.has(&h));
    }

    #[test]
    fn put_blob_allowlist_suppresses_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"hello\nAKIAIOSFODNN7EXAMPLE\nworld".to_vec();
        let obj = Object::Blob(Blob::new(bytes.clone()));
        let h = obj.hash();
        let toml_text = format!(
            r#"
            [[ignore]]
            blob_hash = "{}"
        "#,
            h.to_hex()
        );
        let al = crate::scanner::Allowlist::from_toml(&toml_text).unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap().with_allowlist(al);
        let hash = store
            .put(&obj)
            .expect("allowlisted blob object should put cleanly");
        assert_eq!(hash, h);
        assert!(store.has(&h));
    }

    #[test]
    fn put_raw_rejects_blob_with_high_entropy_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        // 30 base64-ish chars with high entropy — same shape as the
        // entropy_detector_catches_high_entropy_run unit test in scanner.rs.
        let blob = b"data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi";
        match store.put_raw(crate::ObjectKind::Blob, blob) {
            Err(Error::SecretDetected { hits }) => {
                assert!(hits
                    .iter()
                    .any(|h| h.kind == crate::scanner::HitKind::HighEntropy));
            }
            other => panic!("expected SecretDetected (HighEntropy), got {other:?}"),
        }
    }

    // Audit finding #3: a raw object (segment/manifest) tampered with on
    // disk while keeping its object key must be rejected on get_raw, not
    // returned for parsing.
    #[test]
    fn get_raw_rejects_tampered_object_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let hash = store
            .put_raw(ObjectKind::Segment, b"canonical segment bytes")
            .unwrap();

        // Overwrite the stored object with different bytes at the same
        // path (same content address) — the exact shape of a corrupted
        // disk or a store writer that altered an object in place.
        let path = store.path_for(&hash);
        store.write_at(&path, b"tampered bytes").unwrap();

        match store.get_raw(&hash) {
            Err(Error::IntegrityError { declared, computed }) => {
                assert_eq!(declared, hash);
                assert_eq!(computed, Hash::of(b"tampered bytes"));
                assert_ne!(computed, declared);
            }
            other => panic!("expected IntegrityError, got {other:?}"),
        }
    }

    #[test]
    fn get_raw_returns_untampered_object() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let hash = store
            .put_raw(ObjectKind::Segment, b"canonical segment bytes")
            .unwrap();
        assert_eq!(store.get_raw(&hash).unwrap(), b"canonical segment bytes");
    }

    #[test]
    fn put_raw_clean_blob_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let blob = b"normal-looking content";
        let h = store
            .put_raw(ObjectKind::Blob, blob)
            .expect("clean blob should put");
        assert_eq!(h, Hash::of(blob));
        assert!(store.has(&h));
    }
}
