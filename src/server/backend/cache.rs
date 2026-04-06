use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use lru::LruCache;
use std::num::NonZeroUsize;

#[derive(Clone, Debug)]
pub struct CachedEntry {
    pub payload: Vec<u8>,
    pub compressed: bool,
    pub expires_at: Option<u64>,
}

pub struct ReadThroughCache {
    inner: Mutex<LruCache<String, CachedEntry>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ReadThroughCache {
    pub fn new(max_entries: usize) -> Self {
        let cap = NonZeroUsize::new(max_entries).expect("cache capacity must be > 0");
        Self {
            inner: Mutex::new(LruCache::new(cap)),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn get(&self, key: &str) -> Option<CachedEntry> {
        let mut cache = self.inner.lock().unwrap();
        if let Some(entry) = cache.get(key) {
            if let Some(exp) = entry.expires_at {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                if now >= exp {
                    cache.pop(key);
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            }
            let result = entry.clone();
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(result)
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    pub fn put(&self, key: String, entry: CachedEntry) {
        let mut cache = self.inner.lock().unwrap();
        cache.put(key, entry);
    }

    pub fn evict(&self, key: &str) {
        let mut cache = self.inner.lock().unwrap();
        cache.pop(key);
    }

    pub fn purge_expired(&self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut cache = self.inner.lock().unwrap();
        let expired_keys: Vec<String> = cache
            .iter()
            .filter_map(|(k, v)| {
                if let Some(exp) = v.expires_at {
                    if now >= exp {
                        return Some(k.clone());
                    }
                }
                None
            })
            .collect();

        for key in expired_keys {
            cache.pop(&key);
        }
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_and_get() {
        let cache = ReadThroughCache::new(10);
        cache.put(
            "key1".to_string(),
            CachedEntry {
                payload: b"value1".to_vec(),
                compressed: false,
                expires_at: None,
            },
        );

        let entry = cache.get("key1").unwrap();
        assert_eq!(entry.payload, b"value1");
        assert!(!entry.compressed);
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 0);
    }

    #[test]
    fn get_miss() {
        let cache = ReadThroughCache::new(10);
        assert!(cache.get("nonexistent").is_none());
        assert_eq!(cache.hits(), 0);
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn evict_removes_entry() {
        let cache = ReadThroughCache::new(10);
        cache.put(
            "key1".to_string(),
            CachedEntry {
                payload: b"val".to_vec(),
                compressed: false,
                expires_at: None,
            },
        );
        cache.evict("key1");
        assert!(cache.get("key1").is_none());
    }

    #[test]
    fn lru_eviction_on_capacity() {
        let cache = ReadThroughCache::new(2);
        cache.put(
            "a".to_string(),
            CachedEntry {
                payload: vec![],
                compressed: false,
                expires_at: None,
            },
        );
        cache.put(
            "b".to_string(),
            CachedEntry {
                payload: vec![],
                compressed: false,
                expires_at: None,
            },
        );
        cache.put(
            "c".to_string(),
            CachedEntry {
                payload: vec![],
                compressed: false,
                expires_at: None,
            },
        );

        // "a" should have been evicted (LRU)
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn expired_entry_returns_none() {
        let cache = ReadThroughCache::new(10);
        cache.put(
            "exp".to_string(),
            CachedEntry {
                payload: b"data".to_vec(),
                compressed: false,
                expires_at: Some(1), // already expired (epoch second 1)
            },
        );

        assert!(cache.get("exp").is_none());
        assert_eq!(cache.misses(), 1);
    }

    #[test]
    fn purge_expired_removes_stale_entries() {
        let cache = ReadThroughCache::new(10);
        cache.put(
            "expired".to_string(),
            CachedEntry {
                payload: vec![],
                compressed: false,
                expires_at: Some(1), // long past
            },
        );
        cache.put(
            "alive".to_string(),
            CachedEntry {
                payload: vec![],
                compressed: false,
                expires_at: None,
            },
        );

        cache.purge_expired();
        assert_eq!(cache.len(), 1);
        assert!(cache.get("alive").is_some());
    }

    #[test]
    fn hit_miss_counters_accumulate() {
        let cache = ReadThroughCache::new(10);
        cache.put(
            "k".to_string(),
            CachedEntry {
                payload: vec![],
                compressed: false,
                expires_at: None,
            },
        );

        cache.get("k"); // hit
        cache.get("k"); // hit
        cache.get("missing"); // miss

        assert_eq!(cache.hits(), 2);
        assert_eq!(cache.misses(), 1);
    }
}
