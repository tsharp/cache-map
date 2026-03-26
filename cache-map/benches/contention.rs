use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use cache_map::{CacheConfiguration, CacheMap, DashCache, PapayaCache};

const READS_PER_THREAD: u64 = 100_000;
const ELEMENT_COUNT: u64 = 250_000;

fn build_dash(n: u64) -> Arc<dyn CacheMap<u64, u64> + Send + Sync> {
    let config = CacheConfiguration::new()
        .set_default_ttl(Duration::from_secs(3600));
    let cache = DashCache::from_config(config);
    for i in 0..n {
        let _ = cache.insert(i, i);
    }
    Arc::new(cache)
}

fn build_papaya(n: u64) -> Arc<dyn CacheMap<u64, u64> + Send + Sync> {
    let config = CacheConfiguration::new()
        .set_default_ttl(Duration::from_secs(3600));
    let cache = PapayaCache::from_config(config);
    for i in 0..n {
        let _ = cache.insert(i, i);
    }
    Arc::new(cache)
}

// ---------------------------------------------------------------------------
// Read-only benchmark: DashCache vs PapayaCache
// ---------------------------------------------------------------------------
fn read_benchmark(c: &mut Criterion) {
    let thread_counts: &[usize] = &[1, 4, 8];

    let mut group = c.benchmark_group("read_only");
    group.sample_size(20);

    for &threads in thread_counts {
        let total_reads = READS_PER_THREAD * threads as u64;
        group.throughput(Throughput::Elements(total_reads));

        // DashCache
        group.bench_with_input(
            BenchmarkId::new("DashCache", format!("{threads}t")),
            &threads,
            |b, &thread_count| {
                let cache = build_dash(ELEMENT_COUNT);
                b.iter(|| run_readers(Arc::clone(&cache), thread_count, ELEMENT_COUNT));
            },
        );

        // PapayaCache
        group.bench_with_input(
            BenchmarkId::new("PapayaCache", format!("{threads}t")),
            &threads,
            |b, &thread_count| {
                let cache = build_papaya(ELEMENT_COUNT);
                b.iter(|| run_readers(Arc::clone(&cache), thread_count, ELEMENT_COUNT));
            },
        );
    }
    group.finish();
}

fn run_readers(cache: Arc<dyn CacheMap<u64, u64> + Send + Sync>, threads: usize, key_range: u64) {
    let barrier = Arc::new(Barrier::new(threads));

    let handles: Vec<_> = (0..threads)
        .map(|t| {
            let barrier = Arc::clone(&barrier);
            let cache = Arc::clone(&cache);
            thread::spawn(move || {
                let mut rng = SmallRng::seed_from_u64(t as u64);
                barrier.wait();
                for _ in 0..READS_PER_THREAD {
                    let key = rng.random_range(0..key_range);
                    let _ = cache.get(&key);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }
}

// ---------------------------------------------------------------------------
// Mixed read/write benchmark: DashCache vs PapayaCache
// ---------------------------------------------------------------------------
fn mixed_benchmark(c: &mut Criterion) {
    let thread_counts: &[usize] = &[1, 4, 8];
    let writer_count: usize = 4;
    let writes_per_writer: u64 = 10_000;

    let mut group = c.benchmark_group("mixed_rw");
    group.sample_size(20);

    for &readers in thread_counts {
        let total_ops = READS_PER_THREAD * readers as u64
            + writes_per_writer * writer_count as u64;
        group.throughput(Throughput::Elements(total_ops));

        // DashCache
        group.bench_with_input(
            BenchmarkId::new("DashCache", format!("{readers}r_{writer_count}w")),
            &readers,
            |b, &reader_count| {
                b.iter(|| {
                    let cache = build_dash(ELEMENT_COUNT);
                    run_mixed(cache, reader_count, writer_count, writes_per_writer, ELEMENT_COUNT);
                });
            },
        );

        // PapayaCache
        group.bench_with_input(
            BenchmarkId::new("PapayaCache", format!("{readers}r_{writer_count}w")),
            &readers,
            |b, &reader_count| {
                b.iter(|| {
                    let cache = build_papaya(ELEMENT_COUNT);
                    run_mixed(cache, reader_count, writer_count, writes_per_writer, ELEMENT_COUNT);
                });
            },
        );
    }
    group.finish();
}

fn run_mixed(
    cache: Arc<dyn CacheMap<u64, u64> + Send + Sync>,
    readers: usize,
    writers: usize,
    writes_per_writer: u64,
    initial_keys: u64,
) {
    let total_threads = readers + writers;
    let barrier = Arc::new(Barrier::new(total_threads));
    let key_ceiling = Arc::new(AtomicU64::new(initial_keys));
    let stop = Arc::new(AtomicBool::new(false));

    let mut handles = Vec::with_capacity(total_threads);

    // Writers
    for t in 0..writers {
        let barrier = Arc::clone(&barrier);
        let key_ceiling = Arc::clone(&key_ceiling);
        let cache = Arc::clone(&cache);
        handles.push(thread::spawn(move || {
            let mut rng = SmallRng::seed_from_u64(0xCAFE + t as u64);
            barrier.wait();
            for _ in 0..writes_per_writer {
                let key = key_ceiling.fetch_add(1, Ordering::AcqRel);
                let ttl = Duration::from_secs(rng.random_range(5..20));
                let _ = cache.insert_with_ttl(key, key, ttl);
            }
        }));
    }

    // Readers
    for t in 0..readers {
        let barrier = Arc::clone(&barrier);
        let key_ceiling = Arc::clone(&key_ceiling);
        let stop = Arc::clone(&stop);
        let cache = Arc::clone(&cache);
        handles.push(thread::spawn(move || {
            let mut rng = SmallRng::seed_from_u64(t as u64);
            barrier.wait();
            for _ in 0..READS_PER_THREAD {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let ceiling = key_ceiling.load(Ordering::Acquire);
                let key = rng.random_range(0..ceiling);
                let _ = cache.get(&key);
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }
}

criterion_group!(benches, read_benchmark, mixed_benchmark);
criterion_main!(benches);
