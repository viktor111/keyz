use std::{
    io::{Read, Write},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::Serialize;

use crate::{
    config::{BackendType, ReplicationRole, StoreConfig},
    server::error::{KeyzError, Result},
};

use super::backend::aof::AofRecord;
use super::backend::disk::DiskBackend;
use super::backend::memory::InMemoryBackend;
use super::backend::{SnapshotEntry, StorageBackend};
use super::health::Health;
use super::replication::leader::{FollowerRegistry, ReplicationBroadcaster};
use super::replication::{MAX_REPLICATION_FRAME, REPLICATION_SET_OVERHEAD};

#[derive(Clone)]
pub struct Store {
    backend: Arc<dyn StorageBackend>,
    compression_threshold: usize,
    default_ttl: Option<u64>,
    cleaner: Arc<CleanerState>,
    cleanup_interval_ms: u64,
    started_at: std::time::Instant,
    broadcaster: Option<Arc<ReplicationBroadcaster>>,
    replication_role: ReplicationRole,
    follower_registry: Option<Arc<FollowerRegistry>>,
    mutation_order: Arc<parking_lot::Mutex<()>>,
    max_bytes: u64,
    health: Health,
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

struct CleanerState {
    signal: Arc<(Mutex<bool>, Condvar)>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
    maintenance: Arc<parking_lot::Mutex<MaintenanceHealth>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MaintenanceHealth {
    pub cleanup_failures: u64,
    pub flush_failures: u64,
    pub sync_failures: u64,
    pub operation_failures: u64,
    pub last_cleanup_error: Option<String>,
    pub last_flush_error: Option<String>,
    pub last_sync_error: Option<String>,
    pub last_operation_error: Option<String>,
    #[serde(skip)]
    operation_errors: std::collections::BTreeMap<&'static str, (u64, String)>,
}

impl MaintenanceHealth {
    pub fn healthy(&self) -> bool {
        self.last_cleanup_error.is_none()
            && self.last_flush_error.is_none()
            && self.last_sync_error.is_none()
            && self.last_operation_error.is_none()
    }

    fn record(&mut self, operation: &str, result: &Result<()>) {
        let (count, last_error) = match operation {
            "cleanup" => (&mut self.cleanup_failures, &mut self.last_cleanup_error),
            "flush" => (&mut self.flush_failures, &mut self.last_flush_error),
            _ => (&mut self.sync_failures, &mut self.last_sync_error),
        };
        match result {
            Ok(()) => *last_error = None,
            Err(error) => {
                *count += 1;
                let error = error.to_string();
                eprintln!(
                    "{}",
                    serde_json::json!({"level":"error", "component":"storage", "operation":operation, "error":error})
                );
                *last_error = Some(error);
            }
        }
    }
}

impl Store {
    pub fn new() -> Self {
        Self::with_config(StoreConfig::default()).expect("default config should always work")
    }

    pub fn with_config(config: StoreConfig) -> Result<Self> {
        config.validate()?;
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
                    config.disk.aof_enabled,
                    config.disk.aof_fsync.clone(),
                )?;
                Arc::new(disk)
            }
        };

        backend.set_max_bytes(config.max_bytes)?;
        let interval = Duration::from_millis(config.cleanup_interval_ms.max(1));
        let health = Health::default();
        let cleaner = CleanerState::spawn(Arc::clone(&backend), interval, health.clone())?;

        Ok(Self {
            backend,
            compression_threshold: config.compression_threshold,
            default_ttl: config.default_ttl_secs,
            cleaner: Arc::new(cleaner),
            cleanup_interval_ms: config.cleanup_interval_ms,
            started_at: std::time::Instant::now(),
            broadcaster: None,
            replication_role: ReplicationRole::Disabled,
            follower_registry: None,
            mutation_order: Arc::new(parking_lot::Mutex::new(())),
            max_bytes: config.max_bytes,
            health,
        })
    }

    pub fn set_broadcaster(&mut self, broadcaster: Arc<ReplicationBroadcaster>) {
        self.broadcaster = Some(broadcaster);
    }

    pub fn set_replication_role(&mut self, role: ReplicationRole) {
        self.replication_role = role;
    }

    pub fn set_follower_registry(&mut self, registry: Arc<FollowerRegistry>) {
        self.follower_registry = Some(registry);
    }

    pub fn replication_info(&self) -> serde_json::Value {
        let role = match self.replication_role {
            ReplicationRole::Disabled => "disabled",
            ReplicationRole::Leader => "leader",
            ReplicationRole::Follower => "follower",
        };

        let mut info = serde_json::json!({ "role": role });

        if let Some(ref registry) = self.follower_registry {
            let followers: Vec<serde_json::Value> = registry
                .list()
                .into_iter()
                .map(|(addr, secs)| {
                    serde_json::json!({
                        "address": addr.to_string(),
                        "connected_secs": format!("{:.1}", secs),
                    })
                })
                .collect();
            info["followers"] = serde_json::json!(followers);
        }

        info
    }

    pub fn insert(&self, key: String, value: Vec<u8>, seconds: u64) -> Result<()> {
        self.insert_with_ttl(key, value, (seconds != 0).then_some(seconds))
    }

    /// `None` selects the configured default; `Some(0)` disables expiration.
    pub fn insert_with_ttl(
        &self,
        key: String,
        value: Vec<u8>,
        ttl_secs: Option<u64>,
    ) -> Result<()> {
        let expires_at = self.ttl_deadline(ttl_secs)?;
        let (payload, compressed) = compress_if_needed(&value, self.compression_threshold)?;
        if self.replication_role == ReplicationRole::Leader || self.broadcaster.is_some() {
            validate_replication_set_size(key.len(), payload.len())?;
        }
        // Publishing while still holding this lock preserves backend commit order.
        let _mutation = self.mutation_order.lock();
        self.observe_operation(
            "insert",
            self.backend
                .insert(key.clone(), payload.clone(), expires_at, compressed),
        )?;

        if let Some(ref broadcaster) = self.broadcaster {
            broadcaster.broadcast(&AofRecord::Set {
                key,
                value: payload,
                expires_at: expires_at.unwrap_or(0),
                compressed,
            });
        }

        Ok(())
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let result = self.backend.get(key).and_then(|entry| {
            entry
                .map(|(data, compressed)| decompress_if_needed(&data, compressed))
                .transpose()
        });
        self.observe_operation("get", result)
    }

    pub fn delete(&self, key: &str) -> Result<Option<String>> {
        let _mutation = self.mutation_order.lock();
        let result = self.observe_operation("delete", self.backend.delete(key))?;

        if result.is_some() {
            if let Some(ref broadcaster) = self.broadcaster {
                broadcaster.broadcast(&AofRecord::Del {
                    key: key.to_string(),
                });
            }
        }

        Ok(result)
    }

    pub(crate) fn replication_snapshot(
        &self,
        broadcaster: &ReplicationBroadcaster,
    ) -> (
        Vec<SnapshotEntry>,
        tokio::sync::broadcast::Receiver<Vec<u8>>,
    ) {
        // No mutation can commit between capturing the dataset and subscribing.
        // Consequently every live record follows the state in this snapshot.
        let _mutation = self.mutation_order.lock();
        let entries = self.backend.snapshot_entries();
        let receiver = broadcaster.subscribe();
        (entries, receiver)
    }

    /// Check recovered data before starting a leader. This uses a consistent
    /// snapshot because the backend's public snapshot API owns its payloads.
    pub fn validate_replication_entries(&self) -> Result<()> {
        let _mutation = self.mutation_order.lock();
        for (key, value, _, _) in self.backend.snapshot_entries() {
            validate_replication_set_size(key.len(), value.len())?;
        }
        Ok(())
    }

    pub(crate) fn replace_replicated_snapshot(&self, entries: Vec<SnapshotEntry>) -> Result<()> {
        let _mutation = self.mutation_order.lock();
        if self.replication_role == ReplicationRole::Leader || self.broadcaster.is_some() {
            for (key, value, _, _) in &entries {
                validate_replication_set_size(key.len(), value.len())?;
            }
        }
        self.observe_operation("replace_snapshot", self.backend.replace_entries(entries))
    }

    pub fn apply_replicated_insert(
        &self,
        key: String,
        value: Vec<u8>,
        expires_at: u64,
        compressed: bool,
    ) -> Result<()> {
        if self.replication_role == ReplicationRole::Leader || self.broadcaster.is_some() {
            validate_replication_set_size(key.len(), value.len())?;
        }
        let _mutation = self.mutation_order.lock();
        let exp = if expires_at == 0 {
            None
        } else {
            Some(expires_at)
        };
        self.observe_operation(
            "replicated_insert",
            self.backend.insert(key, value, exp, compressed),
        )
    }

    pub fn expires_in(&self, key: &str) -> Result<Option<u64>> {
        self.observe_operation("expires_in", self.backend.expires_in(key))
    }

    pub fn len(&self) -> usize {
        self.backend.len()
    }

    pub fn is_empty(&self) -> bool {
        self.backend.is_empty()
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
            used_bytes: self.backend.used_bytes(),
            max_bytes: self.max_bytes,
            maintenance: self.cleaner.maintenance.lock().clone(),
        }
    }

    pub fn health(&self) -> Health {
        self.health.clone()
    }

    /// Called by the storage worker after all admitted requests are complete.
    pub fn shutdown(&self) -> Result<()> {
        self.health.stopping();
        self.cleaner.shutdown()?;
        self.flush()
    }

    pub fn flush(&self) -> Result<()> {
        let result = self.backend.flush();
        let mut maintenance = self.cleaner.maintenance.lock();
        maintenance.record("flush", &result);
        self.health.set_storage_healthy(maintenance.healthy());
        result
    }

    fn observe_operation<T>(&self, operation: &'static str, result: Result<T>) -> Result<T> {
        let mut status = self.cleaner.maintenance.lock();
        match &result {
            Err(
                KeyzError::Io(_)
                | KeyzError::Backend(_)
                | KeyzError::CorruptDataFile(_)
                | KeyzError::Encryption(_)
                | KeyzError::Time(_),
            ) => {
                let error = result.as_ref().err().unwrap().to_string();
                status.operation_failures += 1;
                let message = format!("{operation}: {error}");
                let sequence = status.operation_failures;
                status
                    .operation_errors
                    .insert(operation, (sequence, message.clone()));
                status.last_operation_error = Some(message);
                eprintln!(
                    "{}",
                    serde_json::json!({"level":"error", "component":"storage", "operation":operation, "error":error})
                );
            }
            Ok(_) => {
                if status.operation_errors.remove(operation).is_some() {
                    status.last_operation_error = status
                        .operation_errors
                        .values()
                        .max_by_key(|(sequence, _)| sequence)
                        .map(|(_, message)| message.clone());
                }
            }
            _ => {}
        }
        self.health.set_storage_healthy(status.healthy());
        result
    }

    #[cfg(test)]
    pub(crate) fn is_compressed(&self, key: &str) -> Option<bool> {
        self.backend
            .as_any()
            .downcast_ref::<InMemoryBackend>()
            .and_then(|mem| mem.is_compressed(key))
    }

    fn ttl_deadline(&self, seconds: Option<u64>) -> Result<Option<u64>> {
        let ttl = seconds.unwrap_or_else(|| self.default_ttl.unwrap_or(0));

        if ttl == 0 {
            return Ok(None);
        }

        let deadline = current_epoch_seconds()?
            .checked_add(ttl)
            .ok_or_else(|| KeyzError::InvalidCommand("expiration is too large".into()))?;
        Ok(Some(deadline))
    }
}

