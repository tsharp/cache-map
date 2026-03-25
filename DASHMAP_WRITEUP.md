# DashCache: A Concurrent TTL Cache

## Overview

`DashCache` is a concurrent, TTL-based cache backed by `DashMap` v6, suitable for any multi-threaded service that needs fast, lock-sharded key-value storage with automatic expiration. Common use cases include session stores, connection state tracking, request deduplication, rate limiting, and multi-tier caches with cascading eviction. This document covers the design, implementation decisions, and performance characterization from microbenchmarks through large-scale stress testing on AMD EPYC hardware.

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
        ▼
  DashCache<K, V>               DashMap-backed implementation with TTL
        │
        ├── on_evict callback    Fires on expiry/removal (for cascading)
        ├── Lazy eviction        Expired entries removed on get()
        └── Periodic cleanup     Background cleanup via par_iter or retain
```

### Key Types

- **`CacheConfiguration<K, V>`** — Builder for cache settings: default TTL, max capacity, initial capacity, shard count, cleanup interval, `on_evict` callback.
- **`CacheMap<K, V>`** — Trait: `insert`, `insert_with_ttl`, `get`, `evict`, `refresh`, `contains_key`, `len`, `clear`.
- **`DashCache<K, V>`** — The concrete implementation. Wraps `DashMap<K, CacheEntry<V>>` where `CacheEntry` stores the value and `expires_at: Instant`.
- **`EvictionListener<K, V>`** — `Arc<dyn Fn(K, V) + Send + Sync>` callback fired on every eviction.
- **`CacheError`** — Currently: `MaxCapacityReached`.

---

## Implementation Details

### TTL Expiration

Each entry records its `expires_at` timestamp at insert time. Expiration is enforced at two levels:

1. **Lazy eviction**: Every `get()`, `contains_key()`, and `refresh()` call checks expiry. If the entry is expired, it is removed inline and the `on_evict` callback fires. This means most expired entries are cleaned up organically by readers.

2. **Periodic cleanup**: A dedicated cleanup pass removes all remaining expired entries. Two strategies are available:
   - **`retain`**: DashMap's built-in `retain()` — single-threaded, iterates all shards. Cannot fire `on_evict` callbacks (values are dropped in-place).
   - **`par_iter` + collect + remove**: Parallel scan using Rayon, collects expired keys, then removes and notifies. Can fire `on_evict`.

### Adaptive Cleanup Strategy

`DashCache::from_config()` auto-selects the cleanup strategy:

```rust
let use_retain = !has_evict_callback && available_parallelism <= 8;
```

- **`retain`** is used when there's no `on_evict` callback AND the machine has ≤8 cores. It avoids Rayon overhead on small machines.
- **`par_iter`** is used otherwise — required for `on_evict` support, and outperforms `retain` on machines with >8 cores.

### Eviction Callbacks and Cascading

The `on_evict` callback is invoked **outside** any DashMap shard lock. The cleanup flow:

1. `par_iter()` scans all entries, collecting keys of expired entries
2. For each expired key, `remove()` is called (acquires a brief write lock on one shard)
3. `on_evict(key, value)` is called after the lock is released

This means the callback can safely interact with other `DashCache` instances (e.g., cascading evictions across parent → child cache tiers) without risk of deadlock.

### Recommended Production Pattern: `on_evict` + mpsc

For cascading eviction across multiple cache tiers:

```
[reader threads]  →  primary_cache.get()           (hot path, ~120-180ns)
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

### Shard Count

Default shard count is `available_parallelism × 4`. On a 32-core/64-thread EPYC, this yields 256 shards, providing low per-shard contention (~0.25 threads per shard on average).

---

## Benchmarking Environment

### Hardware

- **CPU**: AMD EPYC 9V45 96-Core Processor (Zen 5 Turin), 1 socket
- **Cores**: 32 physical, 64 logical (SMT/2 threads per core)
- **L3 Cache**: 128 MB (4 × 32 MB — one per CCD)
- **CCDs**: 4, each with 8 physical cores sharing 32 MB L3
- **NUMA**: 1 node (hypervisor-level; physical CCD topology still matters)
- **VM**: Cloud, Hyper-V

### CCD Layout (Estimated)

| CCD | Physical Cores | Logical CPUs |
|-----|---------------|-------------|
| 0 | 0–7 | 0–7, 32–39 |
| 1 | 8–15 | 8–15, 40–47 |
| 2 | 16–23 | 16–23, 48–55 |
| 3 | 24–31 | 24–31, 56–63 |

Verify with: `cat /sys/devices/system/cpu/cpu0/cache/index3/shared_cpu_list`

---

## Performance Results

### Read-Only Stress Test

Initial testing with a single loader thread adding 100K elements every 10s, variable TTL:

| Threads | Per-thread (reads/s) | Total (reads/s) | Notes |
|---------|---------------------|-----------------|-------|
| 8 pinned (CCD 0) | 19.5M | 156M | Best per-thread |
| 16 unpinned | 7.2M | 115M | NUMA cliff — worst total |
| 32 unpinned | 8.3M | 266M | Recovery, even CCD distribution |
| 64 unpinned | 5.8M | 374M | SMT saturation |

### Mixed Read/Write Stress Test

4 continuous writer threads, 250K target cache size, 2s cleanup interval, 60s duration:

