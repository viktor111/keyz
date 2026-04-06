use std::{
    io::{Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::Serialize;

use crate::{
    config::{BackendType, StoreConfig},
    server::error::Result,
};

use super::backend::disk::DiskBackend;
use super::backend::memory::InMemoryBackend;
use super::backend::StorageBackend;

#[derive(Clone)]
pub struct Store {
    backend: Arc<dyn StorageBackend>,
    compression_threshold: usize,
    default_ttl: Option<u64>,
    cleaner: Arc<CleanerState>,
    cleanup_interval_ms: u64,
    started_at: std::time::Instant,
}

struct CleanerState {
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Store {
    pub fn new() -> Self {
        Self::with_config(StoreConfig::default()).expect("default config should always work")
    }

    pub fn with_config(config: StoreConfig) -> Result<Self> {
        let backend: Arc<dyn StorageBackend> = match config.backend {
            BackendType::Memory => Arc::new(InMemoryBackend::new()),
            BackendType::Disk => {
                let encryption_key = config.disk.resolve_encryption_key()?;
                let disk = DiskBackend::new(
                    config.disk.data_dir.clone().into(),
                    config.disk.partition_count,
                    encryption_key,
                    config.disk.cache_max_entries,
                    config.disk.flush_interval_ms,
                )?;
                Arc::new(disk)
            }
        };

        let interval = Duration::from_millis(config.cleanup_interval_ms);
        let cleaner = CleanerState::spawn(Arc::clone(&backend), interval);

        Ok(Self {
            backend,
            compression_threshold: config.compression_threshold,
            default_ttl: config.default_ttl_secs,
            cleaner: Arc::new(cleaner),
            cleanup_interval_ms: config.cleanup_interval_ms,
            started_at: std::time::Instant::now(),
        })
    }

    pub fn insert(&self, key: String, value: Vec<u8>, seconds: u64) -> Result<()> {
        let expires_at = self.ttl_deadline(seconds)?;
        let (payload, compressed) = compress_if_needed(&value, self.compression_threshold)?;
        self.backend.insert(key, payload, expires_at, compressed)
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self.backend.get(key)? {
            Some((data, compressed)) => {
                let value = decompress_if_needed(&data, compressed)?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    pub fn delete(&self, key: &str) -> Result<Option<String>> {
        self.backend.delete(key)
    }

    pub fn expires_in(&self, key: &str) -> Result<Option<u64>> {
        self.backend.expires_in(key)
    }

    pub fn len(&self) -> usize {
        self.backend.len()
    }

    pub fn stats(&self) -> StoreStats {
        let backend_stats = self.backend.stats_extra();
        let uptime_secs = self.started_at.elapsed().as_secs_f64();

        StoreStats {
            keys: backend_stats.keys,
            compressed_keys: backend_stats.compressed_keys,
            compression_threshold: self.compression_threshold,
            default_ttl_secs: self.default_ttl,
            cleanup_interval_ms: self.cleanup_interval_ms,
            uptime_secs,
            backend_type: backend_stats.backend_type,
            cache_hits: backend_stats.cache_hits,
            cache_misses: backend_stats.cache_misses,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_compressed(&self, key: &str) -> Option<bool> {
        self.backend
            .as_any()
            .downcast_ref::<InMemoryBackend>()
            .and_then(|mem| mem.is_compressed(key))
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
    fn spawn(backend: Arc<dyn StorageBackend>, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_signal = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            while !stop_signal.load(Ordering::Relaxed) {
                let _ = backend.purge_expired();
                thread::sleep(interval);
            }
            let _ = backend.purge_expired();
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

fn current_epoch_seconds() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

pub(crate) fn compress_if_needed(value: &[u8], threshold: usize) -> Result<(Vec<u8>, bool)> {
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

pub(crate) fn decompress_if_needed(value: &[u8], compressed: bool) -> Result<Vec<u8>> {
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
    pub backend_type: String,
    pub cache_hits: u64,
    pub cache_misses: u64,
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
            ..StoreConfig::default()
        })?;
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
            ..StoreConfig::default()
        })?;
        store.insert("ttl".to_string(), b"value".to_vec(), 0)?;
        thread::sleep(Duration::from_secs(2));
        assert_eq!(store.get("ttl")?, None);
        Ok(())
    }

    #[test]
    fn disk_backend_through_store() -> Result<()> {
        use crate::config::{BackendType, DiskConfig};

        let dir = std::env::temp_dir().join(format!("keyz_store_test_{}", rand::random::<u64>()));

        let config = StoreConfig {
            backend: BackendType::Disk,
            disk: DiskConfig {
                data_dir: dir.to_string_lossy().to_string(),
                partition_count: 4,
                encryption_enabled: false,
                encryption_key: None,
                cache_max_entries: 100,
                flush_interval_ms: 60000,
            },
            ..StoreConfig::default()
        };

        let store = Store::with_config(config)?;

        // Basic CRUD through Store facade
        store.insert("key1".to_string(), b"value1".to_vec(), 0)?;
        assert_eq!(store.get("key1")?, Some(b"value1".to_vec()));

        store.insert("key2".to_string(), b"value2".to_vec(), 0)?;
        assert_eq!(store.len(), 2);

        assert_eq!(store.delete("key1")?, Some("key1".to_string()));
        assert_eq!(store.get("key1")?, None);
        assert_eq!(store.len(), 1);

        // Compression through Store facade
        let large = vec![b'x'; 2048];
        store.insert("big".to_string(), large.clone(), 0)?;
        assert_eq!(store.get("big")?, Some(large));

        // Stats should report disk backend
        let stats = store.stats();
        assert_eq!(stats.backend_type, "disk");
        assert!(stats.keys >= 2);

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn disk_backend_persistence_through_store() -> Result<()> {
        use crate::config::{BackendType, DiskConfig};

        let dir = std::env::temp_dir().join(format!("keyz_persist_test_{}", rand::random::<u64>()));

        let disk_config = DiskConfig {
            data_dir: dir.to_string_lossy().to_string(),
            partition_count: 4,
            encryption_enabled: false,
            encryption_key: None,
            cache_max_entries: 0,
            flush_interval_ms: 60000,
        };

        // Write data
        {
            let store = Store::with_config(StoreConfig {
                backend: BackendType::Disk,
                disk: disk_config.clone(),
                ..StoreConfig::default()
            })?;
            store.insert("persist_key".to_string(), b"persist_val".to_vec(), 0)?;
            // Drop triggers flush
        }

        // Reload and verify
        {
            let store = Store::with_config(StoreConfig {
                backend: BackendType::Disk,
                disk: disk_config,
                ..StoreConfig::default()
            })?;
            assert_eq!(store.get("persist_key")?, Some(b"persist_val".to_vec()));
        }

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
