use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use routini::load_balancing::Backend;
use routini::load_balancing::strategy::{
    Adaptive, BackendIter, BackendSelection, Consistent, FNVHash, FewestConnections, Random,
    RoundRobin, Strategy,
};
use std::collections::BTreeSet;
use std::hint::black_box;
use std::sync::Arc;

fn create_backends(strategy: &impl Strategy, count: usize) -> BTreeSet<Backend> {
    (0..count)
        .map(|i| {
            // Create valid IP addresses using different octets
            let addr = if i < 255 {
                format!("127.0.0.{}:8080", i + 1)
            } else if i < 510 {
                format!("127.0.1.{}:8080", (i - 255) + 1)
            } else {
                format!("127.0.2.{}:8080", (i - 510) + 1)
            };
            let mut backend = Backend::new(&addr)
                .expect(&format!("Failed to create backend with addr: {}", addr));
            backend.metrics = strategy.metrics();
            backend
        })
        .collect()
}

fn benchmark_strategy<S>(
    c: &mut Criterion,
    strategy_name: &str,
    strategy: S,
    backend_counts: &[usize],
) where
    S: Strategy + Clone,
    <S::BackendSelector as BackendSelection>::Iter: BackendIter,
{
    let mut group = c.benchmark_group(strategy_name);

    for &count in backend_counts {
        group.throughput(Throughput::Elements(1));

        let backends = create_backends(&strategy, count);
        let selector = Arc::new(strategy.build_backend_selector(&backends));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_backends", count)),
            &count,
            |b, _| {
                b.iter(|| {
                    let mut iter = selector.iter(black_box(b"test_key"));
                    // Consume the iterator to measure selection performance
                    black_box(iter.next());
                });
            },
        );
    }

    group.finish();
}

fn benchmark_all_strategies(c: &mut Criterion) {
    let backend_counts = vec![1, 2, 5, 10, 25, 50, 100, 250, 500];

    // Benchmark RoundRobin
    benchmark_strategy(c, "RoundRobin", RoundRobin, &backend_counts);

    // Benchmark Random
    benchmark_strategy(c, "Random", Random, &backend_counts);

    // Benchmark FNVHash
    benchmark_strategy(c, "FNVHash", FNVHash, &backend_counts);

    // Benchmark Consistent
    benchmark_strategy(c, "Consistent", Consistent, &backend_counts);

    // Benchmark FewestConnections
    benchmark_strategy(c, "FewestConnections", FewestConnections, &backend_counts);

    // Benchmark Adaptive (using FNVHash as default)
    benchmark_strategy(c, "Adaptive_FNVHash", Adaptive::FNVHash, &backend_counts);

    // Benchmark Adaptive with RoundRobin
    benchmark_strategy(
        c,
        "Adaptive_RoundRobin",
        Adaptive::RoundRobin,
        &backend_counts,
    );
}

fn benchmark_full_iteration(c: &mut Criterion) {
    let mut group = c.benchmark_group("FullIteration");

    let backend_counts = vec![10, 50, 100, 250, 500];

    for &count in &backend_counts {
        group.throughput(Throughput::Elements(count as u64));

        let backends = create_backends(&RoundRobin, count);
        let selector = Arc::new(RoundRobin.build_backend_selector(&backends));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_backends", count)),
            &count,
            |b, _| {
                b.iter(|| {
                    let mut iter = selector.iter(black_box(b"test_key"));
                    let mut backend_count = 0;
                    while let Some(backend) = iter.next() {
                        black_box(backend);
                        backend_count += 1;
                    }
                    black_box(backend_count);
                });
            },
        );
    }

    group.finish();
}

fn benchmark_concurrent_access(c: &mut Criterion) {
    let mut group = c.benchmark_group("ConcurrentAccess");

    let backend_counts = vec![10, 50, 100];

    for &count in &backend_counts {
        let backends = create_backends(&RoundRobin, count);
        let selector = Arc::new(RoundRobin.build_backend_selector(&backends));

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_backends", count)),
            &count,
            |b, _| {
                b.iter(|| {
                    // Simulate multiple concurrent accesses
                    let s1 = selector.clone();
                    let s2 = selector.clone();
                    let s3 = selector.clone();

                    let mut iter1 = s1.iter(black_box(b"key1"));
                    let mut iter2 = s2.iter(black_box(b"key2"));
                    let mut iter3 = s3.iter(black_box(b"key3"));

                    black_box(iter1.next());
                    black_box(iter2.next());
                    black_box(iter3.next());
                });
            },
        );
    }

    group.finish();
}

fn benchmark_key_distribution(c: &mut Criterion) {
    let mut group = c.benchmark_group("KeyDistribution");

    let backend_count = 100;
    let backends = create_backends(&RoundRobin, backend_count);
    let selector = Arc::new(FNVHash.build_backend_selector(&backends));

    group.bench_function("different_keys", |b| {
        let mut counter = 0u64;
        b.iter(|| {
            counter += 1;
            let key = format!("key_{}", counter);
            let mut iter = selector.iter(black_box(key.as_bytes()));
            black_box(iter.next());
        });
    });

    group.bench_function("same_key", |b| {
        b.iter(|| {
            let mut iter = selector.iter(black_box(b"constant_key"));
            black_box(iter.next());
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    benchmark_all_strategies,
    benchmark_full_iteration,
    benchmark_concurrent_access,
    benchmark_key_distribution
);
criterion_main!(benches);