fn validate_replication_set_size(key_bytes: usize, payload_bytes: usize) -> Result<()> {
    let encoded_bytes = key_bytes
        .checked_add(payload_bytes)
        .and_then(|bytes| bytes.checked_add(REPLICATION_SET_OVERHEAD));
    if encoded_bytes.is_none_or(|bytes| bytes > MAX_REPLICATION_FRAME) {
        return Err(KeyzError::InvalidCommand(format!(
            "stored key and value exceed the replication record limit of {MAX_REPLICATION_FRAME} bytes"
        )));
    }
    Ok(())
}

impl CleanerState {
    fn spawn(backend: Arc<dyn StorageBackend>, interval: Duration, health: Health) -> Result<Self> {
        let signal = Arc::new((Mutex::new(false), Condvar::new()));
        let thread_signal = Arc::clone(&signal);
        let maintenance = Arc::new(parking_lot::Mutex::new(MaintenanceHealth::default()));
        let thread_maintenance = Arc::clone(&maintenance);

        let handle = thread::Builder::new()
            .name("keyz-maintenance".into())
            .spawn(move || {
                let checkpoint_interval = backend.checkpoint_interval();
                let mut cleanup_due = Instant::now();
                let mut flush_due = checkpoint_interval.map(|interval| Instant::now() + interval);
                let mut sync_due = Instant::now() + Duration::from_secs(1);
                let (stop, wake) = &*thread_signal;
                loop {
                    if *stop.lock().unwrap_or_else(|error| error.into_inner()) {
                        break;
                    }
                    let now = Instant::now();
                    if now >= cleanup_due {
                        let result = backend.purge_expired();
                        let mut status = thread_maintenance.lock();
                        status.record("cleanup", &result);
                        health.set_storage_healthy(status.healthy());
                        cleanup_due = Instant::now() + interval;
                    }
                    if flush_due.is_some_and(|due| now >= due) {
                        let result = backend.flush();
                        let mut status = thread_maintenance.lock();
                        status.record("flush", &result);
                        health.set_storage_healthy(status.healthy());
                        flush_due = checkpoint_interval.map(|interval| Instant::now() + interval);
                    }
                    if now >= sync_due {
                        let result = backend.sync_log();
                        let mut status = thread_maintenance.lock();
                        status.record("sync", &result);
                        health.set_storage_healthy(status.healthy());
                        sync_due = Instant::now() + Duration::from_secs(1);
                    }
                    let due = cleanup_due
                        .min(sync_due)
                        .min(flush_due.unwrap_or(cleanup_due));
                    let stopped = stop.lock().unwrap_or_else(|error| error.into_inner());
                    if *stopped {
                        break;
                    }
                    let _ =
                        wake.wait_timeout(stopped, due.saturating_duration_since(Instant::now()));
                }
            })?;

        Ok(Self {
            signal,
            handle: Mutex::new(Some(handle)),
            maintenance,
        })
    }

