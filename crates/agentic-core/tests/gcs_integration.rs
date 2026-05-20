//! End-to-end tests for `GcsObjectStore` against `fake-gcs-server`.
//!
//! Gated by `#[ignore]` so the default `cargo test` run skips them.
//! Bring up the fixture, create the bucket, and run with:
//!
//! ```bash
//! podman compose -f tests/fixtures/fake-gcs.yml up -d
//! # fake-gcs-server does not auto-create buckets; create it via the
//! # JSON API before running the tests.
//! curl -X POST -H 'Content-Type: application/json' \
//!   -d '{"name":"agentic-test-bucket"}' \
//!   http://localhost:54323/storage/v1/b
//! GCS_ENDPOINT=http://localhost:54323 GCS_BUCKET=agentic-test-bucket \
//!   cargo test -p agentic-core --test gcs_integration -- --ignored
//! ```
//!
//! Every test scopes its objects under a unique prefix
//! (`test-<nanos>`) so concurrent runs against the same bucket don't
//! collide.

use std::time::{SystemTime, UNIX_EPOCH};

use agentic_core::{gcs_store::GcsObjectStore, Blob, Hash, Object, ObjectKind, ObjectStore};

fn endpoint() -> Option<String> {
    std::env::var("GCS_ENDPOINT").ok()
}

fn bucket() -> Option<String> {
    std::env::var("GCS_BUCKET").ok()
}

fn bearer() -> Option<String> {
    std::env::var("GCS_TOKEN").ok()
}

fn fresh_prefix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("test-{nanos}")
}

fn make_store(prefix: &str) -> Option<(GcsObjectStore, tempfile::TempDir)> {
    // Bucket is the only hard requirement. `GCS_ENDPOINT` is unset against
    // public GCS (the store defaults to the real host) and set against
    // `fake-gcs-server`. `GCS_TOKEN` carries a bearer for real-GCS runs
    // and is unset for fake-gcs (which ignores auth).
    let bucket = bucket()?;
    let cache = tempfile::tempdir().unwrap();
    let store = GcsObjectStore::new(
        bucket,
        prefix.to_string(),
        cache.path(),
        endpoint(),
        bearer(),
    )
    .unwrap();
    Some((store, cache))
}

#[test]
#[ignore]
fn put_raw_then_get_raw_roundtrip() {
    let prefix = fresh_prefix();
    let Some((store, _cache)) = make_store(&prefix) else {
        eprintln!("GCS_BUCKET not set — skipping");
        return;
    };

    let payload = b"hello gcs world";
    let h = store.put_raw(ObjectKind::Blob, payload).unwrap();
    assert_eq!(h, Hash::of(payload));

    // First read hits the local cache (warm after put).
    let got = store.get_raw(&h).unwrap();
    assert_eq!(got, payload);

    // Drop the cache and re-read — this exercises the actual GCS GET.
    drop(_cache);
    let cache2 = tempfile::tempdir().unwrap();
    let cold_store =
        GcsObjectStore::new(bucket().unwrap(), prefix, cache2.path(), endpoint(), bearer()).unwrap();
    let cold = cold_store.get_raw(&h).unwrap();
    assert_eq!(cold, payload);
    // Subsequent reads should now hit the cold-store's cache.
    let warm = cold_store.get_raw(&h).unwrap();
    assert_eq!(warm, payload);
}

#[test]
#[ignore]
fn put_object_then_get_validates_integrity() {
    let prefix = fresh_prefix();
    let Some((store, _cache)) = make_store(&prefix) else {
        eprintln!("GCS_BUCKET not set — skipping");
        return;
    };

    let blob = Blob::new(b"hello gcs".to_vec());
    let obj = Object::Blob(blob.clone());
    let h = store.put(&obj).unwrap();
    assert_eq!(h, obj.hash());

    let fetched = store.get(&h).unwrap();
    match fetched {
        Object::Blob(b) => assert_eq!(b, blob),
        _ => panic!("expected Blob"),
    }
}

#[test]
#[ignore]
fn has_returns_true_after_put_false_for_unknown() {
    let prefix = fresh_prefix();
    let Some((store, _cache)) = make_store(&prefix) else {
        eprintln!("GCS_BUCKET not set — skipping");
        return;
    };
    let h = store.put_raw(ObjectKind::Blob, b"abc").unwrap();
    assert!(store.has(&h));

    // Force a real GCS HEAD by using a fresh cache.
    let cache2 = tempfile::tempdir().unwrap();
    let cold =
        GcsObjectStore::new(bucket().unwrap(), prefix, cache2.path(), endpoint(), bearer()).unwrap();
    let nope = Hash::of(b"never-uploaded");
    assert!(!cold.has(&nope));
}

#[test]
#[ignore]
fn missing_object_returns_not_found() {
    let prefix = fresh_prefix();
    let Some((store, _cache)) = make_store(&prefix) else {
        eprintln!("GCS_BUCKET not set — skipping");
        return;
    };
    let nope = Hash::of(b"definitely-not-uploaded");
    let err = store.get_raw(&nope).unwrap_err();
    match err {
        agentic_core::Error::NotFound(h) => assert_eq!(h, nope),
        other => panic!("expected NotFound, got {other:?}"),
    }
}
