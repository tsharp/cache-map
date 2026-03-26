use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use dashmap::DashMap;
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use cache_map::{CacheConfiguration, CacheMap, DashCache};

const READS: u64 = 100_000;

fn bench_reads(c: &mut Criterion) {
    let sizes: &[u64] = &[1_000, 10_000, 100_000, 1_000_000];

    let mut group = c.benchmark_group("single_thread_reads");
    group.sample_size(30);

    for &n in sizes {
        group.throughput(Throughput::Elements(READS));

        // Raw DashMap
        group.bench_with_input(
            BenchmarkId::new("DashMap", n),
            &n,
            |b, &n| {
                let map = DashMap::with_capacity(n as usize);
                for i in 0..n {
                    map.insert(i, i);
                }
                let mut rng = SmallRng::seed_from_u64(0xBEEF);
                b.iter(|| {
                    for _ in 0..READS {
                        let key = rng.random_range(0..n);
                        std::hint::black_box(map.get(&key));
                    }
                });
            },
        );

        // DashCache (TTL wrapper)
        group.bench_with_input(
            BenchmarkId::new("DashCache", n),
            &n,
            |b, &n| {
                let config = CacheConfiguration::new()
                    .set_default_ttl(Duration::from_secs(3600));
                let cache = DashCache::from_config(config);
                for i in 0..n {
                    let _ = cache.insert(i, i);
                }
                let mut rng = SmallRng::seed_from_u64(0xBEEF);
                b.iter(|| {
                    for _ in 0..READS {
                        let key = rng.random_range(0..n);
                        std::hint::black_box(cache.get(&key));
                    }
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_reads);
criterion_main!(benches);