    fn shutdown(&self) -> Result<()> {
        let (stop, wake) = &*self.signal;
        *stop.lock().unwrap_or_else(|error| error.into_inner()) = true;
        wake.notify_all();
        let mut guard = self
            .handle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(handle) = guard.take() {
            handle
                .join()
                .map_err(|_| KeyzError::Backend("storage maintenance thread panicked".into()))?;
        }
        Ok(())
    }
}

impl Drop for CleanerState {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!(
                "{}",
                serde_json::json!({"level":"error", "component":"storage", "operation":"shutdown", "error":error.to_string()})
            );
        }
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
    pub used_bytes: u64,
    pub max_bytes: u64,
    pub maintenance: MaintenanceHealth,
}

#[cfg(test)]
mod tests {
    use super::super::backend::BackendStats;
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn replication_size_checks_match_wire_format_and_exact_boundaries() {
        let key = "ключ";
        let value = b"binary\0value";
        let encoded = AofRecord::Set {
            key: key.into(),
            value: value.to_vec(),
            expires_at: 123,
            compressed: false,
        }
        .serialize();
        assert_eq!(
            encoded.len(),
            key.len() + value.len() + REPLICATION_SET_OVERHEAD
        );
        let payload_limit = MAX_REPLICATION_FRAME - REPLICATION_SET_OVERHEAD - key.len();
        assert!(validate_replication_set_size(key.len(), payload_limit).is_ok());
        assert!(matches!(
            validate_replication_set_size(key.len(), payload_limit + 1),
            Err(KeyzError::InvalidCommand(_))
        ));
        assert!(validate_replication_set_size(usize::MAX, 1).is_err());
        assert!(validate_replication_set_size(0, usize::MAX).is_err());
    }

