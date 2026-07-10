//! GCS-backed `ObjectStore`.
//!
//! Implements ADR-0004 Decision 5: every object goes through to Google
//! Cloud Storage (write-through), with a read-through local cache that
//! makes diff and replay free of network cost.
//!
//! Bytes on the wire are wire-compatible with [`FsObjectStore`]:
//! zstd-compressed JSON of an [`Object`] for [`ObjectStore::put`], or
//! raw zstd-compressed user bytes for [`ObjectStore::put_raw`]. A
//! future migration tool can copy in either direction with no
//! transformation.
//!
//! ## Endpoint override
//!
//! The default endpoint is GCS's public host
//! (`https://storage.googleapis.com`). Pass an endpoint override into
//! [`GcsObjectStore::new`] for `fake-gcs-server` or any other
//! GCS-JSON-API-compatible backend — that's how the integration tests
//! run without real GCP credentials.
//!
//! ## Auth
//!
//! For v1.0 we accept an optional bearer token. On a Cloud Run worker
//! the sidecar reads the token from the GCE metadata server before
//! constructing the store; for local dev / tests against fake-gcs the
//! token is `None` and no `Authorization` header is sent. Full Google
//! ADC integration (refreshing tokens from the metadata server on
//! expiry) lands when the sidecar work begins — for now keep it
//! explicit.
//!
//! ## Threading
//!
//! Uses `reqwest::blocking` so the [`ObjectStore`] trait stays sync.
//! That blocks the calling thread for the duration of a GCS round trip
//! (~50–200 ms per ADR-0004); inside the daemon's tokio runtime that
//! is acceptable because the commit lock already serialises writers,
//! and read-through hits the local cache after the first call.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::hash::Hash;
use crate::object::{Object, ObjectKind};
use crate::scanner::{Allowlist, Scanner};
use crate::store::{check_integrity, ObjectStore};
use crate::{Error, Result};

/// Default upstream — GCS's public JSON API host.
pub const DEFAULT_GCS_ENDPOINT: &str = "https://storage.googleapis.com";

/// One HTTP request budget. GCS p99 round-trip for small objects is
/// well under this; we want a clean error rather than a hang if the
/// network drops.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Storage-class hint we set on every upload. Standard class is the
/// only sensible default for hot-path commit objects; the MVP doesn't
/// tier.
const STORAGE_CLASS: &str = "STANDARD";

#[derive(Debug, Clone)]
pub struct GcsObjectStore {
    bucket: String,
    /// Object-name prefix inside the bucket. Lets a single bucket host
    /// multiple repos without colliding on hash addresses.
    prefix: String,
    cache_dir: PathBuf,
    endpoint: String,
    bearer_token: Option<String>,
    client: reqwest::blocking::Client,
    scanner: Arc<Scanner>,
    allowlist: Arc<Allowlist>,
}

