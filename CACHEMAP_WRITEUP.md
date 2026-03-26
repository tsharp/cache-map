# CacheMap: Concurrent TTL Cache Investigation of DashMap vs Papaya

## Overview

`CacheMap` is a trait-based concurrent TTL cache with two backing implementations: `DashCache` (backed by `DashMap` v6, sharded `RwLock`) and `PapayaCache` (backed by `papaya`, lock-free reads). This document describes the design, the two optimization steps that improved throughput by **3–4×**, and the benchmark results collected on AMD EPYC hardware.

Initial testing suggested that `DashCache` stopped scaling at around 16 threads. Adding `CoarseClock` improved the read path by removing per-read timestamp calls, but the larger change was moving to lock-free reads with `papaya`. That comparison showed the earlier limit was primarily `RwLock` contention and allowed throughput to continue scaling up to the machine's 64 logical cores.

---

## Architecture

### Use Case

Any multi-threaded service that maintains hierarchical, time-bounded state. For example, a server with multiple cache tiers:

```
request → primary cache lookup → secondary cache → tertiary cache → origin
```

Each cache layer can have cascading eviction: when a parent entry expires, its dependent entries in child caches are evicted automatically.

### Design

```
CacheConfiguration<K, V>       Generic, implementation-agnostic configuration
        │
        ▼
   CacheMap<K, V>               Trait defining the cache contract
        │
        ├─────────────────┐
        ▼                 ▼
  DashCache<K, V>    PapayaCache<K, V>
  (DashMap/RwLock)   (papaya/lock-free)
        │                 │
        ├── CoarseClock ──┤     Shared: atomic timestamp, no per-read syscall
        ├── on_evict ─────┤     Shared: eviction callbacks for cascading
        ├── Lazy eviction ┤     Shared: expired entries removed on get()
        └── Cleanup ──────┘     Shared: periodic background cleanup
```

### Key Types

- **`CacheConfiguration<K, V>`**: builder for cache settings including default TTL, max capacity, initial capacity, shard count, cleanup interval, and `on_evict` callback.
- **`CacheMap<K, V>`**: trait defining `insert`, `insert_with_ttl`, `get`, `evict`, `refresh`, `contains_key`, `len`, and `clear`.
- **`DashCache<K, V>`**: DashMap-backed implementation using per-shard `RwLock`.
- **`PapayaCache<K, V>`**: papaya-backed implementation using lock-free reads with epoch-based reclamation.
- **`CoarseClock`**: shared coarse monotonic clock. A background thread calls `tick()` every 1ms, and readers check expiry with a single atomic load (~1ns) instead of `Instant::now()` (~20-30ns).
- **`EvictionListener<K, V>`**: `Arc<dyn Fn(K, V) + Send + Sync>` callback fired on every eviction.
- **`CacheError`**: currently only `MaxCapacityReached`.

---

## Implementation Details

### CoarseClock: Eliminating Per-Read Syscalls

The original `DashCache` called `Instant::now()` on every `get()` to check TTL expiry. On Linux this is a `clock_gettime(CLOCK_MONOTONIC)` vDSO call (~5-10ns), on Windows a `QueryPerformanceCounter` call (~20-30ns). At millions of reads per second per thread, this adds up.

`CoarseClock` replaces this with a two-part design:

1. **Background ticker thread**: calls `Instant::now()` once every 1ms, stores the result as milliseconds-since-epoch in an `AtomicU64`.
2. **Reader path**: a single `AtomicU64::load(Acquire)` (~1ns), with no syscall or kernel transition.

```rust
// Writer side (background thread, every 1ms)
pub fn tick(&self) {
    let ms = self.epoch.elapsed().as_millis() as u64;
    self.now_ms.store(ms, Ordering::Release);
}

// Reader side (every get() call)
pub fn is_expired(&self, expires_at_ms: u64) -> bool {
    self.now_ms.load(Ordering::Acquire) >= expires_at_ms
}
```