    #[test]
    fn byte_budget_is_atomic_for_overwrite_delete_and_replication_on_both_backends() -> Result<()> {
        for backend in [BackendType::Memory, BackendType::Disk] {
            let dir = std::env::temp_dir().join(format!("keyz_budget_{}", rand::random::<u64>()));
            let store = Store::with_config(StoreConfig {
                backend,
                max_bytes: 8,
                compression_threshold: 1024,
                disk: crate::config::DiskConfig {
                    data_dir: dir.to_string_lossy().into(),
                    ..Default::default()
                },
                ..Default::default()
            })?;
            store.insert("key".into(), b"value".to_vec(), 0)?;
            assert_eq!(store.stats().used_bytes, 8);
            assert!(matches!(
                store.insert("key".into(), b"longer".to_vec(), 0),
                Err(KeyzError::CapacityExceeded { .. })
            ));
            assert_eq!(store.get("key")?, Some(b"value".to_vec()));
            store.insert("key".into(), b"v".to_vec(), 0)?;
            assert_eq!(store.stats().used_bytes, 4);
            store.apply_replicated_insert("new".into(), vec![1], 0, false)?;
            assert_eq!(store.stats().used_bytes, 8);
            assert!(matches!(
                store.apply_replicated_insert("extra".into(), vec![1], 0, false),
                Err(KeyzError::CapacityExceeded { .. })
            ));
            assert!(matches!(
                store.replace_replicated_snapshot(vec![("oversized".into(), vec![0], None, false)]),
                Err(KeyzError::CapacityExceeded { .. })
            ));
            assert_eq!(store.len(), 2);
            store.delete("new")?;
            assert_eq!(store.stats().used_bytes, 4);
            // Duplicated snapshot keys are counted once, with the last value.
            store.replace_replicated_snapshot(vec![
                ("a".into(), vec![0; 20], None, false),
                ("a".into(), vec![1], None, false),
            ])?;
            assert_eq!(store.stats().used_bytes, 2);
            store.apply_replicated_insert("exp".into(), vec![2], 1, false)?;
            store.backend.purge_expired()?;
            assert_eq!(store.stats().used_bytes, 2);
            store.shutdown()?;
            drop(store);
            if dir.exists() {
                std::fs::remove_dir_all(dir)?;
            }
        }
        Ok(())
    }

