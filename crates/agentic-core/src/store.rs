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
use crate::object::{Blob, Commit, Object, ObjectKind, Tree};
use crate::scanner::{Allowlist, ScanPolicy, Scanner};
use crate::{Error, Result};

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Counter for [`FsObjectStore::write_at`] temp file names.
static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

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

/// Reject a blob whose bytes are exactly the canonical JSON of a `Tree` or
/// `Commit`. Those objects hash `json(inner)`, so such a blob would share a
/// content address with the typed object while being stored under a
/// different envelope; whichever write came first would win the address.
pub(crate) fn check_blob_unambiguous(bytes: &[u8]) -> Result<()> {
    let canonical_tree = serde_json::from_slice::<Tree>(bytes)
        .ok()
        .is_some_and(|t| serde_json::to_vec(&t).is_ok_and(|c| c == bytes));
    let canonical_commit = serde_json::from_slice::<Commit>(bytes)
        .ok()
        .is_some_and(|c| serde_json::to_vec(&c).is_ok_and(|c| c == bytes));
    if canonical_tree || canonical_commit {
        return Err(Error::AmbiguousBlob {
            hash: Hash::of(bytes),
        });
    }
    Ok(())
}

/// Decode the bytes stored at a typed address and verify them against it.
///
/// Stores written before raw objects got their own namespace may hold bare
/// `put_raw` bytes at a typed address. If the stored bytes do not decode to
/// an object with the requested hash but hash to it directly, they are a
/// legacy raw object, and a blob with the same content is the right answer.
pub(crate) fn decode_typed(hash: &Hash, stored: Vec<u8>) -> Result<Object> {
    let parsed = serde_json::from_slice::<Object>(&stored);
    if let Ok(object) = &parsed {
        if object.hash() == *hash {
            return parsed.map_err(Error::from);
        }
    }
    if Hash::of(&stored) == *hash {
        return Ok(Object::Blob(Blob::new(stored)));
    }
    let object = parsed?;
    check_integrity(hash, object.hash())?;
    Ok(object)
}

/// What an occupied address holds, relative to the object being written.
pub(crate) enum Occupant {
    /// The same content: a dedup hit.
    Same,
    /// A valid object that verifies at this address but is not the one
    /// being written. Never replaced.
    Different,
    /// Undecodable or failing its integrity check: corruption or a
    /// pre-fix squatter. Safe to replace.
    Corrupt,
}

/// Judge the bytes stored at typed address `hash` against `object`, exactly
/// as `get` would read them.
pub(crate) fn classify_typed(hash: &Hash, stored: Vec<u8>, object: &Object) -> Occupant {
    match decode_typed(hash, stored) {
        Ok(existing) if existing == *object => Occupant::Same,
        Ok(_) => Occupant::Different,
        Err(_) => Occupant::Corrupt,
    }
}

/// Judge the bytes stored at raw address `hash`. The address determines
/// the content, so a verified occupant is always the same object.
pub(crate) fn classify_raw(hash: &Hash, stored: &[u8]) -> Occupant {
    if Hash::of(stored) == *hash {
        Occupant::Same
    } else {
        Occupant::Corrupt
    }
}

/// Read a raw object found at a legacy (typed) address. Besides bare
/// `put_raw` bytes, that address may hold a blob envelope written by a
/// `put` of the same bytes; both carry the same content, so both are valid.
pub(crate) fn decode_legacy_raw(hash: &Hash, stored: Vec<u8>) -> Result<Vec<u8>> {
    let computed = Hash::of(&stored);
    if computed == *hash {
        return Ok(stored);
    }
    if let Ok(Object::Blob(blob)) = serde_json::from_slice::<Object>(&stored) {
        if blob.hash() == *hash {
            return Ok(blob.bytes);
        }
    }
    Err(Error::IntegrityError {
        declared: *hash,
        computed,
    })
}

/// The store contract. MVP implementer is `FsObjectStore`; future
/// implementers include S3, GCS, and a network-replicated store.
pub trait ObjectStore: Send + Sync {
    /// `put` with a per-call scan policy (ADR-0017). Policy only affects
    /// `Object::Blob` — Trees and Commits are never scanned.
    fn put_with_policy(&self, object: &Object, policy: ScanPolicy) -> Result<Hash>;

    fn put(&self, object: &Object) -> Result<Hash> {
        self.put_with_policy(object, ScanPolicy::default())
    }

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

    /// Typed objects (`put_with_policy`): `<root>/<ab>/<rest>.zst`.
    fn path_for(&self, hash: &Hash) -> PathBuf {
        let (prefix, rest) = hash.shard();
        self.root.join(prefix).join(format!("{rest}.zst"))
    }

