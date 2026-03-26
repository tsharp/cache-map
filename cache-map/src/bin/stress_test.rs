use core::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use mimalloc::MiMalloc;
use rand::rngs::SmallRng;
use rand::{RngExt, SeedableRng};
use cache_map::{CacheConfiguration, CacheMap};
use cache_map::DashCache;
use cache_map::PapayaCache;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const TEST_DURATION: Duration = Duration::from_secs(30);
const MAX_ELEMENTS: u64 = 250_000;
const WRITER_THREADS: usize = 4;
const READER_THREAD_CONFIGS: &[usize] = &[1, 4, 8];
const STATS_INTERVAL: Duration = Duration::from_secs(5);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(2);
// Target TTL range: 5-20s. With 250K elements and avg TTL ~12.5s, steady-state
// replacement rate is ~20K inserts/s spread across WRITER_THREADS.
const TTL_SHORT_MS: (u64, u64) = (3_000, 8_000);
const TTL_LONG_MS: (u64, u64) = (10_000, 25_000);
// Throttle writers to a target aggregate write rate so both cache
// implementations see comparable write pressure.  Each writer sleeps briefly
// every WRITE_BATCH_SIZE inserts to stay near the target.
const TARGET_WRITES_PER_SEC: u64 = 500_000;
const WRITE_BATCH_SIZE: u64 = 500;

#[derive(Debug)]
enum CacheType {
    DashCache,
    PapayaCache,
}

impl fmt::Display for CacheType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheType::DashCache => write!(f, "DashCache"),
            CacheType::PapayaCache => write!(f, "PapayaCache"),
        }
    }
}

