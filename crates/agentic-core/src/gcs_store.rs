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
use crate::scanner::{Allowlist, ScanPolicy, Scanner};
use crate::store::{
    check_blob_unambiguous, check_integrity, decode_legacy_raw, decode_typed, ObjectStore,
};
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

/// Which address namespace an object lives in. Typed objects are JSON
/// envelopes; raw objects are bare bytes. They must never share a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ns {
    Typed,
    Raw,
}

impl Ns {
    /// (directory prefix, file extension) of this namespace's objects.
    fn layout(self) -> (&'static str, &'static str) {
        match self {
            Ns::Typed => ("", "zst"),
            Ns::Raw => ("raw/", "raw.zst"),
        }
    }
}

/// What a conditional upload did.
#[derive(Debug, PartialEq, Eq)]
enum Upload {
    Stored,
    /// HTTP 412: the object exists (or, for a generation match, changed).
    PreconditionFailed,
}

/// A downloaded object: the stored (compressed) bytes and the GCS
/// generation they were read at, when the server reported one.
struct Downloaded {
    compressed: Vec<u8>,
    generation: Option<String>,
}

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
    /// looks familiar. Raw objects (`put_raw`) live under `raw/` with a
    /// distinct `.raw.zst` suffix, so they can never share a name with a
    /// typed object, even for a store whose prefix is `<p>/raw`.
    fn object_name(&self, hash: &Hash, ns: Ns) -> String {
        let (a, b) = hash.shard();
        let p = self.prefix.trim_matches('/');
        let (dir, ext) = ns.layout();
        if p.is_empty() {
            format!("{dir}{a}/{b}.{ext}")
        } else {
            format!("{p}/{dir}{a}/{b}.{ext}")
        }
    }

    fn cache_path(&self, hash: &Hash, ns: Ns) -> PathBuf {
        let (a, b) = hash.shard();
        let (dir, ext) = ns.layout();
        self.cache_dir
            .join(format!("{dir}{a}"))
            .join(format!("{b}.{ext}"))
    }

    fn cache_read(&self, hash: &Hash, ns: Ns) -> Option<Vec<u8>> {
        let path = self.cache_path(hash, ns);
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

    fn cache_write_compressed(&self, hash: &Hash, ns: Ns, compressed: &[u8]) -> Result<()> {
        let path = self.cache_path(hash, ns);
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

    /// `if_generation` is the `ifGenerationMatch` precondition: `0` means
    /// "only if the object does not exist yet".
    fn upload_url(&self, name: &str, if_generation: &str) -> String {
        let encoded = urlencode(name);
        format!(
            "{}/upload/storage/v1/b/{}/o?uploadType=media&ifGenerationMatch={}&name={}",
            self.endpoint.trim_end_matches('/'),
            self.bucket,
            if_generation,
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

    /// Conditional upload (`ifGenerationMatch=<if_generation>`): an existing
    /// object is never overwritten blindly. HTTP 412 is reported to the
    /// caller, which must decide what the existing object means.
    fn upload_compressed(
        &self,
        hash: &Hash,
        ns: Ns,
        compressed: &[u8],
        if_generation: &str,
    ) -> Result<Upload> {
        let name = self.object_name(hash, ns);
        let url = self.upload_url(&name, if_generation);
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
        if resp.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            return Ok(Upload::PreconditionFailed);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(Self::http_err("uploading object", status, &body));
        }
        Ok(Upload::Stored)
    }

    /// Returns the stored object if it exists in GCS. The compressed bytes
    /// are returned as stored so the caller can populate the local cache
    /// without re-compressing; decoding is left to the caller.
    fn download_compressed(&self, hash: &Hash, ns: Ns) -> Result<Option<Downloaded>> {
        let name = self.object_name(hash, ns);
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
        let generation = resp
            .headers()
            .get("x-goog-generation")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let compressed = resp
            .bytes()
            .map_err(|e| Error::Other(anyhow::anyhow!("reading body: {e}")))?
            .to_vec();
        Ok(Some(Downloaded {
            compressed,
            generation,
        }))
    }

    /// Current generation of an object, from the metadata endpoint. Only
    /// used when a download did not report one.
    fn remote_generation(&self, hash: &Hash, ns: Ns) -> Result<String> {
        let url = self.metadata_url(&self.object_name(hash, ns));
        let resp = self
            .with_auth(self.client.get(&url))
            .send()
            .map_err(|e| Error::Other(anyhow::anyhow!("GET {url}: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            return Err(Self::http_err("reading object metadata", status, &body));
        }
        let meta: serde_json::Value = resp
            .json()
            .map_err(|e| Error::Other(anyhow::anyhow!("parsing object metadata: {e}")))?;
        meta.get("generation")
            .and_then(|g| {
                g.as_str()
                    .map(str::to_owned)
                    .or_else(|| g.as_u64().map(|n| n.to_string()))
            })
            .ok_or_else(|| Error::Other(anyhow::anyhow!("object metadata has no generation")))
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
    fn fetch_map<T>(&self, hash: &Hash, ns: Ns, map: impl Fn(Vec<u8>) -> Result<T>) -> Result<T> {
        if let Some(value) = self.try_cache(hash, ns, &map) {
            return Ok(value);
        }
        self.fetch_remote(hash, ns, &map)
    }

    /// The cache half of [`Self::fetch_map`]: a hit that passes `map`, or
    /// `None` (evicting a poisoned or unreadable entry on the way).
    fn try_cache<T>(&self, hash: &Hash, ns: Ns, map: &impl Fn(Vec<u8>) -> Result<T>) -> Option<T> {
        let cache_path = self.cache_path(hash, ns);
        if !cache_path.exists() {
            return None;
        }
        match self.cache_read(hash, ns) {
            Some(cached) => match map(cached) {
                Ok(value) => return Some(value),
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
        None
    }

    /// The network half of [`Self::fetch_map`].
    fn fetch_remote<T>(
        &self,
        hash: &Hash,
        ns: Ns,
        map: &impl Fn(Vec<u8>) -> Result<T>,
    ) -> Result<T> {
        match self.download_compressed(hash, ns)? {
            None => Err(Error::NotFound(*hash)),
            Some(Downloaded { compressed, .. }) => {
                let bytes = zstd::stream::decode_all(&compressed[..])?;
                // Map (which verifies) BEFORE caching: a corrupt GCS object
                // must not be persisted to the local cache.
                let value = map(bytes)?;
                let _ = self.cache_write_compressed(hash, ns, &compressed);
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

    fn remote_exists(&self, hash: &Hash, ns: Ns) -> bool {
        let name = self.object_name(hash, ns);
        let url = self.metadata_url(&name);
        // HEAD against the metadata endpoint is the cheapest existence
        // check the JSON API offers.
        let resp = self.with_auth(self.client.head(&url)).send();
        matches!(resp, Ok(r) if r.status().is_success())
    }
}

impl GcsObjectStore {
    /// Put `compressed` at `hash`, never replacing a *verified* object.
    ///
    /// The upload is conditional on the name being free. On HTTP 412 the
    /// occupant is downloaded and judged by `is_same` (it must decode as
    /// the content being written, exactly as the matching read would). If
    /// it is the same, this is a dedup hit. If not (a pre-fix squatter or
    /// corruption) it is replaced, conditional on the generation just
    /// observed; losing that race is an error, not success. The local
    /// cache is written only once the bucket is known to hold the verified
    /// object, so a failed or refused put never leaves a cache entry.
    ///
    /// Network cost: 1 request for a new object; 2 (upload, download) for a
    /// dedup hit; 3 when a bad occupant is replaced (4 if the download
    /// carried no generation header).
    fn store_compressed(
        &self,
        hash: &Hash,
        ns: Ns,
        compressed: &[u8],
        is_same: impl FnOnce(Vec<u8>) -> bool,
    ) -> Result<()> {
        if self.upload_compressed(hash, ns, compressed, "0")? == Upload::PreconditionFailed {
            let existing = self.download_compressed(hash, ns)?.ok_or_else(|| {
                Error::Other(anyhow::anyhow!("object {hash} vanished during put"))
            })?;
            let same = zstd::stream::decode_all(&existing.compressed[..])
                .ok()
                .is_some_and(is_same);
            if !same {
                let generation = match existing.generation {
                    Some(g) => g,
                    None => self.remote_generation(hash, ns)?,
                };
                tracing::warn!(
                    target: "agentic-core::gcs_store",
                    hash = %hash,
                    "replacing unverifiable object at content address",
                );
                if self.upload_compressed(hash, ns, compressed, &generation)?
                    == Upload::PreconditionFailed
                {
                    return Err(Error::Other(anyhow::anyhow!(
                        "object {hash} changed while replacing an unverifiable occupant"
                    )));
                }
            }
        }
        if let Err(e) = self.cache_write_compressed(hash, ns, compressed) {
            tracing::warn!(
                target: "agentic-core::gcs_store",
                hash = %hash,
                error = %e,
                "failed to write local cache entry after GCS upload",
            );
        }
        Ok(())
    }
}

impl ObjectStore for GcsObjectStore {
    fn put_with_policy(&self, object: &Object, policy: ScanPolicy) -> Result<Hash> {
        // Scanner pre-hook (ADR-0013). Reject blobs containing secrets
        // BEFORE any compression / network I/O — by the time bytes
        // would hit GCS the secret has already left the daemon's
        // address space. Trees and Commits contain hashes + metadata,
        // not user data, so they are skipped.
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
        let compressed = zstd::stream::encode_all(&bytes[..], 3)?;
        self.store_compressed(&hash, Ns::Typed, &compressed, |stored| {
            decode_typed(&hash, stored).is_ok_and(|existing| existing == *object)
        })?;
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
        self.store_compressed(&hash, Ns::Raw, &compressed, |stored| {
            Hash::of(&stored) == hash
        })?;
        Ok(hash)
    }

    fn get(&self, hash: &Hash) -> Result<Object> {
        // Typed objects are addressed by `object.hash()`. Parse once inside
        // the map closure and verify that hash — no second deserialize.
        self.fetch_map(hash, Ns::Typed, |bytes| decode_typed(hash, bytes))
    }

    fn get_raw(&self, hash: &Hash) -> Result<Vec<u8>> {
        // Raw objects are addressed by `Hash::of(bytes)` (the `put_raw`
        // contract) — verify against that, then return the bytes
        // unchanged (zero-copy: `bytes` moves out). Audit finding #3.
        let verify_raw = |bytes: Vec<u8>| {
            check_integrity(hash, Hash::of(&bytes))?;
            Ok(bytes)
        };
        // Legacy layout: raw objects used to share the typed name (and may
        // hold a blob envelope of the same bytes).
        let verify_legacy = |bytes: Vec<u8>| decode_legacy_raw(hash, bytes);
        // Hash-verified cache entries first, under both names, so a cached
        // snapshot restores without touching the network.
        if let Some(bytes) = self.try_cache(hash, Ns::Raw, &verify_raw) {
            return Ok(bytes);
        }
        if let Some(bytes) = self.try_cache(hash, Ns::Typed, &verify_legacy) {
            return Ok(bytes);
        }
        match self.fetch_remote(hash, Ns::Raw, &verify_raw) {
            Err(Error::NotFound(_)) => self.fetch_remote(hash, Ns::Typed, &verify_legacy),
            other => other,
        }
    }

    fn has(&self, hash: &Hash) -> bool {
        [Ns::Raw, Ns::Typed]
            .into_iter()
            .any(|ns| self.cache_path(hash, ns).exists() || self.remote_exists(hash, ns))
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
        let name = store.object_name(&h, Ns::Typed);
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
        let name = store.object_name(&h, Ns::Typed);
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
        assert!(store.cache_read(&h, Ns::Typed).is_none());
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
        store
            .cache_write_compressed(&h, Ns::Typed, &compressed)
            .unwrap();
        let got = store.cache_read(&h, Ns::Typed).unwrap();
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
        store
            .cache_write_compressed(&hash, Ns::Raw, &poison)
            .unwrap();
        assert!(store.cache_path(&hash, Ns::Raw).exists());

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
            !store.cache_path(&hash, Ns::Raw).exists(),
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
        let cache_path = store.cache_path(&hash, Ns::Raw);
        std::fs::create_dir_all(&cache_path).unwrap();
        assert!(cache_path.is_dir());

        let _ = store.get_raw(&hash);
        assert!(
            !cache_path.exists(),
            "a cache entry corrupted into a directory must still be evicted"
        );
    }

    // ---------- mock GCS server -------------------------------------------

    #[derive(Clone, Debug)]
    struct Req {
        method: String,
        target: String,
    }

    struct Resp {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    }

    impl Resp {
        fn status(status: u16) -> Self {
            Resp {
                status,
                headers: Vec::new(),
                body: Vec::new(),
            }
        }
    }

    /// Minimal HTTP/1.1 server (one request per connection) driven by a
    /// handler closure; records every request it sees.
    struct Mock {
        addr: std::net::SocketAddr,
        log: Arc<std::sync::Mutex<Vec<Req>>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Mock {
        fn start(handler: impl Fn(&Req) -> Resp + Send + 'static) -> Mock {
            use std::io::{BufRead, BufReader, Read, Write};
            use std::sync::atomic::Ordering;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let log: Arc<std::sync::Mutex<Vec<Req>>> = Arc::default();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (log2, stop2) = (log.clone(), stop.clone());
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    if stop2.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(mut sock) = conn else { continue };
                    let mut reader = BufReader::new(sock.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        continue;
                    }
                    let mut parts = line.split_whitespace();
                    let req = Req {
                        method: parts.next().unwrap_or_default().to_string(),
                        target: parts.next().unwrap_or_default().to_string(),
                    };
                    let mut content_length = 0usize;
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).unwrap();
                        if h == "\r\n" {
                            break;
                        }
                        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                            content_length = v.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0u8; content_length];
                    reader.read_exact(&mut body).unwrap();
                    log2.lock().unwrap().push(req.clone());
                    let resp = handler(&req);
                    let mut out = format!(
                        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                        resp.status,
                        resp.body.len()
                    );
                    for (k, v) in &resp.headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    let _ = sock.write_all(out.as_bytes());
                    let _ = sock.write_all(&resp.body);
                }
            });
            Mock { addr, log, stop }
        }

        fn store(&self, cache: &Path) -> GcsObjectStore {
            GcsObjectStore::new("b", "p", cache, Some(format!("http://{}", self.addr)), None)
                .unwrap()
        }

        fn requests(&self) -> Vec<Req> {
            self.log.lock().unwrap().clone()
        }
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = std::net::TcpStream::connect(self.addr);
        }
    }

    fn zst(bytes: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(bytes, 3).unwrap()
    }

    /// A bucket whose first upload hits an occupied name (412), serving
    /// `occupant` (generation 77). `replace_status` answers the follow-up
    /// upload that carries `ifGenerationMatch=77`.
    fn occupied_bucket(occupant: Vec<u8>, replace_status: u16) -> Mock {
        Mock::start(move |req| {
            if req.method == "POST" && req.target.contains("ifGenerationMatch=0") {
                Resp::status(412)
            } else if req.method == "POST" && req.target.contains("ifGenerationMatch=77") {
                Resp::status(replace_status)
            } else if req.method == "GET" && req.target.contains("alt=media") {
                Resp {
                    status: 200,
                    headers: vec![("x-goog-generation", "77".into())],
                    body: occupant.clone(),
                }
            } else {
                Resp::status(500)
            }
        })
    }

    fn is_post(r: &Req, needle: &str) -> bool {
        r.method == "POST" && r.target.contains(needle)
    }

    // Uploads are conditional on the name being free (fake-gcs-server
    // ignores the precondition, so it is checked here), and a 412 over the
    // *same* object is a dedup hit: verified by download, nothing replaced.
    #[test]
    fn upload_is_conditional_and_412_over_same_object_is_dedup() {
        let dir = tempdir().unwrap();
        let payload = b"already there";
        let mock = occupied_bucket(zst(payload), 500);
        let store = mock.store(dir.path());
        let h = store.put_raw(ObjectKind::Segment, payload).unwrap();

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 2, "dedup hit costs upload + download: {reqs:?}");
        assert!(is_post(&reqs[0], "ifGenerationMatch=0"), "{reqs:?}");
        assert!(reqs[0].target.contains("raw%2F"), "{reqs:?}");
        assert!(reqs[0].target.contains(".raw.zst"), "{reqs:?}");
        assert!(store.cache_path(&h, Ns::Raw).exists());
    }

    // A 412 over a *different* occupant (pre-fix squatter / corruption) is
    // replaced, conditional on the generation observed, then cached.
    #[test]
    fn put_over_different_raw_occupant_replaces_it() {
        let dir = tempdir().unwrap();
        let mock = occupied_bucket(zst(b"squatter"), 200);
        let store = mock.store(dir.path());
        let h = store.put_raw(ObjectKind::Segment, b"honest").unwrap();

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 3, "{reqs:?}");
        assert!(is_post(&reqs[2], "ifGenerationMatch=77"), "{reqs:?}");
        assert!(store.cache_path(&h, Ns::Raw).exists());
    }

    #[test]
    fn put_over_different_typed_occupant_replaces_it() {
        let dir = tempdir().unwrap();
        let mock = occupied_bucket(zst(b"squatter"), 200);
        let store = mock.store(dir.path());
        let obj = Object::Blob(crate::object::Blob::new(b"prompt".to_vec()));
        let h = store.put(&obj).unwrap();

        let reqs = mock.requests();
        assert!(is_post(&reqs[2], "ifGenerationMatch=77"), "{reqs:?}");
        assert!(store.cache_path(&h, Ns::Typed).exists());
    }

    // Losing the generation-conditional replace is an error, never success,
    // and leaves no cache entry.
    #[test]
    fn second_412_while_replacing_is_an_error() {
        let dir = tempdir().unwrap();
        let mock = occupied_bucket(zst(b"squatter"), 412);
        let store = mock.store(dir.path());
        let h = Hash::of(b"honest");
        assert!(store.put_raw(ObjectKind::Segment, b"honest").is_err());
        assert!(!store.cache_path(&h, Ns::Raw).exists());
    }

    // A failed upload must not leave a cache entry behind.
    #[test]
    fn failed_upload_leaves_no_cache_entry() {
        let dir = tempdir().unwrap();
        let mock = Mock::start(|_| Resp::status(500));
        let store = mock.store(dir.path());
        let h = Hash::of(b"will not upload");
        assert!(store
            .put_raw(ObjectKind::Segment, b"will not upload")
            .is_err());
        assert!(!store.cache_path(&h, Ns::Raw).exists());
    }

    // A hash-verified cached legacy object restores with no network at all.
    #[test]
    fn get_raw_of_cached_legacy_object_makes_no_request() {
        let dir = tempdir().unwrap();
        let mock = Mock::start(|_| Resp::status(500));
        let store = mock.store(dir.path());
        let bytes = b"legacy segment".to_vec();
        let h = Hash::of(&bytes);
        store
            .cache_write_compressed(&h, Ns::Typed, &zst(&bytes))
            .unwrap();

        assert_eq!(store.get_raw(&h).unwrap(), bytes);
        assert!(mock.requests().is_empty(), "{:?}", mock.requests());
    }

    // A legacy name holding a blob envelope of the same bytes is a valid raw
    // object, and its (valid typed) cache entry must not be evicted.
    #[test]
    fn get_raw_tolerates_cached_blob_envelope_without_evicting() {
        let dir = tempdir().unwrap();
        let mock = Mock::start(|_| Resp::status(500));
        let store = mock.store(dir.path());
        let bytes = b"legacy segment".to_vec();
        let h = Hash::of(&bytes);
        let envelope =
            serde_json::to_vec(&Object::Blob(crate::object::Blob::new(bytes.clone()))).unwrap();
        store
            .cache_write_compressed(&h, Ns::Typed, &zst(&envelope))
            .unwrap();

        assert_eq!(store.get_raw(&h).unwrap(), bytes);
        assert!(store.cache_path(&h, Ns::Typed).exists());
        assert!(mock.requests().is_empty());
    }

    // `<p>/raw` as a prefix must not collide with raw objects of prefix `<p>`.
    #[test]
    fn raw_names_cannot_collide_with_typed_names_of_a_raw_prefix() {
        let dir = tempdir().unwrap();
        let h = Hash::of(b"x");
        let outer = GcsObjectStore::new("b", "p", dir.path(), None, None).unwrap();
        let inner = GcsObjectStore::new("b", "p/raw", dir.path(), None, None).unwrap();
        assert_ne!(
            outer.object_name(&h, Ns::Raw),
            inner.object_name(&h, Ns::Typed)
        );
    }
}