impl GcsObjectStore {
    pub fn new(
        bucket: impl Into<String>,
        prefix: impl Into<String>,
        cache_dir: impl Into<PathBuf>,
        endpoint: Option<String>,
        bearer_token: Option<String>,
    ) -> Result<Self> {
        let cache_dir = cache_dir.into();
        fs::create_dir_all(&cache_dir)?;
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| Error::Other(anyhow::anyhow!("building HTTP client: {e}")))?;
        Ok(Self {
            bucket: bucket.into(),
            prefix: prefix.into(),
            cache_dir,
            endpoint: endpoint.unwrap_or_else(|| DEFAULT_GCS_ENDPOINT.to_string()),
            bearer_token,
            client,
            scanner: Arc::new(Scanner::new()),
            allowlist: Arc::new(Allowlist::empty()),
        })
    }

    /// Replace the allowlist on this store. Builder-style so callers can
    /// chain `GcsObjectStore::new(...)?.with_allowlist(al)`.
    pub fn with_allowlist(mut self, allowlist: Allowlist) -> Self {
        self.allowlist = Arc::new(allowlist);
        self
    }

    // ---------- naming + caching -------------------------------------------

    /// GCS object name for a given hash. Sharded by the first two hex
    /// chars (matches the on-disk layout) so a ``gsutil ls gs://b/<p>``
    /// looks familiar.
    fn object_name(&self, hash: &Hash) -> String {
        let (a, b) = hash.shard();
        let p = self.prefix.trim_matches('/');
        if p.is_empty() {
            format!("{a}/{b}.zst")
        } else {
            format!("{p}/{a}/{b}.zst")
        }
    }

    fn cache_path(&self, hash: &Hash) -> PathBuf {
        let (a, b) = hash.shard();
        self.cache_dir.join(a).join(format!("{b}.zst"))
    }

    fn cache_read(&self, hash: &Hash) -> Option<Vec<u8>> {
        let path = self.cache_path(hash);
        let compressed = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(
                    target: "agentic-core::gcs_store",
                    error = %e,
                    hash = %hash.to_hex(),
                    "GCS cache_read failed with non-NotFound error; falling through to GCS fetch"
                );
                return None;
            }
        };
        zstd::stream::decode_all(&compressed[..]).ok()
    }

    fn cache_write_compressed(&self, hash: &Hash, compressed: &[u8]) -> Result<()> {
        let path = self.cache_path(hash);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // tmp + rename keeps the cache torn-write-safe; readers either
        // see the previous file or the new one.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, compressed)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    // ---------- HTTP plumbing ----------------------------------------------

    fn upload_url(&self, name: &str) -> String {
        let encoded = urlencode(name);
        format!(
            "{}/upload/storage/v1/b/{}/o?uploadType=media&name={}",
            self.endpoint.trim_end_matches('/'),
            self.bucket,
            encoded,
        )
    }

    fn download_url(&self, name: &str) -> String {
        let encoded = urlencode(name);
        format!(
            "{}/storage/v1/b/{}/o/{}?alt=media",
            self.endpoint.trim_end_matches('/'),
            self.bucket,
            encoded,
        )
    }

    fn metadata_url(&self, name: &str) -> String {
        let encoded = urlencode(name);
        format!(
            "{}/storage/v1/b/{}/o/{}",
            self.endpoint.trim_end_matches('/'),
            self.bucket,
            encoded,
        )
    }

    fn with_auth(
        &self,
        builder: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        if let Some(token) = &self.bearer_token {
            builder.bearer_auth(token)
        } else {
            builder
        }
    }

    fn http_err(context: &str, status: reqwest::StatusCode, body: &str) -> Error {
        Error::Other(anyhow::anyhow!("{context}: HTTP {status}: {body}"))
    }

    fn upload_compressed(&self, hash: &Hash, compressed: &[u8]) -> Result<()> {
        let name = self.object_name(hash);
        let url = self.upload_url(&name);
        let resp = self
            .with_auth(
                self.client
                    .post(&url)
                    .header("Content-Type", "application/octet-stream")
                    .header("x-goog-storage-class", STORAGE_CLASS),
            )
            .body(compressed.to_vec())
            .send()
            .map_err(|e| Error::Other(anyhow::anyhow!("upload {url}: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(Self::http_err("uploading object", status, &body));
        }
        Ok(())
    }

    /// Returns ``Some((decompressed_bytes, raw_compressed_bytes))`` if
    /// the object exists in GCS. The raw bytes are returned so the
    /// caller can populate the local cache without re-compressing.
    fn download_compressed(&self, hash: &Hash) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let name = self.object_name(hash);
        let url = self.download_url(&name);
        let resp = self
            .with_auth(self.client.get(&url))
            .send()
            .map_err(|e| Error::Other(anyhow::anyhow!("GET {url}: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(Self::http_err("downloading object", status, &body));
        }
        let compressed = resp
            .bytes()
            .map_err(|e| Error::Other(anyhow::anyhow!("reading body: {e}")))?
            .to_vec();
        let bytes = zstd::stream::decode_all(&compressed[..])?;
        Ok(Some((bytes, compressed)))
    }

    /// Fetch the bytes for `hash` and map them to `T`, where `map` also
    /// performs the integrity check as it maps.
    ///
    /// The check differs by caller — `get_raw` verifies `Hash::of(bytes)`
    /// and returns the bytes; `get` parses the typed object and verifies
    /// `object.hash()`, returning the object — so it's injected as a
    /// closure that owns the bytes (no separate verify pass, no double
    /// parse for `get`). Three integrity properties hold regardless of
    /// scheme (2026-07-09 audit finding #3):
    ///
    /// * A **poisoned cache hit** (bytes that fail the check) is evicted
    ///   and we fall through to GCS, which may still hold the intact
    ///   object — a merely-corrupted local cache self-heals.
    /// * An **unreadable cache file** (torn write / zstd decode failure,
    ///   so `cache_read` returns `None`) is likewise evicted, so `has`
    ///   stays consistent and future reads can heal.
    /// * A **corrupt download** is rejected *before* `cache_write`, so a
    ///   bad object is never written to the local cache ("never cache a
    ///   failed verification").
    fn fetch_map<T>(&self, hash: &Hash, map: impl Fn(Vec<u8>) -> Result<T>) -> Result<T> {
        let cache_path = self.cache_path(hash);
        if cache_path.exists() {
            match self.cache_read(hash) {
                Some(cached) => match map(cached) {
                    Ok(value) => return Ok(value),
                    Err(err) => {
                        tracing::warn!(
                            target: "agentic-core::gcs_store",
                            hash = %hash.to_hex(),
                            error = %err,
                            "cached object failed integrity check; evicting and refetching from GCS",
                        );
                        // Evict the poisoned entry so `has`/subsequent reads
                        // don't keep trusting it.
                        self.evict_cache_entry(hash, &cache_path);
                    }
                },
                None => {
                    // Cache file exists but is unreadable/corrupt (e.g. torn
                    // write or zstd decode failure). Evict so `has` stays
                    // consistent and future reads can heal from GCS.
                    self.evict_cache_entry(hash, &cache_path);
                }
            }
        }
        match self.download_compressed(hash)? {
            None => Err(Error::NotFound(*hash)),
            Some((bytes, compressed)) => {
                // Map (which verifies) BEFORE caching: a corrupt GCS object
                // must not be persisted to the local cache.
                let value = map(bytes)?;
                let _ = self.cache_write_compressed(hash, &compressed);
                Ok(value)
            }
        }
    }

    /// Best-effort eviction of a corrupt or poisoned cache entry. Removes
    /// it as a file, falling back to directory removal if the cache path
    /// was corrupted into a directory (which `remove_file` can't unlink).
    /// A failure other than "already gone" is logged — an un-evictable
    /// entry keeps `has()` reporting `true` while every read refetches, so
    /// it must be diagnosable rather than silently swallowed. Mirrors the
    /// unlink-logging discipline of the put-rollback path above.
    fn evict_cache_entry(&self, hash: &Hash, cache_path: &Path) {
        match fs::remove_file(cache_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(file_err) => {
                if cache_path.is_dir() && fs::remove_dir_all(cache_path).is_ok() {
                    return;
                }
                tracing::warn!(
                    target: "agentic-core::gcs_store",
                    hash = %hash.to_hex(),
                    error = %file_err,
                    path = %cache_path.display(),
                    "failed to evict corrupt/poisoned cache entry; has() may stay \
                     inconsistent and reads will keep refetching from GCS",
                );
            }
        }
    }

    fn remote_exists(&self, hash: &Hash) -> bool {
        let name = self.object_name(hash);
        let url = self.metadata_url(&name);
        // HEAD against the metadata endpoint is the cheapest existence
        // check the JSON API offers.
        let resp = self.with_auth(self.client.head(&url)).send();
        matches!(resp, Ok(r) if r.status().is_success())
    }
}

impl ObjectStore for GcsObjectStore {
    fn put(&self, object: &Object) -> Result<Hash> {
        // Scanner pre-hook (ADR-0013). Reject blobs containing secrets
        // BEFORE any compression / network I/O — by the time bytes
        // would hit GCS the secret has already left the daemon's
        // address space. Trees and Commits contain hashes + metadata,
        // not user data, so they are skipped.
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
        let compressed = zstd::stream::encode_all(&bytes[..], 3)?;
        // Cache locally first so a concurrent reader that lands during
        // the upload doesn't miss; on upload failure the cache entry
        // gets blown away. (The cache is a hint, not a source of
        // truth — see post-MVP `agentic gc` for sweeping.)
        self.cache_write_compressed(&hash, &compressed)?;
        if let Err(e) = self.upload_compressed(&hash, &compressed) {
            // Best-effort cache rollback so a failed upload can't masquerade as
            // a successful put on the next get/has. If the unlink itself fails
            // (permissions, transient FS error) we surface it via tracing so an
            // operator can investigate — but still return the upload error,
            // which is the user-visible cause.
            if let Err(unlink_err) = fs::remove_file(self.cache_path(&hash)) {
                if unlink_err.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        hash = %hash,
                        error = %unlink_err,
                        "failed to roll back cache entry after GCS upload error",
                    );
                }
            }
            return Err(e);
        }
        Ok(hash)
    }

    fn put_raw(&self, _kind: ObjectKind, bytes: &[u8]) -> Result<Hash> {
        // Scanner pre-hook (ADR-0013). Reject before any compression /
        // network I/O — by the time we'd hit GCS the secret has already
        // left the daemon's address space.
        let hits = self.scanner.scan(bytes);
        if !hits.is_empty() {
            let h = Hash::of(bytes);
            if !self.allowlist.contains(&h) {
                return Err(Error::SecretDetected { hits });
            }
        }

        let hash = Hash::of(bytes);
        let compressed = zstd::stream::encode_all(bytes, 3)?;
        self.cache_write_compressed(&hash, &compressed)?;
        if let Err(e) = self.upload_compressed(&hash, &compressed) {
            // Best-effort cache rollback so a failed upload can't masquerade as
            // a successful put on the next get/has. If the unlink itself fails
            // (permissions, transient FS error) we surface it via tracing so an
            // operator can investigate — but still return the upload error,
            // which is the user-visible cause.
            if let Err(unlink_err) = fs::remove_file(self.cache_path(&hash)) {
                if unlink_err.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        hash = %hash,
                        error = %unlink_err,
                        "failed to roll back cache entry after GCS upload error",
                    );
                }
            }
            return Err(e);
        }
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> Result<Object> {
        // Typed objects are addressed by `object.hash()`. Parse once inside
        // the map closure and verify that hash — no second deserialize.
        self.fetch_map(hash, |bytes| {
            let object: Object = serde_json::from_slice(&bytes)?;
            check_integrity(hash, object.hash())?;
            Ok(object)
        })
    }

    fn get_raw(&self, hash: &Hash) -> Result<Vec<u8>> {
        // Raw objects are addressed by `Hash::of(bytes)` (the `put_raw`
        // contract) — verify against that, then return the bytes
        // unchanged (zero-copy: `bytes` moves out). Audit finding #3.
        self.fetch_map(hash, |bytes| {
            check_integrity(hash, Hash::of(&bytes))?;
            Ok(bytes)
        })
    }

    fn has(&self, hash: &Hash) -> bool {
        if self.cache_path(hash).exists() {
            return true;
        }
        self.remote_exists(hash)
    }
}

