use std::hash::Hash;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};

use crate::cache::{CacheConfiguration, CacheError, CacheMap, EvictionListener, Result};

/// A value wrapper that tracks when the entry expires.
struct CacheEntry<V> {
    value: V,
    expires_at: Instant
}

impl<V> CacheEntry<V> {
    fn new(value: V, ttl: Duration) -> Self {
        Self {
            value: value,
            expires_at: Instant::now() + ttl
        }
    }

    #[inline]
    #[must_use]
    fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

/// A concurrent hash map with per-entry TTL expiration, backed by `DashMap`.
pub struct DashCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    inner: DashMap<K, CacheEntry<V>>,
    default_ttl: Duration,
    max_capacity: usize,
    use_retain_cleanup: bool,
    on_evict: Option<EvictionListener<K, V>>,    
}

impl<K, V> DashCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn from_config(config: CacheConfiguration<K, V>) -> Self {
        let capacity = config.get_initial_capacity().unwrap_or(1024);
        let inner = match config.get_shard_count() {
            Some(shards) => DashMap::with_capacity_and_shard_amount(capacity, shards),
            None => DashMap::with_capacity(capacity),
        };

        let has_evict = config.get_on_evict().is_some();
        let use_retain = !has_evict && std::thread::available_parallelism()
            .map(|p| p.get() <= 8)
            .unwrap_or(true);

        Self {
            inner,
            default_ttl: config.get_default_ttl().unwrap_or(Duration::from_secs(300)),
            max_capacity: config.get_max_capacity().unwrap_or(0),
            use_retain_cleanup: use_retain,
            on_evict: config.get_on_evict().cloned(),
        }
    }

    /// Fire the eviction callback if one is configured.
    #[inline]
    fn notify_evict(&self, key: K, value: V) {
        if let Some(ref cb) = self.on_evict {
            cb(key, value);
        }
    }

    /// Remove all expired entries in parallel.
    pub fn cleanup(&self) {
        if self.use_retain_cleanup {
            self.inner.retain(|_, entry| !entry.is_expired());
            return;
        }

        let expired_keys: Vec<K> = self
            .inner
            .par_iter()
            .filter(|entry| entry.value().is_expired())
            .map(|entry| entry.key().clone())
            .collect();

        for key in expired_keys {
            if let Some((k, entry)) = self.inner.remove(&key) {
                self.notify_evict(k, entry.value);
            }
        }
    }

    /// Evict the entry if it is expired, returning `true` if it was removed.
    fn evict_if_expired(&self, key: &K) -> bool {
        if let Some(entry) = self.inner.get(key) {
            if entry.is_expired() {
                drop(entry); // release read lock before removing
                if let Some((k, entry)) = self.inner.remove(key) {
                    self.notify_evict(k, entry.value);
                }
                return true;
            }
        }
        false
    }
}

impl<K, V> CacheMap<K, V> for DashCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn insert(&self, key: K, value: V) -> Result {
      self.insert_with_ttl(key, value, self.default_ttl)
    }

    fn insert_with_ttl(&self, key: K, value: V, ttl: Duration) -> Result {
        if self.max_capacity > 0 && self.inner.len() >= self.max_capacity {
            // return an error
            return Err(CacheError::MaxCapacityReached);
        }

        self.inner.insert(key, CacheEntry::new(value, ttl));
        Ok(())
    }

    fn get(&self, key: &K) -> Option<V> {
        let entry = self.inner.get(key)?;
        if entry.is_expired() {
            drop(entry);
            if let Some((k, entry)) = self.inner.remove(key) {
                self.notify_evict(k, entry.value);
            }
            return None;
        }
        Some(entry.value.clone())
    }

    fn evict(&self, key: &K) -> Option<V> {
        self.inner.remove(key).map(|(k, entry)| {
            self.notify_evict(k, entry.value.clone());
            entry.value
        })
    }

    fn refresh(&self, key: &K) -> bool {
        if let Some(mut entry) = self.inner.get_mut(key) {
            if entry.is_expired() {
                drop(entry);
                self.inner.remove(key);
                return false;
            }
            entry.expires_at = Instant::now() + self.default_ttl;
            true
        } else {
            false
        }
    }

    fn contains_key(&self, key: &K) -> bool {
        self.evict_if_expired(key);
        self.inner.contains_key(key)
    }

    fn len(&self) -> usize {
        self.inner.len()
    }

    fn clear(&self) {
        self.inner.clear();
    }
    
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn insert_and_get() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_secs(60));
        let map: DashCache<String, i32> = DashCache::from_config(config);
        
        map.insert("a".to_string(), 1).unwrap();
        assert_eq!(map.get(&"a".to_string()), Some(1));
    }

    #[test]
    fn expired_entry_returns_none() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_millis(50));
        let map: DashCache<String, i32> = DashCache::from_config(config);
        map.insert("a".to_string(), 1).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(map.get(&"a".to_string()), None);
    }

    #[test]
    fn insert_with_custom_ttl() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_secs(60));
        let map: DashCache<String, i32> = DashCache::from_config(config);
        map.insert_with_ttl("a".to_string(), 1, Duration::from_millis(50)).unwrap();
        assert_eq!(map.get(&"a".to_string()), Some(1));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(map.get(&"a".to_string()), None);
    }

    #[test]
    fn evict_returns_value() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_secs(60));
        let map: DashCache<String, i32> = DashCache::from_config(config);
        map.insert("a".to_string(), 42).unwrap();
        assert_eq!(map.evict(&"a".to_string()), Some(42));
        assert_eq!(map.get(&"a".to_string()), None);
    }

    #[test]
    fn refresh_extends_ttl() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_millis(150));
        let map: DashCache<String, i32> = DashCache::from_config(config);
        map.insert("a".to_string(), 1).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(map.refresh(&"a".to_string()));
        thread::sleep(Duration::from_millis(100));
        // Should still be alive because we refreshed
        assert_eq!(map.get(&"a".to_string()), Some(1));
    }

    #[test]
    fn contains_key_respects_expiry() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_millis(50));
        let map: DashCache<String, i32> = DashCache::from_config(config);
        map.insert("a".to_string(), 1).unwrap();
        assert!(map.contains_key(&"a".to_string()));
        thread::sleep(Duration::from_millis(100));
        assert!(!map.contains_key(&"a".to_string()));
    }

    #[test]
    fn cleanup_removes_expired() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_millis(50));
        let map: DashCache<String, i32> = DashCache::from_config(config);
        map.insert("a".to_string(), 1).unwrap();
        map.insert("b".to_string(), 2).unwrap();
        assert_eq!(map.len(), 2);
        thread::sleep(Duration::from_millis(100));
        map.cleanup();
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn clear_removes_all() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_secs(60));
        let map: DashCache<String, i32> = DashCache::from_config(config);
        map.insert("a".to_string(), 1).unwrap();
        map.insert("b".to_string(), 2).unwrap();
        map.clear();
        assert!(map.is_empty());
    }

    #[test]
    fn on_evict_fires_on_expiry() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let evict_count = Arc::new(AtomicUsize::new(0));
        let counter = evict_count.clone();

        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_millis(50))
            .set_on_evict(move |_k: String, _v: i32| {
                counter.fetch_add(1, Ordering::Relaxed);
            });
        let map: DashCache<String, i32> = DashCache::from_config(config);
        map.insert("a".to_string(), 1).unwrap();
        map.insert("b".to_string(), 2).unwrap();
        thread::sleep(Duration::from_millis(100));
        map.cleanup();
        assert_eq!(evict_count.load(Ordering::Relaxed), 2);
    }
}