    /// Raw objects (`put_raw`): `<root>/raw/<ab>/<rest>.raw`. A separate
    /// namespace, with its own suffix, so bare bytes and typed envelopes
    /// can never claim the same file, even for a store rooted at `<p>/raw`.
    fn raw_path_for(&self, hash: &Hash) -> PathBuf {
        let (prefix, rest) = hash.shard();
        self.root
            .join("raw")
            .join(prefix)
            .join(format!("{rest}.raw"))
    }

    /// Write `bytes` at `path` unless it is already occupied. `classify`
    /// judges the occupant exactly as the matching read would: the same
    /// content is a dedup hit, a valid object of another kind is refused
    /// (nothing is written), and a missing or corrupt occupant is replaced
    /// atomically. "Corrupt" means the bytes were read but do not decode or
    /// verify; a read that fails for any other reason (for example EACCES)
    /// is an error, not licence to replace.
    fn write_unless_occupied(
        &self,
        hash: &Hash,
        path: &Path,
        bytes: &[u8],
        classify: impl FnOnce(Vec<u8>) -> Occupant,
    ) -> Result<()> {
        match std::fs::read(path) {
            Ok(compressed) => {
                let occupant = match zstd::stream::decode_all(&compressed[..]) {
                    Ok(stored) => classify(stored),
                    Err(_) => Occupant::Corrupt,
                };
                match occupant {
                    Occupant::Same => return Ok(()),
                    Occupant::Different => return Err(Error::AddressOccupied { hash: *hash }),
                    Occupant::Corrupt => {}
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.write_at(path, bytes)
    }

    fn write_at(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write to a uniquely named temp file in the same directory and
        // atomically rename, so a crash mid-write never leaves a torn object
        // in the store and no other writer (including a store whose root
        // nests this one) can share the temp file.
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let compressed = zstd::stream::encode_all(bytes, 3)?;
        // Retry on a taken name (another process with the same pid in a
        // different namespace); only a file this call created is ever removed.
        let mut opened = None;
        for _ in 0..16 {
            let tmp = path.with_file_name(format!(
                "{name}.{}.{}.tmp",
                std::process::id(),
                NEXT_TMP.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
            {
                Ok(file) => {
                    opened = Some((tmp, file));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let Some((tmp, mut file)) = opened else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("no free temp file name next to {}", path.display()),
            )
            .into());
        };
        let written = (|| {
            file.write_all(&compressed)?;
            drop(file);
            std::fs::rename(&tmp, path)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    fn read_at(&self, path: &Path) -> Result<Vec<u8>> {
        let compressed = std::fs::read(path)?;
        let bytes = zstd::stream::decode_all(&compressed[..])?;
        Ok(bytes)
    }
}

impl ObjectStore for FsObjectStore {
    fn put_with_policy(&self, object: &Object, policy: ScanPolicy) -> Result<Hash> {
        // Scanner pre-hook (ADR-0013). Every user-controlled blob
        // (prompts, tools, model, intent, plan, transcript, evals) is
        // staged via `store.put(&Object::Blob(..))`, so the scanner
        // must run here too — not only in `put_raw`. Trees and Commits
        // contain hashes + metadata, not user data, so they are
        // skipped.
        if let Object::Blob(blob) = object {
            let hits = self.scanner.scan_with(&blob.bytes, policy);
            if !hits.is_empty() {
                let h = object.hash();
                if !self.allowlist.contains(&h) {
                    return Err(Error::SecretDetected { hits });
                }
            }
        }

        if let Object::Blob(blob) = object {
            check_blob_unambiguous(&blob.bytes)?;
        }

        let bytes = serde_json::to_vec(object)?;
        let hash = object.hash();
        let path = self.path_for(&hash);
        self.write_unless_occupied(&hash, &path, &bytes, |stored| {
            classify_typed(&hash, stored, object)
        })?;
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
        let path = self.raw_path_for(&hash);
        self.write_unless_occupied(&hash, &path, bytes, |stored| classify_raw(&hash, &stored))?;
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> Result<Object> {
        let path = self.path_for(hash);
        if !path.exists() {
            return Err(Error::NotFound(*hash));
        }
        let bytes = self.read_at(&path)?;
        // Integrity check: did we get back what the address says we should?
        decode_typed(hash, bytes)
    }

    fn get_raw(&self, hash: &Hash) -> Result<Vec<u8>> {
        let raw_path = self.raw_path_for(hash);
        if !raw_path.exists() {
            // Legacy layout: raw objects used to share the typed path.
            let legacy = self.path_for(hash);
            if !legacy.exists() {
                return Err(Error::NotFound(*hash));
            }
            return decode_legacy_raw(hash, self.read_at(&legacy)?);
        }
        let bytes = self.read_at(&raw_path)?;
        // Raw objects are addressed by `Hash::of(bytes)` (the `put_raw`
        // contract). Verify before returning so a tampered segment or
        // manifest on disk is rejected, not parsed. Audit finding #3.
        check_integrity(hash, Hash::of(&bytes))?;
        Ok(bytes)
    }

    fn has(&self, hash: &Hash) -> bool {
        self.path_for(hash).exists() || self.raw_path_for(hash).exists()
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
        let path = store.raw_path_for(&hash);
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

    #[test]
    fn put_with_policy_skip_entropy_accepts_high_entropy_blob() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let bytes = b"data: aB3xQ9zPmK7nR2vL5jH8wY4tF6cN1oUgEi".to_vec();
        let obj = Object::Blob(Blob::new(bytes));
        let hash = store
            .put_with_policy(&obj, crate::scanner::ScanPolicy { skip_entropy: true })
            .expect("entropy-exempt blob must be accepted");
        assert!(store.has(&hash));
    }

    #[test]
    fn put_with_policy_skip_entropy_still_rejects_pattern_hits() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let bytes = b"hello\nAKIAIOSFODNN7EXAMPLE\nworld".to_vec();
        let obj = Object::Blob(Blob::new(bytes));
        match store.put_with_policy(&obj, crate::scanner::ScanPolicy { skip_entropy: true }) {
            Err(Error::SecretDetected { hits }) => {
                assert!(hits.iter().any(|h| matches!(
                    &h.kind,
                    crate::scanner::HitKind::Pattern(n) if n == "aws_access_key_id"
                )));
            }
            other => panic!("expected SecretDetected, got {other:?}"),
        }
    }

    fn sample_commit() -> Commit {
        Commit {
            parent: None,
            author: "a".into(),
            timestamp: chrono::DateTime::from_timestamp(0, 0).unwrap(),
            message: "m".into(),
            code_sha: None,
            prompts: None,
            tools: None,
            model: None,
            memory_snapshot: None,
            schema_version: None,
            intent: None,
            plan: None,
            transcript: None,
            evals: None,
            cost_cents: 0,
            signatures: Vec::new(),
            peer_uid: None,
        }
    }

    #[test]
    fn blob_put_does_not_shadow_later_put_raw() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let b = b"segment-like bytes".to_vec();
        store.put(&Object::Blob(Blob::new(b.clone()))).unwrap();
        let h = store.put_raw(ObjectKind::Segment, &b).unwrap();
        assert_eq!(store.get_raw(&h).unwrap(), b);
    }

    #[test]
    fn put_raw_does_not_shadow_later_blob_put() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let b = b"segment-like bytes".to_vec();
        let h = store.put_raw(ObjectKind::Segment, &b).unwrap();
        store.put(&Object::Blob(Blob::new(b.clone()))).unwrap();
        match store.get(&h).unwrap() {
            Object::Blob(got) => assert_eq!(got.bytes, b),
            other => panic!("expected Blob, got {other:?}"),
        }
        assert_eq!(store.get_raw(&h).unwrap(), b);
    }

    #[test]
    fn blob_equal_to_canonical_tree_or_commit_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();

        let tree_json = serde_json::to_vec(&Tree::new()).unwrap();
        let commit_json = serde_json::to_vec(&sample_commit()).unwrap();
        for bytes in [tree_json, commit_json] {
            match store.put(&Object::Blob(Blob::new(bytes.clone()))) {
                Err(Error::AmbiguousBlob { hash }) => assert_eq!(hash, Hash::of(&bytes)),
                other => panic!("expected AmbiguousBlob, got {other:?}"),
            }
        }

        // Ordinary JSON, including a non-canonical spelling of a tree, is fine.
        store
            .put(&Object::Blob(Blob::new(br#"{"prompt":"hi"}"#.to_vec())))
            .unwrap();
        store
            .put(&Object::Blob(Blob::new(br#"{ "entries": {} }"#.to_vec())))
            .unwrap();
    }

    #[test]
    fn legacy_raw_layout_is_still_readable() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let bytes = b"written by an older version".to_vec();
        let hash = Hash::of(&bytes);
        store.write_at(&store.path_for(&hash), &bytes).unwrap();

        assert!(store.has(&hash));
        assert_eq!(store.get_raw(&hash).unwrap(), bytes);
        match store.get(&hash).unwrap() {
            Object::Blob(got) => assert_eq!(got.bytes, bytes),
            other => panic!("expected Blob, got {other:?}"),
        }
    }

    #[test]
    fn typed_put_heals_a_wrong_occupant() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let b = b"some prompt".to_vec();
        let hash = Hash::of(&b);
        store.write_at(&store.path_for(&hash), b"squatter").unwrap();

        store.put(&Object::Blob(Blob::new(b.clone()))).unwrap();
        match store.get(&hash).unwrap() {
            Object::Blob(got) => assert_eq!(got.bytes, b),
            other => panic!("expected Blob, got {other:?}"),
        }
    }

    #[test]
    fn raw_put_heals_a_wrong_occupant() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let b = b"segment bytes".to_vec();
        let hash = Hash::of(&b);
        store
            .write_at(&store.raw_path_for(&hash), b"corrupted")
            .unwrap();

        store.put_raw(ObjectKind::Segment, &b).unwrap();
        assert_eq!(store.get_raw(&hash).unwrap(), b);
    }

    #[test]
    fn blob_put_does_not_destroy_a_legacy_raw_object() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let b = b"legacy segment bytes".to_vec();
        let hash = Hash::of(&b);
        store.write_at(&store.path_for(&hash), &b).unwrap();

        store.put(&Object::Blob(Blob::new(b.clone()))).unwrap();
        assert_eq!(store.get_raw(&hash).unwrap(), b);
    }

    #[test]
    fn legacy_raw_read_tolerates_a_blob_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let b = b"legacy segment bytes".to_vec();
        let hash = Hash::of(&b);
        let envelope = serde_json::to_vec(&Object::Blob(Blob::new(b.clone()))).unwrap();
        store.write_at(&store.path_for(&hash), &envelope).unwrap();

        assert_eq!(store.get_raw(&hash).unwrap(), b);
    }

    // A valid object of another kind owns the address (a legacy Blob whose
    // bytes are canonical Tree JSON): the Tree put is refused and the Blob
    // stays readable.
    #[test]
    fn typed_put_refuses_a_valid_other_kind_occupant() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let tree = Tree::new();
        let tree_json = serde_json::to_vec(&tree).unwrap();
        let hash = tree.hash();
        let envelope = serde_json::to_vec(&Object::Blob(Blob::new(tree_json.clone()))).unwrap();
        store.write_at(&store.path_for(&hash), &envelope).unwrap();

        match store.put(&Object::Tree(tree)) {
            Err(Error::AddressOccupied { hash: h }) => assert_eq!(h, hash),
            other => panic!("expected AddressOccupied, got {other:?}"),
        }
        match store.get(&hash).unwrap() {
            Object::Blob(got) => assert_eq!(got.bytes, tree_json),
            other => panic!("expected the Blob to survive, got {other:?}"),
        }
    }

    // Stores rooted at `<p>` and `<p>/raw` must not share a file.
    #[test]
    fn raw_paths_cannot_collide_with_typed_paths_of_a_raw_root() {
        let dir = tempfile::tempdir().unwrap();
        let outer = FsObjectStore::open(dir.path()).unwrap();
        let inner = FsObjectStore::open(dir.path().join("raw")).unwrap();
        let h = Hash::of(b"x");
        assert_ne!(outer.raw_path_for(&h), inner.path_for(&h));
    }

    // An occupant that cannot be read (here: no permission) is neither a
    // dedup hit nor corruption; the put fails and the file is untouched.
    #[cfg(unix)]
    #[test]
    fn unreadable_occupant_fails_the_put_and_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let b = b"some prompt".to_vec();
        let hash = Hash::of(&b);
        let path = store.path_for(&hash);
        store.write_at(&path, b"occupant").unwrap();
        let before = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            eprintln!("running as root: permissions not enforced, skipping");
            return;
        }

        let result = store.put(&Object::Blob(Blob::new(b)));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    // Writers never share a temp file, so a foreign file at the old shared
    // temp name survives, and a failed write leaves no temp file behind.
    #[test]
    fn write_at_uses_private_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let hash = Hash::of(b"x");
        let path = store.path_for(&hash);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let foreign = path.with_extension("tmp");
        std::fs::write(&foreign, b"someone else's temp").unwrap();

        store.write_at(&path, b"payload").unwrap();
        assert_eq!(std::fs::read(&foreign).unwrap(), b"someone else's temp");
        assert_eq!(store.read_at(&path).unwrap(), b"payload");

        // Rename onto a non-empty directory fails; the temp file must go.
        let blocked = store.path_for(&Hash::of(b"y"));
        std::fs::create_dir_all(blocked.join("child")).unwrap();
        assert!(store.write_at(&blocked, b"payload").is_err());
        let leftovers: Vec<_> = std::fs::read_dir(blocked.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn write_at_skips_and_preserves_an_occupied_temp_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let path = store.path_for(&Hash::of(b"z"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let pid = std::process::id();
        // Occupy the next candidate names this process would pick.
        let start = NEXT_TMP.load(Ordering::Relaxed);
        let planted: Vec<_> = (start..start + 4)
            .map(|n| path.with_file_name(format!("{name}.{pid}.{n}.tmp")))
            .collect();
        for p in &planted {
            std::fs::write(p, b"other writer").unwrap();
        }
        store.write_at(&path, b"payload").unwrap();
        for p in &planted {
            assert_eq!(std::fs::read(p).unwrap(), b"other writer");
        }
        assert_eq!(store.read_at(&path).unwrap(), b"payload");
    }
}