    #[test]
    fn concurrent_inserts_never_exceed_the_byte_budget() -> Result<()> {
        let store = Store::with_config(StoreConfig {
            max_bytes: 12,
            ..Default::default()
        })?;
        let barrier = Arc::new(std::sync::Barrier::new(17));
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let store = store.clone();
                let start = Arc::clone(&barrier);
                thread::spawn(move || {
                    start.wait();
                    store.insert(format!("{i:02}"), vec![b'x'; 10], 0)
                })
            })
            .collect();
        barrier.wait();
        let accepted = threads
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(Result::is_ok)
            .count();
        assert_eq!(accepted, 1);
        assert_eq!(store.stats().used_bytes, 12);
        Ok(())
    }

    #[test]
    fn budget_counts_compressed_payload_and_explicit_zero_ttl_skips_default() -> Result<()> {
        let store = Store::with_config(StoreConfig {
            max_bytes: 100,
            default_ttl_secs: Some(60),
            ..Default::default()
        })?;
        store.insert_with_ttl("compressed".into(), vec![b'a'; 4096], Some(0))?;
        assert!(store.stats().used_bytes < 100);
        assert_eq!(store.expires_in("compressed")?, None);
        store.insert_with_ttl("default".into(), vec![b'b'], None)?;
        assert!(store.expires_in("default")?.unwrap() <= 60);
        Ok(())
    }

    #[test]
    fn shutdown_wakes_long_maintenance_wait_and_flushes_without_drop() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("keyz_shutdown_{}", rand::random::<u64>()));
        let store = Store::with_config(StoreConfig {
            backend: BackendType::Disk,
            cleanup_interval_ms: 86_400_000,
            disk: crate::config::DiskConfig {
                data_dir: dir.to_string_lossy().into(),
                partition_count: 16,
                aof_enabled: false,
                flush_interval_ms: 86_400_000,
                ..Default::default()
            },
            ..Default::default()
        })?;
        store.insert("durable".into(), b"value".to_vec(), 0)?;
        let started = Instant::now();
        store.shutdown()?;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(dir.join("data.keyz").exists());
        // Inspect persisted entries while retaining the owner: shutdown, not
        // Drop, must have written them. A second backend now correctly requires
        // releasing the exclusive directory lock first.
        let data = std::fs::read(dir.join("data.keyz"))?;
        let header = crate::server::backend::format::FileHeader::deserialize(&data)?;
        let table = crate::server::backend::format::validate_partition_table(&data, &header)?;
        let mut persisted = Vec::new();
        for row in table {
            let start = row.offset as usize;
            persisted.extend(
                crate::server::backend::format::deserialize_partition_entries_counted(
                    &data[start..start + row.length as usize],
                    row.entry_count,
                )?,
            );
        }
        assert!(persisted
            .iter()
            .any(|entry| entry.key == "durable" && entry.value == b"value"));
        drop(store);
        let loaded = DiskBackend::new(
            dir.clone(),
            16,
            None,
            0,
            60_000,
            false,
            crate::config::AofFsyncPolicy::EverySecond,
        )?;
        assert_eq!(loaded.get("durable")?, Some((b"value".to_vec(), false)));
        drop(loaded);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    struct MaintenanceBackend {
        inner: InMemoryBackend,
        fail_cleanup: std::sync::atomic::AtomicBool,
        fail_write: std::sync::atomic::AtomicBool,
        fail_get: std::sync::atomic::AtomicBool,
        cleanup_runs: std::sync::atomic::AtomicU64,
        flush_runs: std::sync::atomic::AtomicU64,
        sync_runs: std::sync::atomic::AtomicU64,
    }

    impl StorageBackend for MaintenanceBackend {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn insert(
            &self,
            key: String,
            value: Vec<u8>,
            expiry: Option<u64>,
            compressed: bool,
        ) -> Result<()> {
            if self.fail_write.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(KeyzError::Backend("injected write failure".into()));
            }
            self.inner.insert(key, value, expiry, compressed)
        }
        fn get(&self, key: &str) -> Result<Option<(Vec<u8>, bool)>> {
            if self.fail_get.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(KeyzError::Backend("injected read failure".into()));
            }
            self.inner.get(key)
        }
        fn delete(&self, key: &str) -> Result<Option<String>> {
            self.inner.delete(key)
        }
        fn expires_in(&self, key: &str) -> Result<Option<u64>> {
            self.inner.expires_in(key)
        }
        fn len(&self) -> usize {
            self.inner.len()
        }
        fn stats_extra(&self) -> BackendStats {
            self.inner.stats_extra()
        }
        fn snapshot_entries(&self) -> Vec<SnapshotEntry> {
            self.inner.snapshot_entries()
        }
        fn replace_entries(&self, entries: Vec<SnapshotEntry>) -> Result<()> {
            self.inner.replace_entries(entries)
        }
        fn purge_expired(&self) -> Result<()> {
            use std::sync::atomic::Ordering::Relaxed;
            self.cleanup_runs.fetch_add(1, Relaxed);
            if self.fail_cleanup.load(Relaxed) {
                Err(KeyzError::Backend("injected cleanup failure".into()))
            } else {
                Ok(())
            }
        }
        fn checkpoint_interval(&self) -> Option<Duration> {
            Some(Duration::from_millis(15))
        }
        fn flush(&self) -> Result<()> {
            self.flush_runs
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        fn sync_log(&self) -> Result<()> {
            self.sync_runs
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn maintenance_schedules_checkpoints_independently_and_reports_cleanup_recovery() -> Result<()>
    {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
        let backend = Arc::new(MaintenanceBackend {
            inner: InMemoryBackend::new(),
            fail_cleanup: AtomicBool::new(true),
            fail_write: AtomicBool::new(false),
            fail_get: AtomicBool::new(false),
            cleanup_runs: AtomicU64::new(0),
            flush_runs: AtomicU64::new(0),
            sync_runs: AtomicU64::new(0),
        });
        let health = Health::default();
        let cleaner =
            CleanerState::spawn(backend.clone(), Duration::from_millis(400), health.clone())?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while backend.flush_runs.load(Relaxed) < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(backend.flush_runs.load(Relaxed) >= 2);
        assert_eq!(backend.cleanup_runs.load(Relaxed), 1);
        assert_eq!(cleaner.maintenance.lock().cleanup_failures, 1);
        assert!(!cleaner.maintenance.lock().healthy());
        backend.fail_cleanup.store(false, Relaxed);
        while (backend.cleanup_runs.load(Relaxed) < 2 || backend.sync_runs.load(Relaxed) == 0)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        cleaner.shutdown()?;
        let status = cleaner.maintenance.lock();
        assert_eq!(status.cleanup_failures, 1);
        assert!(status.healthy());
        assert!(backend.sync_runs.load(Relaxed) >= 1);
        Ok(())
    }

    #[test]
    fn operation_failures_degrade_health_until_retry_but_capacity_rejection_does_not() -> Result<()>
    {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
        let backend = Arc::new(MaintenanceBackend {
            inner: InMemoryBackend::new(),
            fail_cleanup: AtomicBool::new(false),
            fail_write: AtomicBool::new(true),
            fail_get: AtomicBool::new(false),
            cleanup_runs: AtomicU64::new(0),
            flush_runs: AtomicU64::new(0),
            sync_runs: AtomicU64::new(0),
        });
        backend.inner.set_max_bytes(4)?;
        let mut store = Store::new();
        store.backend = backend.clone();
        assert!(store.insert("k".into(), b"v".to_vec(), 0).is_err());
        assert!(!store.health().snapshot().storage_healthy);
        assert_eq!(store.stats().maintenance.operation_failures, 1);
        store.flush()?;
        assert!(!store.health().snapshot().storage_healthy);
        backend.fail_write.store(false, Relaxed);
        assert!(matches!(
            store.insert("k".into(), b"oversized".to_vec(), 0),
            Err(KeyzError::CapacityExceeded { .. })
        ));
        assert!(!store.health().snapshot().storage_healthy);
        store.insert("k".into(), b"v".to_vec(), 0)?;
        assert!(store.health().snapshot().storage_healthy);
        assert!(matches!(
            store.insert("k".into(), b"oversized".to_vec(), 0),
            Err(KeyzError::CapacityExceeded { .. })
        ));
        assert!(store.health().snapshot().storage_healthy);
        assert_eq!(store.stats().maintenance.operation_failures, 1);
        Ok(())
    }

    #[test]
    fn typed_operation_recovery_preserves_other_unresolved_failures() {
        use crate::protocol::{self, Command, Request, ResponseBody};
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
        let backend = Arc::new(MaintenanceBackend {
            inner: InMemoryBackend::new(),
            fail_cleanup: AtomicBool::new(false),
            fail_write: AtomicBool::new(true),
            fail_get: AtomicBool::new(true),
            cleanup_runs: AtomicU64::new(0),
            flush_runs: AtomicU64::new(0),
            sync_runs: AtomicU64::new(0),
        });
        let mut store = Store::new();
        store.backend = backend.clone();
        let dispatch = |command| {
            let raw = protocol::encode_request(&Request { id: 42, command });
            let response = super::super::dispatcher::dispatch_request(
                raw,
                &store,
                &crate::config::ProtocolConfig::default(),
                false,
            )
            .unwrap();
            protocol::decode_response(&response).unwrap().result
        };
        let set = || Command::Set {
            key: "k".into(),
            value: b"v".to_vec(),
            ttl_secs: Some(0),
        };
        let get = || Command::Get { key: "k".into() };
        assert!(matches!(dispatch(set()), ResponseBody::Error { .. }));
        assert!(matches!(dispatch(get()), ResponseBody::Error { .. }));
        assert_eq!(store.stats().maintenance.operation_failures, 2);
        backend.fail_get.store(false, Relaxed);
        assert!(matches!(dispatch(get()), ResponseBody::Missing));
        assert!(!store.health().snapshot().storage_healthy);
        assert!(store
            .stats()
            .maintenance
            .last_operation_error
            .unwrap()
            .starts_with("insert:"));
        backend.fail_write.store(false, Relaxed);
        assert!(matches!(dispatch(set()), ResponseBody::Ok));
        assert!(store.health().snapshot().storage_healthy);
        assert!(store.stats().maintenance.last_operation_error.is_none());
    }

    #[test]
    fn disk_flush_interval_is_independent_of_cleanup_and_failures_are_observable() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("keyz_checkpoint_{}", rand::random::<u64>()));
        let store = Store::with_config(StoreConfig {
            backend: BackendType::Disk,
            cleanup_interval_ms: 86_400_000,
            disk: crate::config::DiskConfig {
                data_dir: dir.to_string_lossy().into(),
                aof_enabled: false,
                flush_interval_ms: 25,
                ..Default::default()
            },
            ..Default::default()
        })?;
        // Make creating the temporary snapshot fail on both Windows and Unix.
        std::fs::create_dir(dir.join("data.keyz.tmp"))?;
        store.insert("scheduled".into(), b"value".to_vec(), 0)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while store.stats().maintenance.flush_failures == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(store.stats().maintenance.flush_failures > 0);
        assert!(store.stats().maintenance.last_flush_error.is_some());
        std::fs::remove_dir(dir.join("data.keyz.tmp"))?;
        while (!dir.join("data.keyz").exists()
            || store.stats().maintenance.last_flush_error.is_some())
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(dir.join("data.keyz").exists());
        assert!(store.stats().maintenance.healthy());
        store.shutdown()?;
        drop(store);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    // Pause the first insertion after the backend commits, before Store publishes it.
    struct PausedInsertBackend {
        inner: InMemoryBackend,
        committed: Mutex<Option<mpsc::Sender<()>>>,
        resume: Mutex<mpsc::Receiver<()>>,
    }

    impl StorageBackend for PausedInsertBackend {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn insert(
            &self,
            key: String,
            value: Vec<u8>,
            expires_at: Option<u64>,
            compressed: bool,
        ) -> Result<()> {
            self.inner.insert(key, value, expires_at, compressed)?;
            let notify = self.committed.lock().unwrap().take();
            if let Some(notify) = notify {
                notify.send(()).unwrap();
                self.resume.lock().unwrap().recv().unwrap();
            }
            Ok(())
        }

        fn get(&self, key: &str) -> Result<Option<(Vec<u8>, bool)>> {
            self.inner.get(key)
        }
        fn delete(&self, key: &str) -> Result<Option<String>> {
            self.inner.delete(key)
        }
        fn expires_in(&self, key: &str) -> Result<Option<u64>> {
            self.inner.expires_in(key)
        }
        fn len(&self) -> usize {
            self.inner.len()
        }
        fn purge_expired(&self) -> Result<()> {
            self.inner.purge_expired()
        }
        fn stats_extra(&self) -> BackendStats {
            self.inner.stats_extra()
        }
        fn snapshot_entries(&self) -> Vec<SnapshotEntry> {
            self.inner.snapshot_entries()
        }
        fn replace_entries(&self, entries: Vec<SnapshotEntry>) -> Result<()> {
            self.inner.replace_entries(entries)
        }
    }

    fn paused_store() -> (
        Store,
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
        Arc<ReplicationBroadcaster>,
    ) {
        let mut store = Store::with_config(StoreConfig {
            cleanup_interval_ms: 1,
            ..StoreConfig::default()
        })
        .unwrap();
        let (committed_tx, committed_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        store.backend = Arc::new(PausedInsertBackend {
            inner: InMemoryBackend::new(),
            committed: Mutex::new(Some(committed_tx)),
            resume: Mutex::new(resume_rx),
        });
        let broadcaster = Arc::new(ReplicationBroadcaster::new());
        store.set_broadcaster(Arc::clone(&broadcaster));
        (store, committed_rx, resume_tx, broadcaster)
    }

    fn broadcast_record(frame: Vec<u8>) -> AofRecord {
        let mut cursor = std::io::Cursor::new(&frame[4..]);
        AofRecord::deserialize(&mut cursor).unwrap().unwrap()
    }

    #[test]
    fn replication_publishes_concurrent_mutations_in_commit_order() {
        let (store, committed, resume, broadcaster) = paused_store();
        let mut receiver = broadcaster.subscribe();
        let writer_store = store.clone();
        let writer = thread::spawn(move || writer_store.insert("key".into(), b"value".to_vec(), 0));
        committed.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(store.get("key").unwrap(), Some(b"value".to_vec()));

        let delete_store = store.clone();
        let (finished_tx, finished_rx) = mpsc::channel();
        let deleter = thread::spawn(move || {
            let deleted = delete_store.delete("key");
            finished_tx.send(()).unwrap();
            deleted
        });
        // A second mutation cannot pass the paused commit's pending publication.
        let blocked = finished_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err();
        resume.send(()).unwrap();
        writer.join().unwrap().unwrap();
        assert_eq!(deleter.join().unwrap().unwrap(), Some("key".into()));
        assert!(blocked);
        assert!(matches!(
            broadcast_record(receiver.try_recv().unwrap()),
            AofRecord::Set { .. }
        ));
        assert!(matches!(
            broadcast_record(receiver.try_recv().unwrap()),
            AofRecord::Del { .. }
        ));
        assert_eq!(store.get("key").unwrap(), None);
    }

    #[test]
    fn snapshot_subscription_waits_for_pending_publication() {
        let (store, committed, resume, broadcaster) = paused_store();
        let writer_store = store.clone();
        let writer =
            thread::spawn(move || writer_store.insert("key".into(), b"before".to_vec(), 0));
        committed.recv_timeout(Duration::from_secs(2)).unwrap();
        let snapshot_store = store.clone();
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        let snapshotter = thread::spawn(move || {
            snapshot_tx
                .send(snapshot_store.replication_snapshot(&broadcaster))
                .unwrap()
        });
        let blocked = snapshot_rx
            .recv_timeout(Duration::from_millis(100))
            .is_err();
        resume.send(()).unwrap();
        writer.join().unwrap().unwrap();
        snapshotter.join().unwrap();
        assert!(blocked);
        let (entries, mut receiver) = snapshot_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            entries,
            vec![("key".into(), b"before".to_vec(), None, false)]
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        store.insert("key".into(), b"after".to_vec(), 0).unwrap();
        match broadcast_record(receiver.try_recv().unwrap()) {
            AofRecord::Set { value, .. } => assert_eq!(value, b"after"),
            _ => panic!("expected the mutation following the snapshot"),
        }
    }

    #[test]
    fn overflowing_expiration_is_rejected_without_changing_existing_value() -> Result<()> {
        let store = Store::new();
        store.insert("key".into(), b"old".to_vec(), 0)?;
        assert!(matches!(
            store.insert("key".into(), b"new".to_vec(), u64::MAX),
            Err(KeyzError::InvalidCommand(_))
        ));
        assert_eq!(store.get("key")?, Some(b"old".to_vec()));
        let defaults = Store::with_config(StoreConfig {
            default_ttl_secs: Some(u64::MAX),
            ..StoreConfig::default()
        })?;
        assert!(matches!(
            defaults.insert("key".into(), b"value".to_vec(), 0),
            Err(KeyzError::InvalidCommand(_))
        ));
        assert_eq!(defaults.get("key")?, None);
        Ok(())
    }

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
                cache_max_entries: 100,
                aof_enabled: false,
                ..DiskConfig::default()
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

        drop(store);
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
            cache_max_entries: 0,
            aof_enabled: false,
            ..DiskConfig::default()
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
