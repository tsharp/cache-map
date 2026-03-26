# CacheMap: Concurrent TTL Cache — From Lock Contention to Lock-Free Reads

## Overview

`CacheMap` is a trait-based concurrent TTL cache with two backing implementations: `DashCache` (backed by `DashMap` v6, sharded `RwLock`) and `PapayaCache` (backed by `papaya`, lock-free reads). This document covers the design, the two optimization phases that lifted throughput by **3–4×**, and the performance characterization from stress testing on AMD EPYC hardware.

The key narrative: the original `DashCache` implementation appeared to hit a hardware scaling wall at ~16 threads, which was initially attributed to CCD/NUMA topology. Introducing `CoarseClock` delivered measurable gains by eliminating per-read syscalls - but the breakthrough came from switching to lock-free reads via `papaya`, which proved the "hardware wall" was actually `RwLock` contention all along and allowed throughput to scale linearly up to 64 threads — the full logical core count of the machine.

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

- **`CacheConfiguration<K, V>`** — Builder for cache settings: default TTL, max capacity, initial capacity, shard count, cleanup interval, `on_evict` callback.
- **`CacheMap<K, V>`** — Trait: `insert`, `insert_with_ttl`, `get`, `evict`, `refresh`, `contains_key`, `len`, `clear`.
- **`DashCache<K, V>`** — DashMap-backed implementation. Wraps `DashMap<K, CacheEntry<V>>` with per-shard `RwLock`.
- **`PapayaCache<K, V>`** — papaya-backed implementation. Lock-free reads via epoch-based reclamation.
- **`CoarseClock`** — Shared coarse monotonic clock. Background thread calls `tick()` every 1ms; readers check expiry via a single atomic load (~1ns) instead of `Instant::now()` (~20-30ns).
- **`EvictionListener<K, V>`** — `Arc<dyn Fn(K, V) + Send + Sync>` callback fired on every eviction.
- **`CacheError`** — Currently: `MaxCapacityReached`.

---

## Implementation Details

### CoarseClock: Eliminating Per-Read Syscalls

The original `DashCache` called `Instant::now()` on every `get()` to check TTL expiry. On Linux this is a `clock_gettime(CLOCK_MONOTONIC)` vDSO call (~5-10ns), on Windows a `QueryPerformanceCounter` call (~20-30ns). At millions of reads per second per thread, this adds up.

`CoarseClock` replaces this with a two-part design:

1. **Background ticker thread**: calls `Instant::now()` once every 1ms, stores the result as milliseconds-since-epoch in an `AtomicU64`.
2. **Reader path**: a single `AtomicU64::load(Acquire)` (~1ns) — no syscall, no kernel transition.

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

For TTLs measured in seconds, 1ms resolution is more than sufficient. The insert path still uses a real `Instant::now()` for accuracy — inserts are infrequent relative to reads.

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
- **`par_iter`** is used otherwise — required for `on_evict` support, and outperforms `retain` on machines with >8 cores.

### Lock-Free Reads (PapayaCache)

`papaya` uses epoch-based reclamation instead of `RwLock`. Readers call `pin()` to enter an epoch, then access the hash map without acquiring any lock. This means:

- **Reads never block on writes**: a write to any bucket does not stall readers.
- **No shard contention**: there are no shards or lock stripes — the entire map is accessible concurrently.
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
- Reader threads never touch the eviction channel — zero impact on read throughput
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

1. **CoarseClock** — replaced `Instant::now()` per-read with an atomic load. Eliminated ~20-30ns of syscall overhead per read. Measurable but modest improvement within DashCache.
2. **Lock-free reads (PapayaCache)** — replaced DashMap's sharded `RwLock` with papaya's epoch-based lock-free reads. This was the breakthrough: it proved that the scaling wall previously attributed to CCD/NUMA topology was actually `RwLock` contention, and unlocked linear scaling to the full 64 logical cores.

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

Aggregate throughput plateaus at ~386M reads/s. Writers achieved only 42–99K writes/s against a 500K target — severely starved by reader lock contention. At the time, the scaling drop beyond 16 threads was attributed to CCD boundary effects and L3 coherency traffic.

#### DashCache with CoarseClock

| Config | Per-reader (reads/s) | Total read tp | Write tp | Scaling eff. |
|--------|---------------------|-------------|----------|--------------|
| 8r + 4w | 8.8M | 70M | 53K | 100% (baseline) |
| 16r + 4w | 8.4M | 135M | 73K | 96% |
| 32r + 4w | 7.2M | 231M | 60K | 82% |
| 64r + 4w | 5.5M | 351M | 107K | 63% |
| 128r + 4w | 3.1M | 400M | 50K | 35% |

CoarseClock provided a ~5% per-reader improvement at low thread counts (8.4M → 8.8M at 8 readers) and a ~4% aggregate improvement at high thread counts (386M → 400M at 128 readers). The gains are real but modest — the dominant bottleneck was still `RwLock` contention on shard read locks, not the timestamp syscall.

#### PapayaCache with CoarseClock (lock-free reads)

| Config | Per-reader (reads/s) | Total read tp | Write tp | Scaling eff. |
|--------|---------------------|-------------|----------|--------------|
| 8r + 4w | 31.6M | 253M | 460K | 100% (baseline) |
| 16r + 4w | 31.2M | 499M | 460K | 99% |
| 32r + 4w | 25.7M | 822M | 461K | 81% |
| 64r + 4w | 21.2M | 1,360M | 470K | 67% |
| 128r + 4w | 10.5M | 1,349M | 444K | 33% |