| Config | Per-reader (reads/s) | Total read tp | Write tp | Scaling efficiency |
|--------|---------------------|-------------|----------|-------------------|
| 4r + 4w | 8.8M | 35M | 67K | 100% (baseline) |
| 8r + 4w | 8.4M | 67M | 57K | 96% |
| 16r + 4w | 8.2M | 131M | 63K | 93% |
| 32r + 4w | 7.3M | 233M | 63K | 83% |
| 64r + 4w | 5.5M | 350M | 99K | 63% |
| 128r + 4w | 3.0M | 386M | 42K | 34% |

### `retain` vs `par_iter` Cleanup (32 readers + 4 writers)

| Strategy | Total read tp | Per-reader | Delta |
|----------|-------------|-----------|-------|
| par_iter | 233M | 7.3M | baseline |
| retain | 227M | 7.1M | -3% |

On this 32-core EPYC, `par_iter` is marginally faster. `retain` wins on ≤8 core machines (~17% faster there).

---

## Key Findings

### 1. CCD Boundary is the Scaling Cliff

Per-reader throughput is remarkably stable within a CCD (8.2–8.8M reads/s from 4 to 16 readers), then drops significantly as threads cross CCD boundaries. The root cause is L3 cache coherency traffic over Infinity Fabric (~100ns cross-CCD vs ~10ns intra-CCD).

### 2. Writes Don't Hurt Reads

Adding 4 writer threads (63K writes/s) caused only a 6–12% read throughput reduction. DashMap's per-shard write locks are highly localized — a write to shard N doesn't block reads on shards 0–(N-1) or (N+1)–max.

### 3. Cache Size is Rock Stable

With continuous writers, periodic cleanup, and lazy eviction, the cache held exactly 250,000 ± 2 entries across all 60-second runs. No memory growth, no drift.

### 4. Lazy Eviction Dominates Cleanup

With many reader threads, most expired entries are caught by `get()` returning `None` before the cleanup thread runs. At 128 readers: only 635 explicit evictions vs 34,668 at 4 readers. The cleanup thread is a backstop, not the primary eviction mechanism.

### 5. No Lock Convoy

Even at 128 threads (2:1 oversubscription on a 64-logical-CPU machine), there is no throughput collapse. Scaling degrades gracefully — the bottleneck is CPU time-sharing and cache coherency, not lock contention.

### 6. Misses Are Cheaper Than Hits

Throughput increases as hit rate drops. Returning `None` for a miss skips the value clone and most expiry logic — it's just a hash probe hitting an empty slot.

---

## Production Recommendations

### Process Thread Pinning

**Pin the cache-heavy process to one CCD (8 physical cores).**

```bash
taskset -c 0-7 ./your_service
```

This gives:
- 8 threads sharing 32 MB L3 — all cache lookups local
- ~8.5M lookups/s per-thread under mixed read/write
- 68M total lookups/s — sufficient for most workloads
- Remaining 56 cores free for other services, background tasks, I/O

If TLS termination, serialization, or compression saturate 8 cores, pin to 16 cores (2 CCDs) — only 7% efficiency loss.

### Latency Budget Impact

At the worst measured per-thread throughput (3.0M reads/s at 128 threads), a single cache lookup is ~333ns. At the recommended 8-thread config, it's ~120ns. For most services with per-request latency budgets in the hundreds of microseconds, the cache is negligible. **It is unlikely to be a bottleneck.**

### Write Headroom

Measured write throughput: 42K–99K inserts/s. Services handling even aggressive churn (thousands of new entries/s) have 10–20× headroom.

### Allocator

Consider `mimalloc` or `jemalloc` with per-thread caching. Global allocator contention is a common 5–10% tax on NUMA machines that's reclaimed for free by switching allocators.

### Future NUMA-Aware Scaling

To scale beyond one CCD, the architecture to pursue is per-CCD sharding:

```
CCD 0 (cores 0-7)            CCD 1 (cores 8-15)
┌──────────────────┐         ┌──────────────────┐
│ listener (SO_REUSEPORT)    │ listener (SO_REUSEPORT)
│ async runtime     │         │ async runtime     │
│ primary cache     │         │ primary cache     │
│ secondary cache   │         │ secondary cache   │
│ tertiary cache    │         │ tertiary cache    │
└────────┬─────────┘         └────────┬─────────┘
         └──── shared storage / origin ────┘
                   (async I/O, latency-tolerant)
```

Each CCD is a self-contained shard with its own cache instances. Affinity via consistent hashing on request/connection ID ensures clients always hit the same CCD. The only cross-CCD communication is I/O to the shared origin. This eliminates the 30% penalty seen when threads span CCDs.

**Recommendation: start with one CCD and scale out only when profiling shows it's needed.** 8 cores on one CCD is sufficient for most deployments.

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

All tests pass: `cargo test -p ttl_hash_map`

---

## Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `dashmap` | 6.1.0 | Concurrent hash map (features: `inline`, `raw-api`, `rayon`) |
| `rayon` | 1.x | Parallel iteration for cleanup |
| `rand` | 0.8 | Random key generation in stress tests (feature: `small_rng`) |
| `criterion` | 0.5 | Microbenchmarks (dev-dependency) |

---

## Running

```bash
# Unit tests
cargo test -p ttl_hash_map

# Stress test (mixed read/write, 32/64/128 reader configs)
cargo run --release -p ttl_hash_map --bin stress_test

# Criterion microbenchmark
cargo bench -p ttl_hash_map --bench contention

# Pinned to one CCD (Linux)
taskset -c 0-7 cargo run --release -p ttl_hash_map --bin stress_test
```
