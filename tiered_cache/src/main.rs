use std::time::Duration;
use cache_map::cache::{CacheConfiguration, CacheMap};
use cache_map::dash_cache::DashCache;

fn main() {
    let config = CacheConfiguration::new()
        .set_default_ttl(Duration::from_secs(60))
        .set_max_capacity(100);

    let cache: DashCache<String, String> = DashCache::from_config(config);

    cache.insert("hello".into(), "world".into()).unwrap();
    println!("get(\"hello\") = {:?}", cache.get(&"hello".into()));
}
