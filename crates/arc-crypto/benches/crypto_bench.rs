use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use arc_crypto::*;

fn bench_blake3_commit(c: &mut Criterion) {
    let mut group = c.benchmark_group("blake3_commit");
    for size in [128, 256, 512, 1024] {
        let data = vec![0u8; size];
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, _| {
            b.iter(|| blake3_commit::commit_transaction(0x01, &data));
        });
    }
    group.finish();
}

fn bench_pedersen_commit(c: &mut Criterion) {
    c.bench_function("pedersen_commit", |b| {
        b.iter(|| pedersen::commit_value(42));
    });
}

fn bench_merkle_tree(c: &mut Criterion) {
    let mut group = c.benchmark_group("merkle_tree");
    for n in [1000, 10_000, 100_000] {
        let leaves: Vec<Hash256> = (0..n as u32)
            .map(|i| hash_bytes(&i.to_le_bytes()))
            .collect();
        group.bench_with_input(BenchmarkId::from_parameter(n), &leaves, |b, leaves| {
            b.iter(|| MerkleTree::from_leaves(leaves.clone()));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_blake3_commit, bench_pedersen_commit, bench_merkle_tree);
criterion_main!(benches);