fn run_scenario(cache_type: CacheType, reader_threads: usize, test_duration: Duration) {
    println!("\n{}", "=".repeat(60));
    println!("  Cache Type: {}", cache_type);
    println!("  Stress test: {reader_threads} readers + {WRITER_THREADS} writers, {test_duration:?}");
    println!("  Target cache size: {MAX_ELEMENTS} elements");
    println!("{}\n", "=".repeat(60));

    let config = CacheConfiguration::new()
        .set_default_ttl(Duration::from_secs(15))
        .set_max_capacity(MAX_ELEMENTS as usize);

    let cache: Arc<dyn CacheMap<u64, u64> + Send + Sync> = match cache_type {
        CacheType::DashCache => Arc::new(DashCache::from_config(config)),
        CacheType::PapayaCache => Arc::new(PapayaCache::from_config(config)),
    };

    let running = Arc::new(AtomicBool::new(true));
    let total_reads = Arc::new(AtomicU64::new(0));
    let total_hits = Arc::new(AtomicU64::new(0));
    let total_writes = Arc::new(AtomicU64::new(0));
    let total_evictions = Arc::new(AtomicU64::new(0));
    let key_ceiling = Arc::new(AtomicU64::new(0));

    // Pre-seed the cache to MAX_ELEMENTS so readers start hot
    {
        let mut rng = SmallRng::seed_from_u64(0xBEEF);
        for i in 0..MAX_ELEMENTS {
            let ttl_ms = rng.random_range(5_000..20_000u64);
            let _ = cache.insert_with_ttl(i, i, Duration::from_millis(ttl_ms));
        }
        key_ceiling.store(MAX_ELEMENTS, Ordering::Release);
    }

    let start = Instant::now();

    // --- Writer threads: continuous inserts simulating entry churn ---
    let writers: Vec<_> = (0..WRITER_THREADS)
        .map(|t| {
            let cache = Arc::clone(&cache);
            let running = Arc::clone(&running);
            let total_writes = Arc::clone(&total_writes);
            let key_ceiling = Arc::clone(&key_ceiling);

            thread::Builder::new()
                .name(format!("writer-{t}"))
                .spawn(move || {
                    let mut rng = SmallRng::seed_from_u64(0xCAFE + t as u64);
                    let mut local_writes = 0u64;
                    let per_thread_rate = TARGET_WRITES_PER_SEC / WRITER_THREADS as u64;
                    let batch_duration = Duration::from_secs_f64(
                        WRITE_BATCH_SIZE as f64 / per_thread_rate as f64
                    );
                    let mut batch_start = Instant::now();

                    while running.load(Ordering::Relaxed) {
                        let key = key_ceiling.fetch_add(1, Ordering::AcqRel);
                        let ttl_ms = if rng.random_bool(0.4) {
                            rng.random_range(TTL_SHORT_MS.0..TTL_SHORT_MS.1)
                        } else {
                            rng.random_range(TTL_LONG_MS.0..TTL_LONG_MS.1)
                        };
                        let _ = cache.insert_with_ttl(key, key, Duration::from_millis(ttl_ms));
                        local_writes += 1;

                        if local_writes % WRITE_BATCH_SIZE == 0 {
                            total_writes.fetch_add(WRITE_BATCH_SIZE, Ordering::Relaxed);

                            // Sleep to stay near the target write rate.
                            let elapsed = batch_start.elapsed();
                            if elapsed < batch_duration {
                                thread::sleep(batch_duration - elapsed);
                            }
                            batch_start = Instant::now();
                        }
                    }

                    let remainder = local_writes % WRITE_BATCH_SIZE;
                    total_writes.fetch_add(remainder, Ordering::Relaxed);
                })
                .expect("failed to spawn writer")
        })
        .collect();

    // --- Cleanup thread: periodic eviction ---
    let cleaner = {
        let cache = Arc::clone(&cache);
        let running = Arc::clone(&running);
        let total_evictions = Arc::clone(&total_evictions);

        thread::Builder::new()
            .name("cleaner".into())
            .spawn(move || {
                while running.load(Ordering::Relaxed) {
                    thread::sleep(CLEANUP_INTERVAL);
                    if !running.load(Ordering::Relaxed) {
                        break;
                    }
                    let before = cache.len();
                    cache.cleanup();
                    let after = cache.len();
                    let evicted = before.saturating_sub(after);
                    total_evictions.fetch_add(evicted as u64, Ordering::Relaxed);
                }
            })
            .expect("failed to spawn cleaner")
    };

    // --- Reader threads ---
    let barrier = Arc::new(Barrier::new(reader_threads));
    let readers: Vec<_> = (0..reader_threads)
        .map(|t| {
            let cache = Arc::clone(&cache);
            let running = Arc::clone(&running);
            let total_reads = Arc::clone(&total_reads);
            let total_hits = Arc::clone(&total_hits);
            let key_ceiling = Arc::clone(&key_ceiling);
            let barrier = Arc::clone(&barrier);

            thread::Builder::new()
                .name(format!("reader-{t}"))
                .spawn(move || {
                    let mut rng = SmallRng::seed_from_u64(t as u64);
                    let mut local_reads = 0u64;
                    let mut local_hits = 0u64;

                    barrier.wait();

                    while running.load(Ordering::Relaxed) {
                        let ceiling = key_ceiling.load(Ordering::Acquire);
                        if ceiling == 0 {
                            continue;
                        }
                        let key = rng.random_range(0..ceiling);
                        if cache.get(&key).is_some() {
                            local_hits += 1;
                        }
                        local_reads += 1;

                        if local_reads % 10_000 == 0 {
                            total_reads.fetch_add(10_000, Ordering::Relaxed);
                            total_hits.fetch_add(local_hits, Ordering::Relaxed);
                            local_hits = 0;
                        }
                    }

                    let remainder = local_reads % 10_000;
                    total_reads.fetch_add(remainder, Ordering::Relaxed);
                    total_hits.fetch_add(local_hits, Ordering::Relaxed);
                })
                .expect("failed to spawn reader")
        })
        .collect();

    // --- Stats reporter (main thread) ---
    let mut last_reads = 0u64;
    let mut last_writes = 0u64;
    let mut last_time = Instant::now();

    while start.elapsed() < test_duration {
        thread::sleep(STATS_INTERVAL);
        let now = Instant::now();
        let elapsed = now.duration_since(last_time).as_secs_f64();
        let current_reads = total_reads.load(Ordering::Relaxed);
        let current_writes = total_writes.load(Ordering::Relaxed);
        let read_delta = current_reads - last_reads;
        let write_delta = current_writes - last_writes;
        let read_tp = read_delta as f64 / elapsed;
        let write_tp = write_delta as f64 / elapsed;

        println!(
            "  [{:>5.1}s] reads: {:>12} | writes: {:>10} | hit: {:>5.1}% | r/s: {:>10.0} | w/s: {:>8.0} | size: {}",
            start.elapsed().as_secs_f64(),
            current_reads,
            current_writes,
            if current_reads > 0 {
                total_hits.load(Ordering::Relaxed) as f64 / current_reads as f64 * 100.0
            } else {
                0.0
            },
            read_tp,
            write_tp,
            cache.len(),
        );

        last_reads = current_reads;
        last_writes = current_writes;
        last_time = now;
    }

    // Signal all threads to stop
    running.store(false, Ordering::Release);
    for w in writers {
        w.join().expect("writer panicked");
    }
    cleaner.join().expect("cleaner panicked");
    for r in readers {
        r.join().expect("reader panicked");
    }

    // Final report
    let final_reads = total_reads.load(Ordering::Relaxed);
    let final_writes = total_writes.load(Ordering::Relaxed);
    let final_hits = total_hits.load(Ordering::Relaxed);
    let final_evictions = total_evictions.load(Ordering::Relaxed);
    let duration = start.elapsed().as_secs_f64();

    println!("\n  --- Results ({reader_threads} readers + {WRITER_THREADS} writers) ---");
    println!("  Duration:       {duration:.1}s");
    println!("  Total reads:    {final_reads}");
    println!("  Total writes:   {final_writes}");
    println!("  Total hits:     {final_hits}");
    println!(
        "  Hit rate:       {:.1}%",
        if final_reads > 0 {
            final_hits as f64 / final_reads as f64 * 100.0
        } else {
            0.0
        }
    );
    println!("  Avg read tp:    {:.0} reads/s", final_reads as f64 / duration);
    println!("  Avg write tp:   {:.0} writes/s", final_writes as f64 / duration);
    println!(
        "  Per-reader:     {:.0} reads/s",
        final_reads as f64 / duration / reader_threads as f64
    );
    println!("  Total evictions: {final_evictions}");
    println!("  Final cache size: {}", cache.len());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut readers: Option<Vec<usize>> = None;
    let mut duration_secs: Option<u64> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--readers" | "-r" => {
                i += 1;
                if i < args.len() {
                    readers = Some(
                        args[i]
                            .split(',')
                            .filter_map(|s| s.trim().parse::<usize>().ok())
                            .collect(),
                    );
                }
            }
            "--duration" | "-d" => {
                i += 1;
                if i < args.len() {
                    duration_secs = args[i].trim().parse::<u64>().ok();
                }
            }
            "--help" | "-h" => {
                println!("Usage: stress_test [OPTIONS]");
                println!();
                println!("Options:");
                println!("  -r, --readers <N[,N,...]>  Reader thread counts (default: 1,4,8)");
                println!("  -d, --duration <SECS>      Test duration in seconds (default: 30)");
                println!("  -h, --help                 Show this help");
                return;
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let reader_configs = readers.unwrap_or_else(|| READER_THREAD_CONFIGS.to_vec());
    let test_duration = Duration::from_secs(duration_secs.unwrap_or(TEST_DURATION.as_secs()));

    println!("CacheMap Stress Test (Mixed Read/Write)");
    println!("========================================");

    for &threads in &reader_configs {
        run_scenario(CacheType::PapayaCache, threads, test_duration);
        run_scenario(CacheType::DashCache, threads, test_duration);
    }

    println!("\nAll scenarios complete.");
}
