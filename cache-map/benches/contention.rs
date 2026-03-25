use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use cache_map::cache::{CacheConfiguration, CacheMap};
use cache_map::dash_cache::DashCache;

const READS_PER_THREAD: u64 = 100_000;

/// Pre-populate a DashCache with `element_count` entries keyed 0..element_count.
fn build_cache(element_count: u64) -> Arc<DashCache<u64, u64>> {
    let config = CacheConfiguration::new()
        .set_default_ttl(Duration::from_secs(3600)); // long TTL so nothing expires during bench
    let cache: DashCache<u64, u64> = DashCache::from_config(config);
    for i in 0..element_count {
        let _ = cache.insert(i, i);
    }
    Arc::new(cache)
}

fn contention_benchmark(c: &mut Criterion) {
    let element_counts: &[u64] = &[1_000, 10_000, 100_000, 1_000_000];
    let thread_counts: &[usize] = &[1, 4, 8];

    let mut group = c.benchmark_group("dash_cache_reads");
    group.sample_size(20); // fewer samples for the heavier configs

    for &elements in element_counts {
        for &threads in thread_counts {
            let total_reads = READS_PER_THREAD * threads as u64;
            group.throughput(Throughput::Elements(total_reads));

            group.bench_with_input(
                BenchmarkId::new(
                    format!("{threads}t"),
                    format!("{elements}_elems"),
                ),
                &(elements, threads),
                |b, &(element_count, thread_count)| {
                    let cache = build_cache(element_count);

                    b.iter(|| {
                        let barrier = Arc::new(Barrier::new(thread_count));
                        let handles: Vec<_> = (0..thread_count)
                            .map(|t| {
                                let cache = Arc::clone(&cache);
                                let barrier = Arc::clone(&barrier);
                                thread::spawn(move || {
                                    let mut rng = SmallRng::seed_from_u64(t as u64);
                                    barrier.wait(); // all threads start together
                                    for _ in 0..READS_PER_THREAD {
                                        let key = rng.random_range(0..element_count);
                                        let _ = cache.get(&key);
                                    }
                                })
                            })
                            .collect();

                        for h in handles {
                            h.join().unwrap();
                        }
                    });
                },
            );
        }
    }
    group.finish();
}

criterion_group!(benches, contention_benchmark);
criterion_main!(benches);
