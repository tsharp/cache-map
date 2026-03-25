use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use ttl_hash_map::cache::{CacheConfiguration, CacheMap};
use ttl_hash_map::dash_cache::DashCache;

const TEST_DURATION: Duration = Duration::from_secs(60);
const LOAD_INTERVAL: Duration = Duration::from_secs(10);
const ELEMENTS_PER_LOAD: u64 = 100_000;
const READER_THREAD_CONFIGS: &[usize] = &[4, 8, 16];
const STATS_INTERVAL: Duration = Duration::from_secs(5);

fn run_scenario(reader_threads: usize) {
    println!("\n{}", "=".repeat(60));
    println!("  Stress test: {reader_threads} reader threads, {TEST_DURATION:?} duration");
    println!("  Loader: {ELEMENTS_PER_LOAD} elements every {LOAD_INTERVAL:?}");
    println!("{}\n", "=".repeat(60));

    let config = CacheConfiguration::new()
        .set_default_ttl(Duration::from_secs(15)); // base TTL, overridden per-entry
    let cache: Arc<DashCache<u64, u64>> = Arc::new(DashCache::from_config(config));

    let running = Arc::new(AtomicBool::new(true));
    let total_reads = Arc::new(AtomicU64::new(0));
    let total_hits = Arc::new(AtomicU64::new(0));
    let total_loads = Arc::new(AtomicU64::new(0));
    let total_evictions = Arc::new(AtomicU64::new(0));
    let key_ceiling = Arc::new(AtomicU64::new(0));

    // Pre-seed the cache so readers have something from the start
    {
        let mut rng = SmallRng::seed_from_u64(0xBEEF);
        for i in 0..ELEMENTS_PER_LOAD {
            let ttl_ms = rng.gen_range(5_000..20_000u64);
            let _ = cache.insert_with_ttl(i, i, Duration::from_millis(ttl_ms));
        }
        key_ceiling.store(ELEMENTS_PER_LOAD, Ordering::Release);
    }

    let start = Instant::now();

    // --- Loader thread ---
    let loader = {
        let cache = Arc::clone(&cache);
        let running = Arc::clone(&running);
        let total_loads = Arc::clone(&total_loads);
        let total_evictions = Arc::clone(&total_evictions);
        let key_ceiling = Arc::clone(&key_ceiling);

        thread::Builder::new()
            .name("loader".into())
            .spawn(move || {
                let mut rng = SmallRng::seed_from_u64(0xCAFE);
                let mut batch = 1u64;

                while running.load(Ordering::Relaxed) {
                    thread::sleep(LOAD_INTERVAL);
                    if !running.load(Ordering::Relaxed) {
                        break;
                    }

                    // Cleanup expired entries first
                    let before = cache.len();
                    cache.cleanup();
                    let after = cache.len();
                    let evicted = before.saturating_sub(after);
                    total_evictions.fetch_add(evicted as u64, Ordering::Relaxed);

                    // Load new batch with variable TTLs
                    let base_key = batch * ELEMENTS_PER_LOAD;
                    for i in 0..ELEMENTS_PER_LOAD {
                        let key = base_key + i;
                        // Variable TTL: ~40% expire before next interval, ~60% survive
                        let ttl_ms = if rng.gen_bool(0.4) {
                            rng.gen_range(2_000..8_000u64) // expires before next 10s
                        } else {
                            rng.gen_range(10_000..25_000u64) // survives into next interval
                        };
                        let _ = cache.insert_with_ttl(key, key, Duration::from_millis(ttl_ms));
                    }

                    let new_ceiling = base_key + ELEMENTS_PER_LOAD;
                    key_ceiling.store(new_ceiling, Ordering::Release);
                    total_loads.fetch_add(ELEMENTS_PER_LOAD, Ordering::Relaxed);
                    batch += 1;

                    println!(
                        "  [{:>5.1}s] Loaded batch {batch}: evicted {evicted}, cache size {}",
                        start.elapsed().as_secs_f64(),
                        cache.len()
                    );
                }
            })
            .expect("failed to spawn loader")
    };

    // --- Reader threads ---
    // Use a barrier so all readers start hitting the cache simultaneously
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
                        // Read keys from the full range (0..ceiling)
                        // This means many reads will miss (expired or not yet loaded)
                        let key = rng.gen_range(0..ceiling);
                        if cache.get(&key).is_some() {
                            local_hits += 1;
                        }
                        local_reads += 1;

                        // Flush to shared counters periodically to reduce contention on atomics
                        if local_reads % 10_000 == 0 {
                            total_reads.fetch_add(10_000, Ordering::Relaxed);
                            total_hits.fetch_add(local_hits, Ordering::Relaxed);
                            local_hits = 0;
                        }
                    }

                    // Flush remaining
                    let remainder = local_reads % 10_000;
                    total_reads.fetch_add(remainder, Ordering::Relaxed);
                    total_hits.fetch_add(local_hits, Ordering::Relaxed);
                })
                .expect("failed to spawn reader")
        })
        .collect();

    // --- Stats reporter (main thread) ---
    let mut last_reads = 0u64;
    let mut last_time = Instant::now();

    while start.elapsed() < TEST_DURATION {
        thread::sleep(STATS_INTERVAL);
        let now = Instant::now();
        let elapsed = now.duration_since(last_time).as_secs_f64();
        let current_reads = total_reads.load(Ordering::Relaxed);
        let delta = current_reads - last_reads;
        let throughput = delta as f64 / elapsed;

        println!(
            "  [{:>5.1}s] reads: {current_reads:>12} | hit rate: {:>5.1}% | throughput: {:>10.0} reads/s | cache size: {}",
            start.elapsed().as_secs_f64(),
            if current_reads > 0 {
                total_hits.load(Ordering::Relaxed) as f64 / current_reads as f64 * 100.0
            } else {
                0.0
            },
            throughput,
            cache.len(),
        );

        last_reads = current_reads;
        last_time = now;
    }

    // Signal all threads to stop
    running.store(false, Ordering::Release);
    loader.join().expect("loader panicked");
    for r in readers {
        r.join().expect("reader panicked");
    }

    // Final report
    let final_reads = total_reads.load(Ordering::Relaxed);
    let final_hits = total_hits.load(Ordering::Relaxed);
    let duration = start.elapsed().as_secs_f64();

    println!("\n  --- Results ({reader_threads} reader threads) ---");
    println!("  Duration:     {duration:.1}s");
    println!("  Total reads:  {final_reads}");
    println!("  Total hits:   {final_hits}");
    println!(
        "  Hit rate:     {:.1}%",
        if final_reads > 0 {
            final_hits as f64 / final_reads as f64 * 100.0
        } else {
            0.0
        }
    );
    println!("  Avg throughput: {:.0} reads/s", final_reads as f64 / duration);
    println!(
        "  Per-thread:     {:.0} reads/s",
        final_reads as f64 / duration / reader_threads as f64
    );
    println!("  Total loads:    {}", total_loads.load(Ordering::Relaxed));
    println!(
        "  Total evictions: {}",
        total_evictions.load(Ordering::Relaxed)
    );
    println!("  Final cache size: {}", cache.len());
}

fn main() {
    println!("DashCache Stress Test");
    println!("=====================");

    for &threads in READER_THREAD_CONFIGS {
        run_scenario(threads);
    }

    println!("\nAll scenarios complete.");
}
