use std::{
    io::{Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dashmap::DashMap;
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::Serialize;

use crate::{config::StoreConfig, server::error::Result};

#[derive(Clone)]
pub struct Store {
    data: Arc<DashMap<String, ValueEntry>>,
    compression_threshold: usize,
    default_ttl: Option<u64>,
    cleaner: Arc<CleanerState>,
    cleanup_interval_ms: u64,
    started_at: std::time::Instant,
}

struct ValueEntry {
    payload: Vec<u8>,
    expires_at: Option<u64>,
    compressed: bool,
}

struct CleanerState {
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Store {
    pub fn new() -> Self {
        Self::with_config(StoreConfig::default())
    }

    pub fn with_config(config: StoreConfig) -> Self {
        let data = Arc::new(DashMap::new());
        let interval = Duration::from_millis(config.cleanup_interval_ms);
        let cleaner = CleanerState::spawn(Arc::clone(&data), interval);

        Self {
            data,
            compression_threshold: config.compression_threshold,
            default_ttl: config.default_ttl_secs,
            cleaner: Arc::new(cleaner),
            cleanup_interval_ms: config.cleanup_interval_ms,
            started_at: std::time::Instant::now(),
        }
    }

    pub fn insert(&self, key: String, value: Vec<u8>, seconds: u64) -> Result<()> {
        let expires_at = self.ttl_deadline(seconds)?;
        let (payload, compressed) = compress_if_needed(&value, self.compression_threshold)?;

        self.data.insert(
            key,
            ValueEntry {
                payload,
                expires_at,
                compressed,
            },
        );
        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let now = current_epoch_seconds()?;
        if let Some(entry) = self.data.get(key) {
            if entry.is_expired(now) {
                drop(entry);
                self.data.remove(key);
                return Ok(None);
            }

            let data = decompress_if_needed(&entry.payload, entry.compressed)?;
            return Ok(Some(data));
        }
        Ok(None)
    }

    pub fn delete(&self, key: &str) -> Result<Option<String>> {
        let now = current_epoch_seconds()?;
        if let Some(entry) = self.data.get(key) {
            if entry.is_expired(now) {
                drop(entry);
                self.data.remove(key);
                return Ok(None);
            }
        }

        match self.data.remove(key) {
            Some((removed_key, entry)) => {
                if entry.is_expired(now) {
                    return Ok(None);
                }
                Ok(Some(removed_key))
            }
            None => Ok(None),
        }
    }

    pub fn expires_in(&self, key: &str) -> Result<Option<u64>> {
        let now = current_epoch_seconds()?;

        if let Some(entry) = self.data.get(key) {
            match entry.expires_at {
                Some(expiry) if now < expiry => Ok(Some(expiry - now)),
                Some(_) => {
                    drop(entry);
                    self.data.remove(key);
                    Ok(None)
                }
                None => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn stats(&self) -> StoreStats {
        let keys = self.data.len();
        let compressed_keys = self.data.iter().filter(|entry| entry.compressed).count();
        let uptime_secs = self.started_at.elapsed().as_secs_f64();

        StoreStats {
            keys,
            compressed_keys,
            compression_threshold: self.compression_threshold,
            default_ttl_secs: self.default_ttl,
            cleanup_interval_ms: self.cleanup_interval_ms,
            uptime_secs,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_compressed(&self, key: &str) -> Option<bool> {
        self.data.get(key).map(|entry| entry.compressed)
    }

    fn ttl_deadline(&self, seconds: u64) -> Result<Option<u64>> {
        let ttl = if seconds == 0 {
            self.default_ttl.unwrap_or(0)
        } else {
            seconds
        };

        if ttl == 0 {
            return Ok(None);
        }

        Ok(Some(current_epoch_seconds()? + ttl))
    }
}

impl CleanerState {
    fn spawn(data: Arc<DashMap<String, ValueEntry>>, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_signal = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            while !stop_signal.load(Ordering::Relaxed) {
                purge_expired(&data);
                thread::sleep(interval);
            }
            purge_expired(&data);
        });

        Self {
            stop,
            handle: Mutex::new(Some(handle)),
        }
    }

    fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.handle.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for CleanerState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl ValueEntry {
    fn is_expired(&self, now: u64) -> bool {
        match self.expires_at {
            Some(expiry) => now >= expiry,
            None => false,
        }
    }
}

fn purge_expired(data: &DashMap<String, ValueEntry>) {
    if let Ok(now) = current_epoch_seconds() {
        data.retain(|_, value| !value.is_expired(now));
    }
}

fn current_epoch_seconds() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn compress_if_needed(value: &[u8], threshold: usize) -> Result<(Vec<u8>, bool)> {
    if value.len() < threshold {
        return Ok((value.to_vec(), false));
    }

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(value)?;
    let compressed = encoder.finish()?;

    if compressed.len() < value.len() {
        Ok((compressed, true))
    } else {
        Ok((value.to_vec(), false))
    }
}

fn decompress_if_needed(value: &[u8], compressed: bool) -> Result<Vec<u8>> {
    if !compressed {
        return Ok(value.to_vec());
    }

    let mut decoder = GzDecoder::new(value);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;
    Ok(decompressed)
}

#[derive(Debug, Serialize)]
pub struct StoreStats {
    pub keys: usize,
    pub compressed_keys: usize,
    pub compression_threshold: usize,
    pub default_ttl_secs: Option<u64>,
    pub cleanup_interval_ms: u64,
    pub uptime_secs: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn insert_and_get_without_expire() -> Result<()> {
        let store = Store::new();
        store.insert("a".to_string(), b"b".to_vec(), 0)?;
        assert_eq!(store.get("a")?, Some(b"b".to_vec()));
        Ok(())
    }

    #[test]
    fn value_expires() -> Result<()> {
        let store = Store::new();
        store.insert("a".to_string(), b"b".to_vec(), 1)?;
        thread::sleep(Duration::from_secs(2));
        assert_eq!(store.get("a")?, None);
        Ok(())
    }

    #[test]
    fn delete_and_expires_in_behaviour() -> Result<()> {
        let store = Store::new();
        store.insert("a".to_string(), b"b".to_vec(), 0)?;
        assert_eq!(store.delete("a")?, Some("a".to_string()));
        assert_eq!(store.get("a")?, None);

        store.insert("b".to_string(), b"c".to_vec(), 1)?;
        if let Some(ttl) = store.expires_in("b")? {
            assert!(ttl <= 1);
        } else {
            panic!("expected ttl");
        }
        thread::sleep(Duration::from_secs(2));
        assert_eq!(store.delete("b")?, None);
        assert_eq!(store.expires_in("b")?, None);
        assert_eq!(store.get("b")?, None);
        Ok(())
    }

    #[test]
    fn delete_before_expiration_removes_value() -> Result<()> {
        let store = Store::new();
        store.insert("a".to_string(), b"b".to_vec(), 10)?;
        assert_eq!(store.delete("a")?, Some("a".to_string()));
        assert_eq!(store.get("a")?, None);
        Ok(())
    }

    #[test]
    fn large_values_are_compressed() -> Result<()> {
        let store = Store::new();
        let threshold = StoreConfig::default().compression_threshold;
        let large = vec![b'a'; threshold * 4];
        store.insert("big".to_string(), large.clone(), 0)?;
        assert_eq!(store.is_compressed("big"), Some(true));
        assert_eq!(store.get("big")?, Some(large));
        Ok(())
    }

    #[test]
    fn small_values_stay_uncompressed() -> Result<()> {
        let store = Store::new();
        store.insert("tiny".to_string(), b"hi".to_vec(), 0)?;
        assert_eq!(store.is_compressed("tiny"), Some(false));
        assert_eq!(store.get("tiny")?, Some(b"hi".to_vec()));
        Ok(())
    }

    #[test]
    fn stats_reflects_store_state() -> Result<()> {
        let store = Store::new();
        store.insert("a".to_string(), b"value".to_vec(), 0)?;
        let stats = store.stats();
        assert_eq!(stats.keys, 1);
        assert_eq!(
            stats.compression_threshold,
            StoreConfig::default().compression_threshold
        );
        assert_eq!(
            stats.cleanup_interval_ms,
            StoreConfig::default().cleanup_interval_ms
        );
        assert!(stats.uptime_secs >= 0.0);
        Ok(())
    }

    #[test]
    fn background_worker_purges_expired_keys() -> Result<()> {
        let store = Store::with_config(StoreConfig {
            compression_threshold: StoreConfig::default().compression_threshold,
            cleanup_interval_ms: 50,
            default_ttl_secs: Some(1),
        });
        store.insert("temp".to_string(), b"value".to_vec(), 1)?;
        thread::sleep(Duration::from_secs(3));
        assert_eq!(store.get("temp")?, None);
        assert_eq!(store.len(), 0);
        Ok(())
    }

    #[test]
    fn default_ttl_applies_when_zero() -> Result<()> {
        let store = Store::with_config(StoreConfig {
            compression_threshold: StoreConfig::default().compression_threshold,
            cleanup_interval_ms: StoreConfig::default().cleanup_interval_ms,
            default_ttl_secs: Some(1),
        });
        store.insert("ttl".to_string(), b"value".to_vec(), 0)?;
        thread::sleep(Duration::from_secs(2));
        assert_eq!(store.get("ttl")?, None);
        Ok(())
    }
}