For TTLs measured in seconds, 1ms resolution is more than sufficient. The insert path still uses a real `Instant::now()` for accuracy because inserts are infrequent relative to reads.

Both `DashCache` and `PapayaCache` share the same `CoarseClock` design, so the gains apply to both implementations.

### TTL Expiration

Each entry stores `expires_at_ms` (milliseconds since the clock's epoch) at insert time. Expiration is enforced at two levels:

1. **Lazy eviction**: Every `get()`, `contains_key()`, and `refresh()` call checks expiry via `CoarseClock::is_expired()`. If expired, the entry is removed inline and the `on_evict` callback fires.

2. **Periodic cleanup**: A dedicated cleanup pass removes remaining expired entries.
   - **DashCache**: `retain` (single-threaded, no callback support) or `par_iter` + collect + remove (parallel, supports `on_evict`).
   - **PapayaCache**: Iterates via `pin()`, collects expired keys, removes and notifies.

### Adaptive Cleanup Strategy (DashCache)

`DashCache::from_config()` auto-selects the cleanup strategy:

```rust
let use_retain = !has_evict_callback && available_parallelism <= 8;
```

- **`retain`** is used when there's no `on_evict` callback AND the machine has ≤8 cores.
- **`par_iter`** is used otherwise because it is required for `on_evict` support and performs better than `retain` on machines with more than 8 cores.

### Lock-Free Reads (PapayaCache)

`papaya` uses epoch-based reclamation instead of `RwLock`. Readers call `pin()` to enter an epoch, then access the hash map without acquiring any lock. This means:

- **Reads never block on writes**: a write to any bucket does not stall readers.
- **No shard contention**: there are no shards or lock stripes, so the entire map is accessible concurrently.
- **Linear scaling**: read throughput scales with core count until the hardware (memory bandwidth, L3 capacity) is saturated.

The tradeoff is that `papaya` defers memory reclamation until all readers in an epoch have finished, which increases memory footprint under heavy write churn. In practice, with 250K entries and ~460K writes/s, memory usage remained stable.

### Eviction Callbacks and Cascading

The `on_evict` callback is invoked **outside** any lock. The cleanup flow:

1. Scan all entries, collecting keys of expired entries
2. For each expired key, `remove()` is called
3. `on_evict(key, value)` is called after removal

This means the callback can safely interact with other cache instances (e.g., cascading evictions across parent → child cache tiers) without risk of deadlock.

### Recommended Production Pattern: `on_evict` + mpsc

For cascading eviction across multiple cache tiers:

```
[reader threads]  →  primary_cache.get()           (hot path)
[writer threads]  →  primary_cache.insert()

[cleanup thread]  →  primary_cache.cleanup()
                       on_evict: tx.send(key)          ← ~50ns, non-blocking

[eviction worker] ←  rx.recv()
                       secondary_cache.evict(child_keys)
                       tertiary_cache.evict(grandchild_keys)
```

Benefits:
- Reader threads never touch the eviction channel, so the read path is unaffected
- Cascading deletes run on a dedicated thread, off the hot path
- Bounded channel provides natural backpressure if cascades pile up

---

## Benchmarking Environment

### Hardware

- **CPU**: AMD EPYC 9V45 96-Core Processor (Zen 5 Turin), 1 socket
- **Cores**: 32 physical, 64 logical (SMT/2 threads per core)
- **L3 Cache**: 128 MB
- **VM**: Cloud, Hyper-V

---

## Performance Results

### Optimization Timeline

The read throughput improvements came in two phases:

1. **CoarseClock**: replaced `Instant::now()` per read with an atomic load. This removed roughly 20-30ns of timestamp overhead per read and produced a measurable but modest improvement within DashCache.
2. **Lock-free reads (PapayaCache)**: replaced DashMap's sharded `RwLock` with papaya's epoch-based lock-free reads. This comparison showed that the earlier scaling limit was primarily `RwLock` contention, and it allowed throughput to continue scaling to the full 64 logical cores.

### Mixed Read/Write Stress Test

All tests: 4 writer threads, 250K target cache size, 500K target writes/s, 2s cleanup interval, CoarseClock enabled.

#### Original DashCache (pre-CoarseClock, `Instant::now()` per read)

| Config | Per-reader (reads/s) | Total read tp | Write tp | Scaling eff. |
|--------|---------------------|-------------|----------|--------------|
| 8r + 4w | 8.4M | 67M | 57K | 100% (baseline) |
| 16r + 4w | 8.2M | 131M | 63K | 98% |
| 32r + 4w | 7.3M | 233M | 63K | 87% |
| 64r + 4w | 5.5M | 350M | 99K | 65% |
| 128r + 4w | 3.0M | 386M | 42K | 36% |

Aggregate throughput plateaus at ~386M reads/s. Writers achieved only 42–99K writes/s against a 500K target, indicating substantial starvation from reader lock contention.

#### DashCache with CoarseClock

| Config | Per-reader (reads/s) | Total read tp | Write tp | Scaling eff. |
|--------|---------------------|-------------|----------|--------------|
| 8r + 4w | 8.8M | 70M | 53K | 100% (baseline) |
| 16r + 4w | 8.4M | 135M | 73K | 96% |
| 32r + 4w | 7.2M | 231M | 60K | 82% |
| 64r + 4w | 5.5M | 351M | 107K | 63% |
| 128r + 4w | 3.1M | 400M | 50K | 35% |

CoarseClock provided a ~5% per-reader improvement at low thread counts (8.4M → 8.8M at 8 readers) and a ~4% aggregate improvement at high thread counts (386M → 400M at 128 readers). The gains are real but modest. The dominant bottleneck remained `RwLock` contention on shard read locks rather than the timestamp call.

#### PapayaCache with CoarseClock (lock-free reads)

| Config | Per-reader (reads/s) | Total read tp | Write tp | Scaling eff. |
|--------|---------------------|-------------|----------|--------------|
| 8r + 4w | 31.6M | 253M | 460K | 100% (baseline) |
| 16r + 4w | 31.2M | 499M | 460K | 99% |
| 32r + 4w | 25.7M | 822M | 461K | 81% |
| 64r + 4w | 21.2M | 1,360M | 470K | 67% |
| 128r + 4w | 10.5M | 1,349M | 444K | 33% |

Lock-free reads delivered a **3.6× per-reader improvement** at 8 threads (8.8M → 31.6M) and a **3.4× aggregate improvement** at 128 threads (400M → 1,349M). Aggregate throughput scales nearly linearly from 8 to 64 readers (253M → 1,360M, 5.4× for 8× threads) and then plateaus at the machine's 64 logical cores. This strongly suggests the earlier scaling limit was due to lock contention.

### Head-to-Head Comparison

| Readers | DashCache reads/s | PapayaCache reads/s | Ratio | DashCache writes/s | PapayaCache writes/s | Write ratio |
|---------|-------------------|---------------------|-------|--------------------|----------------------|-------------|
| 8 | 70M | 253M | 3.6× | 53K | 460K | 8.7× |
| 16 | 135M | 499M | 3.7× | 73K | 460K | 6.3× |
| 32 | 231M | 822M | 3.6× | 60K | 461K | 7.7× |
| 64 | 351M | 1,360M | 3.9× | 107K | 470K | 4.4× |
| 128 | 400M | 1,349M | 3.4× | 50K | 444K | 8.9× |

### Writer Starvation: The Hidden Cost of RwLock

The most striking difference is write throughput. Both implementations target 500K writes/s via throttled writer threads. PapayaCache consistently achieves 89–94% of target (444–470K writes/s). DashCache achieves only 10–21% of target (50–107K writes/s).

This happens because DashMap's `RwLock` allows unlimited concurrent readers on a shard. Under heavy read load, writers rarely get a chance to acquire the exclusive write lock. The result:

| Readers | DashCache writes/s | % of 500K target | PapayaCache writes/s | % of 500K target |
|---------|--------------------|-------------------|----------------------|-------------------|
| 8 | 53K | 11% | 460K | 92% |
| 16 | 73K | 15% | 460K | 92% |
| 32 | 60K | 12% | 461K | 92% |
| 64 | 107K | 21% | 470K | 94% |
| 128 | 50K | 10% | 444K | 89% |

With DashCache, the key ceiling (total keys ever written) after 30 seconds is ~1–3M. With PapayaCache, it reaches ~14M. This means DashCache readers search a much smaller key space, which artificially inflates hit rates. The higher hit rate here is a symptom of writer starvation rather than better caching behavior.

### Where the Hardware Ceiling Actually Is

PapayaCache's results reveal the true hardware scaling profile of this machine:

- **8–32 readers**: near-linear aggregate scaling, ~25-31M reads/s per thread.
- **64 readers**: aggregate plateau at ~1.36B reads/s. Per-reader drops to 21M, which is consistent with 64 threads on 64 logical cores reaching saturation.
- **128 readers**: aggregate flat at ~1.35B reads/s. Per-reader halves to 10.5M, which is consistent with oversubscription and two threads competing for each logical core.

The transition at 64 threads aligns with the logical core count, which points to CPU time-sharing as the primary limit at that point. DashCache's earlier plateau at ~16 threads obscured that result because `RwLock` contention dominated before hardware limits became relevant.

---

## Key Findings

### 1. RwLock Contention Was the Primary Bottleneck

The PapayaCache results make the bottleneck much clearer. With lock-free reads, per-reader throughput holds at 31M reads/s from 8 to 16 threads and then degrades gradually (25.7M at 32, 21.2M at 64). Aggregate throughput also scales to 64 threads. In these measurements, the dominant limitation in DashCache was the `RwLock` contention penalty.

### 2. CoarseClock: Necessary but Not Sufficient

Replacing `Instant::now()` with an atomic load saved ~20-30ns per read. At 30M reads/s/thread, that is 600–900ms of CPU time saved per second per thread. Within DashCache, however, the gain was largely masked by `RwLock` contention. CoarseClock provides the most benefit when paired with lock-free reads, where the timestamp check becomes a larger share of the read-path cost.

### 3. Writer Starvation Under Read-Heavy Workloads

DashMap's `RwLock` allows unlimited concurrent readers, which starves writers under high read load. DashCache achieved only 10–21% of target write throughput across all configurations. PapayaCache, with no reader locks to compete against, consistently hit 89–94%. For workloads with continuous write churn (session stores, rate limiters, connection trackers), this starvation can cause stale data and inconsistent behavior.

### 4. Cache Size Remained Stable

Both implementations held exactly 250,000 ± 5 entries across all runs with continuous writers, periodic cleanup, and lazy eviction. No memory growth, no drift.

### 5. Lazy Eviction Dominates Cleanup

With many reader threads, most expired entries are caught by `get()` before the cleanup thread runs. The cleanup thread is a backstop, not the primary eviction mechanism.

### 6. Misses Are Cheaper Than Hits

Throughput increases as hit rate drops. Returning `None` for a miss skips the value clone and reduces the amount of work on the read path.

### 7. Allocator Choice Was Not a Primary Lever For This Workload

The EPYC rerun with the stress test linked against `mimalloc` did not show a clear material improvement over the earlier numbers for this benchmark shape. The workload here uses small `u64 -> u64` entries, so allocator overhead is a relatively small fraction of total cost compared with synchronization, cache-line movement, and core saturation.

That does not mean allocator choice never matters. `mimalloc` and `jemalloc` are still reasonable options to test, but they are more likely to help when cache entries are larger, values require heap allocation, or eviction and cloning involve more allocator traffic than this benchmark does.

---

## Production Recommendations

### When to Use PapayaCache vs DashCache

| Consideration | PapayaCache | DashCache |
|---------------|-------------|-----------|
| Read-heavy workloads (>4 reader threads) | Recommended based on measured 3.6× higher throughput | Adequate if throughput is not critical |
| Write-heavy workloads | Recommended because writer starvation was not observed in testing | Risk of writer starvation under concurrent reads |
| Memory footprint | Slightly higher (epoch-based deferred reclamation) | Lower (immediate reclamation on eviction) |
| Ecosystem maturity | Newer (`papaya` 0.2.x) | Mature (`dashmap` 6.x, widely deployed) |
| `on_evict` callbacks | Supported | Supported (with `par_iter` cleanup) |
| Cascading multi-tier caches | Supported | Supported |

For read-heavy services, the measurements favor PapayaCache. DashCache remains viable for low-concurrency or write-dominated workloads where the `RwLock` overhead is less significant.

### Latency Budget Impact

At PapayaCache's worst measured per-thread throughput (10.5M reads/s at 128 threads), a single cache lookup is ~95ns. At the recommended operating point (≤64 threads), it is ~32-48ns. For services with per-request latency budgets in the hundreds of microseconds, the cache lookup cost is small relative to the overall request budget.

### Write Headroom

PapayaCache sustains 444–470K inserts/s under all tested configurations. For workloads handling thousands of new entries per second, that leaves substantial headroom. DashCache's write starvation under load should be factored into capacity planning if DashCache is used.

### Allocator

For the current benchmark workload, switching the stress test to `mimalloc` did not produce a clear material gain. That result is consistent with the benchmark using very small entries, where allocator cost is not the dominant bottleneck.

`mimalloc` and `jemalloc` are still worth evaluating for more allocation-heavy scenarios, especially if cached values are larger, contain owned buffers or strings, or trigger significantly more heap churn during insert, clone, and eviction. In those cases, allocator behavior can matter more than it does for a `u64 -> u64` stress test.

### Scaling Strategy

With PapayaCache, scaling is straightforward: add reader threads up to the logical core count and throughput scales well. No special scheduling was required in these tests. Beyond the logical core count, oversubscription halves per-thread throughput, so reader count should stay at or below available logical cores.

---

## Test Suite

9 unit tests covering all cache operations:

| Test | Coverage |
|------|----------|
| `insert_and_get` | Basic insert + read-back |
| `expired_entry_returns_none` | TTL expiry on `get()` |
| `insert_with_custom_ttl` | Per-entry TTL override |
| `evict_returns_value` | Manual eviction returns value |
| `refresh_extends_ttl` | TTL refresh keeps entry alive |
| `contains_key_respects_expiry` | `contains_key()` respects TTL |
| `cleanup_removes_expired` | Periodic cleanup removes expired |
| `clear_removes_all` | `clear()` empties cache |
| `on_evict_fires_on_expiry` | Eviction callback fires correctly |

All tests pass: `cargo test -p cache-map`

---

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `dashmap` | 6.1.0 | Concurrent hash map (features: `inline`, `raw-api`, `rayon`) |
| `papaya` | 0.2.3 | Lock-free concurrent hash map |
| `mimalloc` | 0.1 | Alternative allocator used by the stress test binary |
| `rayon` | 1.x | Parallel iteration for DashCache cleanup |
| `rand` | 0.10 | Random key generation in stress tests |
| `criterion` | 0.8 | Microbenchmarks (dev-dependency) |

---

## Running

```bash
# Unit tests
cargo test -p cache-map

# Stress test (default: 1/4/8 readers, 30s per scenario)
cargo run --release -p cache-map --bin stress_test

# Custom reader configs and duration
cargo run --release -p cache-map --bin stress_test -- --readers 8,16,32,64,128 --duration 30

# Criterion microbenchmark
cargo bench -p cache-map --bench contention
```
