use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

/// Type alias for the eviction callback.
pub type EvictionListener<K, V> = Arc<dyn Fn(K, V) + Send + Sync>;

/// Errors that can occur when interacting with a cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheError {
    MaxCapacityReached,
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::MaxCapacityReached => write!(f, "cache has reached its maximum capacity"),
        }
    }
}

impl std::error::Error for CacheError {}

pub type Result<T = ()> = std::result::Result<T, CacheError>;

pub struct CacheConfiguration<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Default TTL for entries in the cache. Can be overridden per-entry.
    default_ttl: Option<Duration>,

    /// Maximum number of entries in the cache. If `None`, the cache can grow without bound.
    max_capacity: Option<usize>,

    /// Initial capacity of the cache. If `None`, a default value is used.
    initial_capacity: Option<usize>,

    /// Interval at which the cache performs cleanup of expired entries.
    cleanup_interval: Option<Duration>,

    /// Number of shards for the underlying concurrent map.
    /// Higher values reduce contention at the cost of slightly more memory.
    /// Defaults to `num_cpus * 4` if not set.
    shard_count: Option<usize>,

    /// Optional callback invoked when an entry is evicted (expired or removed).
    on_evict: Option<EvictionListener<K, V>>,
}

impl<K, V> CacheConfiguration<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        CacheConfiguration {
            default_ttl: None,
            max_capacity: None,
            initial_capacity: None,
            cleanup_interval: None,
            shard_count: None,
            on_evict: None,
        }
    }

    pub fn set_default_ttl(mut self, ttl: Duration) -> Self {
        self.default_ttl = Some(ttl);
        self
    }

    pub fn set_max_capacity(mut self, capacity: usize) -> Self {
        self.max_capacity = Some(capacity);
        self
    }

    pub fn set_initial_capacity(mut self, capacity: usize) -> Self {
        self.initial_capacity = Some(capacity);
        self
    }

    pub fn set_cleanup_interval(mut self, interval: Duration) -> Self {
        self.cleanup_interval = Some(interval);
        self
    }

    /// Set the number of shards for the underlying concurrent map.
    /// Defaults to `num_cpus * 4` if not set.
    /// A good starting point for high-core-count machines is `num_cpus * 8`.
    pub fn set_shard_count(mut self, count: usize) -> Self {
        self.shard_count = Some(count);
        self
    }

    /// Set a callback that fires when an entry is evicted.
    /// Replaces any previously set callback.
    pub fn set_on_evict(mut self, listener: impl Fn(K, V) + Send + Sync + 'static) -> Self {
        self.on_evict = Some(Arc::new(listener));
        self
    }

    pub fn get_default_ttl(&self) -> Option<Duration> {
        self.default_ttl
    }

    pub fn get_max_capacity(&self) -> Option<usize> {
        self.max_capacity
    }

    pub fn get_initial_capacity(&self) -> Option<usize> {
        self.initial_capacity
    }

    pub fn get_cleanup_interval(&self) -> Option<Duration> {
        self.cleanup_interval
    }

    pub fn get_shard_count(&self) -> Option<usize> {
        self.shard_count
    }

    pub fn get_on_evict(&self) -> Option<&EvictionListener<K, V>> {
        self.on_evict.as_ref()
    }
}

/// Trait for a concurrent key-value map with TTL-based expiration.
pub trait CacheMap<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Insert a value with the default TTL.
    fn insert(&self, key: K, value: V) -> Result;

    /// Insert a value with a custom TTL override.
    fn insert_with_ttl(&self, key: K, value: V, ttl: Duration) -> Result;

    /// Get a cloned value for the given key, if it exists and has not expired.
    fn get(&self, key: &K) -> Option<V>;

    /// Evict a key immediately, returning the value if it existed.
    fn evict(&self, key: &K) -> Option<V>;

    /// Refresh the TTL of an existing key, returning `true` if the key was present and refreshed.
    fn refresh(&self, key: &K) -> bool;

    /// Check if a key exists and is not expired.
    fn contains_key(&self, key: &K) -> bool;

    /// Returns the total number of entries.
    fn len(&self) -> usize;

    /// Returns `true` if the map contains no entries.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove all entries.
    fn clear(&self);
}