/// Percent-encode the few characters that appear in GCS object names
/// after sharding (the rest are hex digits and dots). `/` is the only
/// non-unreserved character we expect, and the GCS JSON API requires it
/// percent-encoded (`%2F`) inside the resource path even though it is a
/// legal object-name character — so we always encode it here. Everything
/// else outside the RFC 3986 unreserved set is also percent-encoded.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else if c == '/' {
            // GCS treats "/" as part of the object name; the JSON API
            // requires it percent-encoded inside the resource path.
            out.push_str("%2F");
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn object_name_shards_and_prefixes() {
        let dir = tempdir().unwrap();
        let store = GcsObjectStore::new(
            "test-bucket",
            "repo-A",
            dir.path(),
            Some("http://localhost:4443".into()),
            None,
        )
        .unwrap();
        let h = Hash::of(b"hello");
        let name = store.object_name(&h);
        let hex = h.to_hex();
        assert!(name.starts_with("repo-A/"));
        assert!(name.contains(&format!("{}/", &hex[..2])));
        assert!(name.ends_with(".zst"));
        assert!(name.contains(&hex[2..]));
    }

    #[test]
    fn empty_prefix_skips_leading_slash() {
        let dir = tempdir().unwrap();
        let store = GcsObjectStore::new(
            "test-bucket",
            "",
            dir.path(),
            Some("http://localhost:4443".into()),
            None,
        )
        .unwrap();
        let h = Hash::of(b"hello");
        let name = store.object_name(&h);
        assert!(!name.starts_with('/'));
        let hex = h.to_hex();
        assert!(name.starts_with(&format!("{}/", &hex[..2])));
    }

    #[test]
    fn urlencode_handles_slash_and_unreserved() {
        assert_eq!(urlencode("abc"), "abc");
        assert_eq!(urlencode("a/b"), "a%2Fb");
        assert_eq!(urlencode("a.b-c_d~e"), "a.b-c_d~e");
        assert_eq!(urlencode("a b"), "a%20b");
    }

    #[test]
    fn cache_read_returns_none_when_missing() {
        let dir = tempdir().unwrap();
        let store = GcsObjectStore::new(
            "test-bucket",
            "p",
            dir.path(),
            Some("http://localhost:4443".into()),
            None,
        )
        .unwrap();
        let h = Hash::of(b"nope");
        assert!(store.cache_read(&h).is_none());
    }

    #[test]
    fn cache_write_and_read_roundtrip() {
        let dir = tempdir().unwrap();
        let store = GcsObjectStore::new(
            "test-bucket",
            "p",
            dir.path(),
            Some("http://localhost:4443".into()),
            None,
        )
        .unwrap();
        let h = Hash::of(b"hello");
        let payload = b"hello-world";
        let compressed = zstd::stream::encode_all(&payload[..], 3).unwrap();
        store.cache_write_compressed(&h, &compressed).unwrap();
        let got = store.cache_read(&h).unwrap();
        assert_eq!(got, payload);
    }

    // Audit finding #3: a poisoned local cache entry (bytes that don't
    // match their content address) must never be returned. It is evicted
    // and the read falls through to GCS. Here the fake endpoint refuses
    // the connection, so the fall-through fails — the point of the test
    // is that get_raw does NOT return the poisoned bytes, and that the
    // bad entry is gone afterwards. Deterministic, no server needed.
    #[test]
    fn cache_hit_poisoned_is_evicted_and_not_returned() {
        let dir = tempdir().unwrap();
        // Reserve a loopback port then drop the listener so connects to it
        // get an immediate, deterministic ECONNREFUSED — more robust than
        // assuming a fixed low port (e.g. :1) happens to be closed.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_addr = listener.local_addr().unwrap();
        drop(listener);
        let store = GcsObjectStore::new(
            "test-bucket",
            "p",
            dir.path(),
            Some(format!("http://{closed_addr}")),
            None,
        )
        .unwrap();

        // Address is Hash::of(good), but we plant Hash-mismatched bytes at
        // that cache slot — the shape of a corrupted local cache/disk.
        let good = b"the canonical bytes";
        let hash = Hash::of(good);
        let poison = zstd::stream::encode_all(&b"tampered!!"[..], 3).unwrap();
        store.cache_write_compressed(&hash, &poison).unwrap();
        assert!(store.cache_path(&hash).exists());

        let result = store.get_raw(&hash);
        // Never returns the poison. (The fall-through GCS fetch errors
        // because nothing is listening — that's fine; not-returning-poison
        // is the property under test.)
        if let Ok(bytes) = &result {
            assert_ne!(
                bytes.as_slice(),
                b"tampered!!",
                "poisoned cache bytes must never be returned"
            );
        }
        assert!(result.is_err(), "no server to heal from, so read errors");
        assert!(
            !store.cache_path(&hash).exists(),
            "poisoned cache entry must be evicted"
        );
    }

    // A cache path corrupted into a directory can't be unlinked with
    // remove_file; evict_cache_entry must fall back to directory removal so
    // an un-evictable entry doesn't keep `has()` inconsistent forever.
    #[test]
    fn cache_entry_corrupted_into_directory_is_evicted() {
        let dir = tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_addr = listener.local_addr().unwrap();
        drop(listener);
        let store = GcsObjectStore::new(
            "test-bucket",
            "p",
            dir.path(),
            Some(format!("http://{closed_addr}")),
            None,
        )
        .unwrap();

        // Corrupt the cache slot into a directory (unreadable as a file, so
        // cache_read returns None → the eviction path runs).
        let hash = Hash::of(b"anything");
        let cache_path = store.cache_path(&hash);
        std::fs::create_dir_all(&cache_path).unwrap();
        assert!(cache_path.is_dir());

        let _ = store.get_raw(&hash);
        assert!(
            !cache_path.exists(),
            "a cache entry corrupted into a directory must still be evicted"
        );
    }
}
