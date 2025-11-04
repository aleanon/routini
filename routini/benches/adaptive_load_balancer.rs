use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use routini::adaptive_loadbalancer::AdaptiveLoadBalancer;
use routini::adaptive_loadbalancer::options::AdaptiveLbOpt;
use routini::load_balancing::discovery::Static;
use routini::load_balancing::strategy::Adaptive;
use routini::load_balancing::{Backend, Backends};
use std::collections::BTreeSet;
use std::hint::black_box;

fn create_backends(count: usize) -> BTreeSet<Backend> {
    let mut backends = BTreeSet::new();

    for i in 0..count {
        // Create valid IP addresses using different octets
        let addr = if i < 255 {
            format!("127.0.0.{}:8080", i + 1)
        } else if i < 510 {
            format!("127.0.1.{}:8080", (i - 255) + 1)
        } else {
            format!("127.0.2.{}:8080", (i - 510) + 1)
        };

        let backend = Backend::new(&addr).expect("Failed to create backend");
        backends.insert(backend);
    }
    backends
}

fn benchmark_adaptive_lb_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("AdaptiveLoadBalancer_Select");

    let backend_counts = vec![1, 5, 10, 25, 50, 100, 250];

    for &count in &backend_counts {
        group.throughput(Throughput::Elements(1));

        let backends = create_backends(count);
        let lb = AdaptiveLoadBalancer::from_backends(Backends::new(Static::new(backends)), None);

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_backends", count)),
            &count,
            |b, _| {
                b.iter(|| {
                    let backend = lb.select(black_box(b"test_key"));
                    black_box(backend);
                });
            },
        );
    }

    group.finish();
}

fn benchmark_adaptive_lb_strategies(c: &mut Criterion) {
    let mut group = c.benchmark_group("AdaptiveLoadBalancer_Strategies");

    let backend_count = 100;
    let backends = create_backends(backend_count);

    let strategies = vec![
        ("RoundRobin", Adaptive::RoundRobin),
        ("Random", Adaptive::Random),
        ("FNVHash", Adaptive::FNVHash),
        ("Consistent", Adaptive::Consistent),
        ("FewestConnections", Adaptive::FewestConnections),
        ("FastestServer", Adaptive::FastestServer),
    ];

    for (name, strategy) in strategies {
        let opts = AdaptiveLbOpt {
            starting_strategy: strategy,
            health_check_interval: None, // Disable health checks for benchmarking
            ..Default::default()
        };

        let lb = AdaptiveLoadBalancer::from_backends(
            Backends::new(Static::new(backends.clone())),
            Some(opts),
        );

        group.bench_with_input(BenchmarkId::from_parameter(name), name, |b, _| {
            b.iter(|| {
                let backend = lb.select(black_box(b"test_key"));
                black_box(backend);
            });
        });
    }

    group.finish();
}

fn benchmark_adaptive_lb_different_keys(c: &mut Criterion) {
    let mut group = c.benchmark_group("AdaptiveLoadBalancer_KeyVariety");

    let backend_count = 100;
    let backends = create_backends(backend_count);

    let lb = AdaptiveLoadBalancer::from_backends(Backends::new(Static::new(backends)), None);

    group.bench_function("same_key", |b| {
        b.iter(|| {
            let backend = lb.select(black_box(b"constant_key"));
            black_box(backend);
        });
    });

    group.bench_function("different_keys", |b| {
        let mut counter = 0u64;
        b.iter(|| {
            counter += 1;
            let key = format!("key_{}", counter);
            let backend = lb.select(black_box(key.as_bytes()));
            black_box(backend);
        });
    });

    group.finish();
}

fn benchmark_adaptive_lb_concurrent_selects(c: &mut Criterion) {
    let mut group = c.benchmark_group("AdaptiveLoadBalancer_Concurrent");

    let backend_counts = vec![10, 50, 100];

    for &count in &backend_counts {
        let backends = create_backends(count);
        let lb = AdaptiveLoadBalancer::from_backends(Backends::new(Static::new(backends)), None);

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_backends", count)),
            &count,
            |b, _| {
                b.iter(|| {
                    // Simulate multiple concurrent selections
                    let b1 = lb.select(black_box(b"key1"));
                    let b2 = lb.select(black_box(b"key2"));
                    let b3 = lb.select(black_box(b"key3"));
                    let b4 = lb.select(black_box(b"key4"));
                    let b5 = lb.select(black_box(b"key5"));

                    black_box((b1, b2, b3, b4, b5));
                });
            },
        );
    }

    group.finish();
}

fn benchmark_adaptive_lb_with_max_iterations(c: &mut Criterion) {
    let mut group = c.benchmark_group("AdaptiveLoadBalancer_MaxIterations");

    let backend_count = 100;
    let backends = create_backends(backend_count);

    let max_iterations = vec![1, 3, 5, 10, 256];

    for &max_iter in &max_iterations {
        let opts = AdaptiveLbOpt {
            max_iterations: max_iter,
            health_check_interval: None,
            ..Default::default()
        };

        let lb = AdaptiveLoadBalancer::from_backends(
            Backends::new(Static::new(backends.clone())),
            Some(opts),
        );

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("max_iter_{}", max_iter)),
            &max_iter,
            |b, _| {
                b.iter(|| {
                    let backend = lb.select(black_box(b"test_key"));
                    black_box(backend);
                });
            },
        );
    }

    group.finish();
}

fn benchmark_adaptive_lb_update_strategy(c: &mut Criterion) {
    let mut group = c.benchmark_group("AdaptiveLoadBalancer_UpdateStrategy");

    let backend_count = 100;
    let backends = create_backends(backend_count);

    let lb = AdaptiveLoadBalancer::from_backends(Backends::new(Static::new(backends)), None);

    // Create a Tokio runtime for async benchmarking
    let runtime = tokio::runtime::Runtime::new().unwrap();

    group.bench_function("update_to_roundrobin", |b| {
        b.iter(|| {
            runtime.block_on(async {
                lb.update_strategy(black_box(Adaptive::RoundRobin)).await;
            });
        });
    });

    group.bench_function("update_to_random", |b| {
        b.iter(|| {
            runtime.block_on(async {
                lb.update_strategy(black_box(Adaptive::Random)).await;
            });
        });
    });

    group.finish();
}

fn benchmark_adaptive_lb_backends_access(c: &mut Criterion) {
    let mut group = c.benchmark_group("AdaptiveLoadBalancer_BackendsAccess");

    let backend_counts = vec![10, 50, 100, 250, 500];

    for &count in &backend_counts {
        let backends = create_backends(count);
        let lb = AdaptiveLoadBalancer::from_backends(Backends::new(Static::new(backends)), None);

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_backends", count)),
            &count,
            |b, _| {
                b.iter(|| {
                    let backends = lb.backends();
                    black_box(backends.len());
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    benchmark_adaptive_lb_select,
    benchmark_adaptive_lb_strategies,
    benchmark_adaptive_lb_different_keys,
    benchmark_adaptive_lb_concurrent_selects,
    benchmark_adaptive_lb_with_max_iterations,
    benchmark_adaptive_lb_update_strategy,
    benchmark_adaptive_lb_backends_access,
);
criterion_main!(benches);
