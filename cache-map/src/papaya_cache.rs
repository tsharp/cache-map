use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::cache::{CacheConfiguration, CacheError, CacheMap, EvictionListener, Result};
use crate::clock::CoarseClock;

/// A value wrapper that tracks when the entry expires.
struct CacheEntry<V> {
    value: V,
    expires_at_ms: u64,
}

/// A concurrent hash map with per-entry TTL expiration, backed by `papaya`.
///
/// Uses `ResizeMode::Blocking` and pre-allocates capacity for high read
/// throughput.  Guards are acquired once per `CacheMap` method call through
/// `pin()` so the guard cost is amortised across the operation.
pub struct PapayaCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    inner: papaya::HashMap<K, CacheEntry<V>>,
    default_ttl: Duration,
    max_capacity: usize,
    on_evict: Option<EvictionListener<K, V>>,
    clock: Arc<CoarseClock>,
    ticker_stop: Arc<AtomicBool>,
}

impl<K, V> PapayaCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn from_config(config: CacheConfiguration<K, V>) -> Self {
        let capacity = config.get_initial_capacity()
            .or(config.get_max_capacity())
            .unwrap_or(1024);

        let inner = papaya::HashMap::builder()
            .capacity(capacity)
            .resize_mode(papaya::ResizeMode::Blocking)
            .build();

        let clock = Arc::new(CoarseClock::new());
        clock.tick();

        let ticker_stop = Arc::new(AtomicBool::new(false));
        let ticker_clock = Arc::clone(&clock);
        let ticker_flag = Arc::clone(&ticker_stop);
        thread::Builder::new()
            .name("cache-tick".into())
            .spawn(move || {
                while !ticker_flag.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(1));
                    ticker_clock.tick();
                }
            })
            .expect("failed to spawn ticker thread");

        Self {
            inner,
            default_ttl: config.get_default_ttl().unwrap_or(Duration::from_secs(300)),
            max_capacity: config.get_max_capacity().unwrap_or(0),
            on_evict: config.get_on_evict().cloned(),
            clock,
            ticker_stop,
        }
    }

    /// Fire the eviction callback if one is configured.
    #[inline]
    fn notify_evict(&self, key: K, value: V) {
        if let Some(ref cb) = self.on_evict {
            cb(key, value);
        }
    }

    /// Evict the entry if it is expired, returning `true` if it was removed.
    fn evict_if_expired(&self, key: &K) -> bool {
        let pin = self.inner.pin();
        if let Some(entry) = pin.get(key) {
            if self.clock.is_expired(entry.expires_at_ms) {
                if let Some((_k, entry)) = pin.remove_entry(key) {
                    self.notify_evict(key.clone(), entry.value.clone());
                }
                return true;
            }
        }
        false
    }
}

impl<K, V> CacheMap<K, V> for PapayaCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn insert(&self, key: K, value: V) -> Result {
      self.insert_with_ttl(key, value, self.default_ttl)
    }

    fn insert_with_ttl(&self, key: K, value: V, ttl: Duration) -> Result {
        if self.max_capacity > 0 && self.inner.len() >= self.max_capacity {
            return Err(CacheError::MaxCapacityReached);
        }

        let expires_at_ms = self.clock.expire_at_ms(ttl);
        let pin = self.inner.pin();
        pin.insert(key, CacheEntry { value, expires_at_ms });
        Ok(())
    }

    fn get(&self, key: &K) -> Option<V> {
        let pin = self.inner.pin();
        let entry = pin.get(key)?;
        if self.clock.is_expired(entry.expires_at_ms) {
            if let Some((_k, removed)) = pin.remove_entry(key) {
                self.notify_evict(key.clone(), removed.value.clone());
            }
            return None;
        }
        Some(entry.value.clone())
    }

    fn evict(&self, key: &K) -> Option<V> {
        let pin = self.inner.pin();
        pin.remove_entry(key).map(|(_k, entry)| {
            let val = entry.value.clone();
            self.notify_evict(key.clone(), val.clone());
            val
        })
    }

    fn refresh(&self, key: &K) -> bool {
        let pin = self.inner.pin();
        if let Some(entry) = pin.get(key) {
            if self.clock.is_expired(entry.expires_at_ms) {
                pin.remove(key);
                return false;
            }
            let expires_at_ms = self.clock.expire_at_ms(self.default_ttl);
            pin.insert(key.clone(), CacheEntry { value: entry.value.clone(), expires_at_ms });
            true
        } else {
            false
        }
    }

    fn cleanup(&self) {
        let pin = self.inner.pin();

        let expired_keys: Vec<K> = pin
            .iter()
            .filter(|(_k, entry)| self.clock.is_expired(entry.expires_at_ms))
            .map(|(k, _)| k.clone())
            .collect();

        for key in expired_keys {
            if let Some((_k, entry)) = pin.remove_entry(&key) {
                self.notify_evict(key, entry.value.clone());
            }
        }
    }

    fn contains_key(&self, key: &K) -> bool {
        self.evict_if_expired(key);
        self.inner.pin().contains_key(key)
    }

    fn len(&self) -> usize {
        self.inner.len()
    }

    fn clear(&self) {
        self.inner.pin().clear();
    }
    
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<K, V> Drop for PapayaCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn drop(&mut self) {
        self.ticker_stop.store(true, Ordering::Release);
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
        let map: PapayaCache<String, i32> = PapayaCache::from_config(config);
        
        map.insert("a".to_string(), 1).unwrap();
        assert_eq!(map.get(&"a".to_string()), Some(1));
    }

    #[test]
    fn expired_entry_returns_none() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_millis(50));
        let map: PapayaCache<String, i32> = PapayaCache::from_config(config);
        map.insert("a".to_string(), 1).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(map.get(&"a".to_string()), None);
    }

    #[test]
    fn insert_with_custom_ttl() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_secs(60));
        let map: PapayaCache<String, i32> = PapayaCache::from_config(config);
        map.insert_with_ttl("a".to_string(), 1, Duration::from_millis(50)).unwrap();
        assert_eq!(map.get(&"a".to_string()), Some(1));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(map.get(&"a".to_string()), None);
    }

    #[test]
    fn evict_returns_value() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_secs(60));
        let map: PapayaCache<String, i32> = PapayaCache::from_config(config);
        map.insert("a".to_string(), 42).unwrap();
        assert_eq!(map.evict(&"a".to_string()), Some(42));
        assert_eq!(map.get(&"a".to_string()), None);
    }

    #[test]
    fn refresh_extends_ttl() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_millis(150));
        let map: PapayaCache<String, i32> = PapayaCache::from_config(config);
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
        let map: PapayaCache<String, i32> = PapayaCache::from_config(config);
        map.insert("a".to_string(), 1).unwrap();
        assert!(map.contains_key(&"a".to_string()));
        thread::sleep(Duration::from_millis(100));
        assert!(!map.contains_key(&"a".to_string()));
    }

    #[test]
    fn cleanup_removes_expired() {
        let config = CacheConfiguration::new()
            .set_default_ttl(Duration::from_millis(50));
        let map: PapayaCache<String, i32> = PapayaCache::from_config(config);
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
        let map: PapayaCache<String, i32> = PapayaCache::from_config(config);
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
        let map: PapayaCache<String, i32> = PapayaCache::from_config(config);
        map.insert("a".to_string(), 1).unwrap();
        map.insert("b".to_string(), 2).unwrap();
        thread::sleep(Duration::from_millis(100));
        map.cleanup();
        assert_eq!(evict_count.load(Ordering::Relaxed), 2);
    }
}
