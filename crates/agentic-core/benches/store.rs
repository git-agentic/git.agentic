//! Criterion benchmarks for the content-addressed object store.
//!
//! Performance targets covered here (from docs/architecture/snapshot-model.md §9):
//!   commit            < 2 s   — measured by bench_commit/prompts_only
//!   write overhead    < 5 ms p99 per object — measured by bench_blob_put
//!
//! Targets NOT measured here (require Postgres; see tests/integration/):
//!   rollback   < 5 s  (end-to-end memory restore)
//!   diff       < 1 s  (manifest diff across snapshots)
//!
//! The benchmarks here are CI-safe (no Postgres, no network).

use std::collections::BTreeMap;

use agentic_core::commit::{stage_and_commit, CommitInputs};
use agentic_core::refs::Refs;
use agentic_core::{Blob, FsObjectStore, Hash, Object, ObjectKind, ObjectStore, Tree, TypedRef};
use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn tmp_store() -> (TempDir, FsObjectStore, Refs) {
    let dir = TempDir::new().unwrap();
    let agentic_dir = dir.path().join(".agentic");
    let objects_dir = agentic_dir.join("objects");
    std::fs::create_dir_all(&objects_dir).unwrap();
    // Mirror the real layout: store at .agentic/objects/, refs at .agentic/.
    let store = FsObjectStore::open(&objects_dir).unwrap();
    let refs = Refs::open(&agentic_dir).unwrap();
    (dir, store, refs)
}

/// Deterministic pseudo-random bytes — reproducible across runs.
fn det_bytes(n: usize) -> Vec<u8> {
    // Use u64 arithmetic so the literal stays in range on 32-bit targets.
    (0..n)
        .map(|i| ((i as u64).wrapping_mul(6_364_136_223_846_793_005_u64) >> 56) as u8)
        .collect()
}

// ── hash ──────────────────────────────────────────────────────────────────────

fn bench_hash(c: &mut Criterion) {
    let mut g = c.benchmark_group("hash");
    for size in [1_024usize, 64 * 1_024, 1_024 * 1_024] {
        let data = det_bytes(size);
        g.throughput(Throughput::Bytes(size as u64));
        g.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, d| {
            b.iter(|| Hash::of(black_box(d)));
        });
    }
    g.finish();
}

// ── blob put / get ────────────────────────────────────────────────────────────

fn bench_blob_put(c: &mut Criterion) {
    let mut g = c.benchmark_group("blob_put");
    for size in [1_024usize, 64 * 1_024, 512 * 1_024] {
        g.throughput(Throughput::Bytes(size as u64));
        g.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            b.iter_batched(
                || {
                    let (dir, store, refs) = tmp_store();
                    let blob = Blob::new(det_bytes(sz));
                    (dir, store, refs, blob)
                },
                |(_dir, store, _refs, blob)| store.put(&Object::Blob(blob)).unwrap(),
                BatchSize::SmallInput,
            );
        });
    }
    g.finish();
}

fn bench_blob_roundtrip(c: &mut Criterion) {
    let mut g = c.benchmark_group("blob_roundtrip");
    for size in [1_024usize, 64 * 1_024, 512 * 1_024] {
        g.throughput(Throughput::Bytes(size as u64));
        g.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &sz| {
            let (_dir, store, _refs) = tmp_store();
            let blob = Blob::new(det_bytes(sz));
            let hash = store.put(&Object::Blob(blob)).unwrap();
            b.iter(|| black_box(store.get(black_box(&hash)).unwrap()));
        });
    }
    g.finish();
}

// ── tree hash ─────────────────────────────────────────────────────────────────

fn bench_tree_hash(c: &mut Criterion) {
    let mut g = c.benchmark_group("tree_hash");
    for n in [10usize, 100, 1_000] {
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let mut tree = Tree::new();
                    for i in 0..n {
                        let blob = Blob::new(det_bytes(64));
                        tree.insert(
                            format!("file_{i:04}.txt"),
                            TypedRef {
                                kind: ObjectKind::Blob,
                                hash: blob.hash(),
                            },
                        );
                    }
                    tree
                },
                |tree| black_box(tree.hash()),
                BatchSize::SmallInput,
            );
        });
    }
    g.finish();
}

// ── tree put (Tree object only — referenced blobs are NOT written) ────────────

fn bench_tree_put(c: &mut Criterion) {
    // Measures only the cost of serialising + storing the Tree manifest.
    // The blobs it references are never written to the store here; their
    // hashes are synthesised in-memory. Run bench_blob_put for blob overhead.
    let mut g = c.benchmark_group("tree_put");
    for n in [10usize, 100, 500] {
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let (dir, store, refs) = tmp_store();
                    let mut tree = Tree::new();
                    for i in 0..n {
                        let blob = Blob::new(det_bytes(64));
                        tree.insert(
                            format!("file_{i:04}.txt"),
                            TypedRef {
                                kind: ObjectKind::Blob,
                                hash: blob.hash(),
                            },
                        );
                    }
                    (dir, store, refs, tree)
                },
                |(_dir, store, _refs, tree)| store.put(&Object::Tree(tree)).unwrap(),
                BatchSize::SmallInput,
            );
        });
    }
    g.finish();
}

// ── commit (prompts-only, no memory) ─────────────────────────────────────────

fn bench_commit(c: &mut Criterion) {
    let mut g = c.benchmark_group("commit");
    g.bench_function("prompts_only", |b| {
        b.iter_batched(
            || {
                let (dir, store, refs) = tmp_store();
                let mut prompts = BTreeMap::new();
                prompts.insert("system.txt".into(), b"You are helpful.".to_vec());
                let inputs = CommitInputs {
                    author: "bench".into(),
                    message: "bench commit".into(),
                    parent: None,
                    code_sha: Some("abc123".into()),
                    prompts,
                    tools: BTreeMap::new(),
                    model: Some("anthropic:claude-opus:2026-05-01".into()),
                    memory_snapshot: None,
                    schema_version: None,
                    intent: None,
                    plan: None,
                    transcript: None,
                    evals: None,
                    cost_cents: 0,
                    peer_uid: None,
                    exempt_entropy_prefixes: Vec::new(),
                };
                (dir, store, refs, inputs)
            },
            |(_dir, store, refs, inputs)| stage_and_commit(&store, &refs, "main", inputs).unwrap(),
            BatchSize::SmallInput,
        );
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_hash,
    bench_blob_put,
    bench_blob_roundtrip,
    bench_tree_hash,
    bench_tree_put,
    bench_commit,
);
criterion_main!(benches);