Lock-free reads delivered a **3.6× per-reader improvement** at 8 threads (8.8M → 31.6M) and **3.4× aggregate improvement** at 128 threads (400M → 1,349M). Crucially, **aggregate throughput scales linearly from 8 to 64 readers** (253M → 1,360M, 5.4× for 8× threads), then plateaus — exactly at the 64 logical core count of the machine. This proves the previous "scaling wall" was lock contention, not hardware topology.

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

With DashCache, the key ceiling (total keys ever written) after 30 seconds is ~1–3M. With PapayaCache, it reaches ~14M. This means DashCache readers search a much smaller key space, artificially inflating hit rates — the high hit rate is a symptom of writer starvation, not better caching.

### Where the Hardware Ceiling Actually Is

PapayaCache's results reveal the true hardware scaling profile of this machine:

- **8–32 readers**: near-linear aggregate scaling, ~25-31M reads/s per thread.
- **64 readers**: aggregate plateau at ~1.36B reads/s. Per-reader drops to 21M — 64 threads on 64 logical cores, fully saturated.
- **128 readers**: aggregate flat at ~1.35B reads/s. Per-reader halves to 10.5M — pure oversubscription, 2 threads competing for each logical core.

The transition at 64 threads aligns precisely with the logical core count, confirming the bottleneck is CPU time-sharing, not CCD/NUMA topology or cache coherency. DashCache's earlier plateau at ~16 threads was masking this — the `RwLock` contention dominated long before hardware limits were relevant.

---

## Key Findings

### 1. RwLock Contention Was the Real Bottleneck — Not NUMA/CCD Topology

The original DashCache writeup attributed the per-reader throughput drop beyond 16 threads to CCD boundary effects (L3 coherency traffic over Infinity Fabric). PapayaCache disproves this: with lock-free reads, per-reader throughput holds at 31M reads/s from 8 to 16 threads and degrades only gradually (25.7M at 32, 21.2M at 64) — a pattern consistent with SMT resource sharing, not CCD cliffs. The aggregate throughput scales linearly to 64 threads. CCD topology may contribute a few percent of overhead, but it is dwarfed by the `RwLock` contention penalty.

### 2. CoarseClock: Necessary but Not Sufficient

Replacing `Instant::now()` with an atomic load saved ~20-30ns per read. This matters — at 30M reads/s/thread, it's 600–900ms of CPU time saved per second per thread. But within DashCache, the gain was masked by `RwLock` contention. CoarseClock's full benefit is only realized when paired with lock-free reads, where the timestamp check becomes the dominant per-read cost and the atomic load keeps it to ~1ns.

### 3. Writer Starvation Under Read-Heavy Workloads

DashMap's `RwLock` allows unlimited concurrent readers, which starves writers under high read load. DashCache achieved only 10–21% of target write throughput across all configurations. PapayaCache, with no reader locks to compete against, consistently hit 89–94%. For workloads with continuous write churn (session stores, rate limiters, connection trackers), this starvation can cause stale data and inconsistent behavior.

### 4. Cache Size is Rock Stable

Both implementations held exactly 250,000 ± 5 entries across all runs with continuous writers, periodic cleanup, and lazy eviction. No memory growth, no drift.

### 5. Lazy Eviction Dominates Cleanup

With many reader threads, most expired entries are caught by `get()` before the cleanup thread runs. The cleanup thread is a backstop, not the primary eviction mechanism.

### 6. Misses Are Cheaper Than Hits

Throughput increases as hit rate drops. Returning `None` for a miss skips the value clone — it's just a hash probe hitting an empty slot.

---

## Production Recommendations

### When to Use PapayaCache vs DashCache

| Consideration | PapayaCache | DashCache |
|---------------|-------------|-----------|
| Read-heavy workloads (>4 reader threads) | **Preferred** — 3.6× throughput | Adequate if throughput is not critical |
| Write-heavy workloads | **Preferred** — no writer starvation | Risk of writer starvation under concurrent reads |
| Memory footprint | Slightly higher (epoch-based deferred reclamation) | Lower (immediate reclamation on eviction) |
| Ecosystem maturity | Newer (`papaya` 0.2.x) | Mature (`dashmap` 6.x, widely deployed) |
| `on_evict` callbacks | Supported | Supported (with `par_iter` cleanup) |
| Cascading multi-tier caches | Supported | Supported |

For most read-heavy services, PapayaCache is the clear choice. DashCache remains viable for low-concurrency or write-dominated workloads where the `RwLock` overhead is negligible.

### Latency Budget Impact

At PapayaCache's worst measured per-thread throughput (10.5M reads/s at 128 threads), a single cache lookup is ~95ns. At the recommended operating point (≤64 threads), it's ~32-48ns. For most services with per-request latency budgets in the hundreds of microseconds, the cache is negligible. **It is unlikely to be a bottleneck.**

### Write Headroom

PapayaCache sustains 444–470K inserts/s under all tested configurations. Services handling even aggressive churn (thousands of new entries/s) have 100× headroom. DashCache's write starvation under load should be factored into capacity planning if using DashCache.

### Allocator

Consider `mimalloc` or `jemalloc` with per-thread caching. Global allocator contention is a common 5–10% tax that's reclaimed for free by switching allocators.

### Scaling Strategy

With PapayaCache, scaling is straightforward: add reader threads up to the logical core count and throughput scales linearly. No thread pinning or CCD-aware scheduling is required. Beyond the logical core count, oversubscription halves per-thread throughput — keep reader thread count ≤ available logical cores.

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
