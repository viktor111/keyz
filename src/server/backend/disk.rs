use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};

use crate::config::AofFsyncPolicy;
use crate::server::error::{KeyzError, Result};

use super::aof::{AofRecord, AofWriter};
use super::cache::{CachedEntry, ReadThroughCache};
use super::encryption;
use super::format::{
    self, Entry, FileHeader, PartitionTableEntry, COMPRESSION_GZIP, HEADER_SIZE,
    PARTITION_TABLE_ENTRY_SIZE,
};
use super::{aof, BackendStats, StorageBackend};

pub struct DiskBackend {
    dir: PathBuf,
    partition_count: u32,
    partitions: Vec<RwLock<PartitionState>>,
    cache: Option<ReadThroughCache>,
    encryption_key: Option<[u8; 32]>,
    aof: Option<Mutex<AofWriter>>,
    aof_fsync_policy: AofFsyncPolicy,
    aof_last_fsync: AtomicU64,
}

struct PartitionState {
    entries: HashMap<String, DiskValueEntry>,
    dirty: bool,
}

#[derive(Clone)]
struct DiskValueEntry {
    payload: Vec<u8>,
    expires_at: Option<u64>,
    compressed: bool,
}

fn partition_for_key(key: &str, partition_count: u32) -> u32 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() % partition_count as u64) as u32
}

fn current_epoch_seconds() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

impl DiskBackend {
    pub fn new(
        dir: PathBuf,
        partition_count: u32,
        encryption_key: Option<[u8; 32]>,
        cache_max_entries: usize,
        _flush_interval_ms: u64,
        aof_enabled: bool,
        aof_fsync: AofFsyncPolicy,
    ) -> Result<Self> {
        fs::create_dir_all(&dir).map_err(|e| {
            KeyzError::Backend(format!("failed to create data dir {}: {e}", dir.display()))
        })?;

        let mut partitions = Vec::with_capacity(partition_count as usize);
        for _ in 0..partition_count {
            partitions.push(RwLock::new(PartitionState {
                entries: HashMap::new(),
                dirty: false,
            }));
        }

        let cache = if cache_max_entries > 0 {
            Some(ReadThroughCache::new(cache_max_entries))
        } else {
            None
        };

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let mut backend = Self {
            dir,
            partition_count,
            partitions,
            cache,
            encryption_key,
            aof: None,
            aof_fsync_policy: aof_fsync,
            aof_last_fsync: AtomicU64::new(now_ms),
        };

        backend.load_from_disk()?;

        if aof_enabled {
            let aof_path = backend.aof_file_path();

            // Replay existing AOF records
            if aof_path.exists() {
                let records = aof::replay_aof(&aof_path, encryption_key.as_ref())?;
                if !records.is_empty() {
                    backend.apply_aof_records(&records);
                    backend.flush()?;
                }
            }

            let mut writer = AofWriter::new(aof_path, encryption_key)?;
            // Truncate in case we just replayed+flushed, or to start clean
            writer.truncate()?;
            backend.aof = Some(Mutex::new(writer));
        }

        Ok(backend)
    }

    fn data_file_path(&self) -> PathBuf {
        self.dir.join("data.keyz")
    }

    fn temp_file_path(&self) -> PathBuf {
        self.dir.join("data.keyz.tmp")
    }

    fn aof_file_path(&self) -> PathBuf {
        self.dir.join("data.aof")
    }

    fn apply_aof_records(&self, records: &[AofRecord]) {
        for record in records {
            match record {
                AofRecord::Set {
                    key,
                    value,
                    expires_at,
                    compressed,
                } => {
                    let idx = partition_for_key(key, self.partition_count) as usize;
                    let mut partition = self.partitions[idx].write();
                    partition.entries.insert(
                        key.clone(),
                        DiskValueEntry {
                            payload: value.clone(),
                            expires_at: if *expires_at == 0 {
                                None
                            } else {
                                Some(*expires_at)
                            },
                            compressed: *compressed,
                        },
                    );
                    partition.dirty = true;
                }
                AofRecord::Del { key } => {
                    let idx = partition_for_key(key, self.partition_count) as usize;
                    let mut partition = self.partitions[idx].write();
                    partition.entries.remove(key);
                    partition.dirty = true;
                }
            }
        }
    }

    fn load_from_disk(&mut self) -> Result<()> {
        let path = self.data_file_path();
        if !path.exists() {
            return Ok(());
        }

        let data = fs::read(&path).map_err(|e| {
            KeyzError::Backend(format!("failed to read {}: {e}", path.display()))
        })?;

        if data.len() < HEADER_SIZE {
            return Err(KeyzError::CorruptDataFile("file too short".into()));
        }

        let header = FileHeader::deserialize(&data[..HEADER_SIZE])?;

        if header.partition_count != self.partition_count {
            return Err(KeyzError::CorruptDataFile(format!(
                "partition count mismatch: file has {}, config has {}",
                header.partition_count, self.partition_count
            )));
        }

        if header.encryption_enabled && self.encryption_key.is_none() {
            return Err(KeyzError::CorruptDataFile(
                "file is encrypted but no encryption key provided".into(),
            ));
        }

        let table_start = HEADER_SIZE;
        let table_size = PARTITION_TABLE_ENTRY_SIZE * header.partition_count as usize;
        if data.len() < table_start + table_size {
            return Err(KeyzError::CorruptDataFile(
                "file too short for partition table".into(),
            ));
        }

        let mut table_entries = Vec::with_capacity(header.partition_count as usize);
        for i in 0..header.partition_count as usize {
            let offset = table_start + i * PARTITION_TABLE_ENTRY_SIZE;
            let entry = PartitionTableEntry::deserialize(
                &data[offset..offset + PARTITION_TABLE_ENTRY_SIZE],
            )?;
            table_entries.push(entry);
        }

        for (i, te) in table_entries.iter().enumerate() {
            if te.length == 0 {
                continue;
            }

            let start = te.offset as usize;
            let end = start + te.length as usize;
            if end > data.len() {
                return Err(KeyzError::CorruptDataFile(format!(
                    "partition {i} extends past end of file"
                )));
            }

            let partition_data = &data[start..end];

            let plaintext = if header.encryption_enabled {
                let key = self.encryption_key.as_ref().unwrap();
                encryption::decrypt_partition(partition_data, key)?
            } else {
                partition_data.to_vec()
            };

            let entries = format::deserialize_partition_entries(&plaintext)?;

            let mut partition = self.partitions[i].write();
            for entry in entries {
                let expires_at = if entry.expires_at == 0 {
                    None
                } else {
                    Some(entry.expires_at)
                };
                partition.entries.insert(
                    entry.key,
                    DiskValueEntry {
                        payload: entry.value,
                        expires_at,
                        compressed: entry.compressed,
                    },
                );
            }
        }

        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        let encryption_enabled = self.encryption_key.is_some();

        let header = FileHeader {
            partition_count: self.partition_count,
            encryption_enabled,
            compression_algo: COMPRESSION_GZIP,
            encryption_salt: [0; 12],
        };

        let mut file_data = header.serialize();

        // Reserve space for partition table
        let table_start = file_data.len();
        let table_size = PARTITION_TABLE_ENTRY_SIZE * self.partition_count as usize;
        file_data.resize(table_start + table_size, 0);

        let mut table_entries = Vec::with_capacity(self.partition_count as usize);

        for i in 0..self.partition_count as usize {
            let partition = self.partitions[i].read();

            if partition.entries.is_empty() {
                table_entries.push(PartitionTableEntry {
                    offset: 0,
                    length: 0,
                    entry_count: 0,
                });
                continue;
            }

            let entries: Vec<Entry> = partition
                .entries
                .iter()
                .map(|(k, v)| Entry {
                    key: k.clone(),
                    value: v.payload.clone(),
                    expires_at: v.expires_at.unwrap_or(0),
                    compressed: v.compressed,
                })
                .collect();

            let serialized = format::serialize_partition_entries(&entries);

            let partition_bytes = if encryption_enabled {
                let key = self.encryption_key.as_ref().unwrap();
                encryption::encrypt_partition(&serialized, key)?
            } else {
                serialized
            };

            let offset = file_data.len() as u64;
            let length = partition_bytes.len() as u32;

            table_entries.push(PartitionTableEntry {
                offset,
                length,
                entry_count: entries.len() as u32,
            });

            file_data.extend(partition_bytes);
        }

        // Write partition table
        for (i, te) in table_entries.iter().enumerate() {
            let offset = table_start + i * PARTITION_TABLE_ENTRY_SIZE;
            let serialized = te.serialize();
            file_data[offset..offset + PARTITION_TABLE_ENTRY_SIZE].copy_from_slice(&serialized);
        }

        // Write to temp file first, then swap in
        let temp_path = self.temp_file_path();
        let data_path = self.data_file_path();
        let backup_path = self.dir.join("data.keyz.bak");

        fs::write(&temp_path, &file_data).map_err(|e| {
            KeyzError::Backend(format!("failed to write temp file: {e}"))
        })?;

        // Safe swap: rename old → backup, rename temp → data, remove backup.
        // If we crash between steps, either the old file or the new file exists.
        if data_path.exists() {
            fs::rename(&data_path, &backup_path).map_err(|e| {
                KeyzError::Backend(format!("failed to backup old data file: {e}"))
            })?;
        }

        fs::rename(&temp_path, &data_path).map_err(|e| {
            // Try to restore backup if rename failed
            let _ = fs::rename(&backup_path, &data_path);
            KeyzError::Backend(format!("failed to rename temp to data file: {e}"))
        })?;

        // Best-effort cleanup of backup
        let _ = fs::remove_file(&backup_path);

        // Clear dirty flags
        for partition in &self.partitions {
            partition.write().dirty = false;
        }

        // Truncate AOF — snapshot is now authoritative
        if let Some(ref aof) = self.aof {
            aof.lock().truncate()?;
        }

        Ok(())
    }

    fn has_dirty_partitions(&self) -> bool {
        self.partitions.iter().any(|p| p.read().dirty)
    }

}

impl StorageBackend for DiskBackend {
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
        let partition_idx = partition_for_key(&key, self.partition_count) as usize;

        let entry = DiskValueEntry {
            payload: value.clone(),
            expires_at,
            compressed,
        };

        // Lock ordering: AOF mutex → partition RwLock
        if let Some(ref aof) = self.aof {
            let mut writer = aof.lock();
            {
                let mut partition = self.partitions[partition_idx].write();
                partition.entries.insert(key.clone(), entry);
                partition.dirty = true;
            }
            let record = AofRecord::Set {
                key: key.clone(),
                value: value.clone(),
                expires_at: expires_at.unwrap_or(0),
                compressed,
            };
            writer.append(&record)?;
            if self.aof_fsync_policy == AofFsyncPolicy::EveryWrite {
                writer.fsync()?;
            }
        } else {
            let mut partition = self.partitions[partition_idx].write();
            partition.entries.insert(key.clone(), entry);
            partition.dirty = true;
        }

        if let Some(ref cache) = self.cache {
            cache.put(
                key,
                CachedEntry {
                    payload: value,
                    compressed,
                    expires_at,
                },
            );
        }

        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, bool)>> {
        // Check cache first
        if let Some(ref cache) = self.cache {
            if let Some(cached) = cache.get(key) {
                return Ok(Some((cached.payload, cached.compressed)));
            }
        }

        let partition_idx = partition_for_key(key, self.partition_count) as usize;
        let now = current_epoch_seconds()?;

        let partition = self.partitions[partition_idx].read();
        if let Some(entry) = partition.entries.get(key) {
            if let Some(exp) = entry.expires_at {
                if now >= exp {
                    drop(partition);
                    let mut partition = self.partitions[partition_idx].write();
                    partition.entries.remove(key);
                    partition.dirty = true;
                    if let Some(ref cache) = self.cache {
                        cache.evict(key);
                    }
                    return Ok(None);
                }
            }

            let result = (entry.payload.clone(), entry.compressed);

            // Populate cache on miss
            if let Some(ref cache) = self.cache {
                cache.put(
                    key.to_string(),
                    CachedEntry {
                        payload: entry.payload.clone(),
                        compressed: entry.compressed,
                        expires_at: entry.expires_at,
                    },
                );
            }

            return Ok(Some(result));
        }

        Ok(None)
    }

    fn delete(&self, key: &str) -> Result<Option<String>> {
        let partition_idx = partition_for_key(key, self.partition_count) as usize;
        let now = current_epoch_seconds()?;

        // Lock ordering: AOF mutex → partition RwLock
        let (removed, was_live) = if let Some(ref aof) = self.aof {
            let mut writer = aof.lock();
            let mut partition = self.partitions[partition_idx].write();
            match partition.entries.remove(key) {
                Some(entry) => {
                    partition.dirty = true;
                    let live = entry.expires_at.map_or(true, |exp| now < exp);
                    if live {
                        let record = AofRecord::Del {
                            key: key.to_string(),
                        };
                        writer.append(&record)?;
                        if self.aof_fsync_policy == AofFsyncPolicy::EveryWrite {
                            writer.fsync()?;
                        }
                    }
                    (true, live)
                }
                None => (false, false),
            }
        } else {
            let mut partition = self.partitions[partition_idx].write();
            match partition.entries.remove(key) {
                Some(entry) => {
                    partition.dirty = true;
                    let live = entry.expires_at.map_or(true, |exp| now < exp);
                    (true, live)
                }
                None => (false, false),
            }
        };

        if removed {
            if let Some(ref cache) = self.cache {
                cache.evict(key);
            }
            if was_live {
                Ok(Some(key.to_string()))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    fn expires_in(&self, key: &str) -> Result<Option<u64>> {
        let partition_idx = partition_for_key(key, self.partition_count) as usize;
        let now = current_epoch_seconds()?;

        let partition = self.partitions[partition_idx].read();
        if let Some(entry) = partition.entries.get(key) {
            match entry.expires_at {
                Some(expiry) if now < expiry => Ok(Some(expiry - now)),
                Some(_) => {
                    drop(partition);
                    let mut partition = self.partitions[partition_idx].write();
                    partition.entries.remove(key);
                    partition.dirty = true;
                    if let Some(ref cache) = self.cache {
                        cache.evict(key);
                    }
                    Ok(None)
                }
                None => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    fn len(&self) -> usize {
        self.partitions
            .iter()
            .map(|p| p.read().entries.len())
            .sum()
    }

    fn purge_expired(&self) -> Result<()> {
        let now = current_epoch_seconds()?;

        // Periodic AOF fsync for every_second policy
        if self.aof_fsync_policy == AofFsyncPolicy::EverySecond {
            if let Some(ref aof) = self.aof {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last = self.aof_last_fsync.load(Ordering::Relaxed);
                if now_ms.saturating_sub(last) >= 1000 {
                    aof.lock().fsync()?;
                    self.aof_last_fsync.store(now_ms, Ordering::Relaxed);
                }
            }
        }

        for partition in &self.partitions {
            let mut p = partition.write();
            let before = p.entries.len();
            p.entries.retain(|_, v| {
                if let Some(exp) = v.expires_at {
                    now < exp
                } else {
                    true
                }
            });
            if p.entries.len() != before {
                p.dirty = true;
            }
        }

        if let Some(ref cache) = self.cache {
            cache.purge_expired();
        }

        // Flush dirty partitions to disk (also truncates AOF)
        if self.has_dirty_partitions() {
            self.flush()?;
        }

        Ok(())
    }

    fn stats_extra(&self) -> BackendStats {
        let mut keys = 0;
        let mut compressed_keys = 0;

        for partition in &self.partitions {
            let p = partition.read();
            keys += p.entries.len();
            compressed_keys += p.entries.values().filter(|v| v.compressed).count();
        }

        let (cache_hits, cache_misses) = if let Some(ref cache) = self.cache {
            (cache.hits(), cache.misses())
        } else {
            (0, 0)
        };

        BackendStats {
            keys,
            compressed_keys,
            backend_type: "disk".to_string(),
            cache_hits,
            cache_misses,
        }
    }
}

impl Drop for DiskBackend {
    fn drop(&mut self) {
        if let Err(e) = self.flush() {
            eprintln!("[keyz] WARNING: failed to flush data to disk on shutdown: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keyz_test_{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn future_epoch(secs: u64) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + secs
    }

    #[test]
    fn basic_insert_get_delete() {
        let dir = temp_dir();
        let backend = DiskBackend::new(dir.clone(), 4, None, 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();

        backend
            .insert("k1".into(), b"v1".to_vec(), None, false)
            .unwrap();
        let result = backend.get("k1").unwrap();
        assert_eq!(result, Some((b"v1".to_vec(), false)));

        let deleted = backend.delete("k1").unwrap();
        assert_eq!(deleted, Some("k1".to_string()));
        assert_eq!(backend.get("k1").unwrap(), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistence_across_reload() {
        let dir = temp_dir();

        {
            let backend = DiskBackend::new(dir.clone(), 4, None, 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();
            backend
                .insert("persist".into(), b"data".to_vec(), None, false)
                .unwrap();
            backend.flush().unwrap();
        }

        // Reload from same directory
        let backend = DiskBackend::new(dir.clone(), 4, None, 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();
        let result = backend.get("persist").unwrap();
        assert_eq!(result, Some((b"data".to_vec(), false)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistence_with_encryption() {
        let dir = temp_dir();
        let key = [42u8; 32];

        {
            let backend = DiskBackend::new(dir.clone(), 4, Some(key), 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();
            backend
                .insert("secret".into(), b"classified".to_vec(), None, false)
                .unwrap();
            backend.flush().unwrap();
        }

        // Verify the raw file is not readable as plaintext
        let raw = fs::read(dir.join("data.keyz")).unwrap();
        let raw_str = String::from_utf8_lossy(&raw);
        assert!(!raw_str.contains("classified"));

        // Reload with correct key
        let backend = DiskBackend::new(dir.clone(), 4, Some(key), 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();
        let result = backend.get("secret").unwrap();
        assert_eq!(result, Some((b"classified".to_vec(), false)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrong_key_fails_to_load() {
        let dir = temp_dir();
        let key = [1u8; 32];
        let wrong_key = [2u8; 32];

        {
            let backend = DiskBackend::new(dir.clone(), 4, Some(key), 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();
            backend
                .insert("k".into(), b"v".to_vec(), None, false)
                .unwrap();
            backend.flush().unwrap();
        }

        let result = DiskBackend::new(dir.clone(), 4, Some(wrong_key), 0, 60000, false, AofFsyncPolicy::EverySecond);
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_hit_on_second_get() {
        let dir = temp_dir();
        let backend = DiskBackend::new(dir.clone(), 4, None, 100, 60000, false, AofFsyncPolicy::EverySecond).unwrap();

        backend
            .insert("ck".into(), b"cv".to_vec(), None, false)
            .unwrap();

        // First get populates cache (insert already put it there)
        let _ = backend.get("ck").unwrap();
        // Second get should be a cache hit
        let _ = backend.get("ck").unwrap();

        let stats = backend.stats_extra();
        assert!(stats.cache_hits >= 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_entries_purged() {
        let dir = temp_dir();
        let backend = DiskBackend::new(dir.clone(), 4, None, 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();

        // Insert with past expiry
        backend
            .insert("old".into(), b"val".to_vec(), Some(1), false)
            .unwrap();

        // Should not be gettable
        assert_eq!(backend.get("old").unwrap(), None);
        assert_eq!(backend.len(), 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expires_in_returns_remaining() {
        let dir = temp_dir();
        let backend = DiskBackend::new(dir.clone(), 4, None, 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();

        let future = future_epoch(60);
        backend
            .insert("ttl".into(), b"val".to_vec(), Some(future), false)
            .unwrap();

        let remaining = backend.expires_in("ttl").unwrap();
        assert!(remaining.is_some());
        assert!(remaining.unwrap() <= 60);
        assert!(remaining.unwrap() > 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn partition_distribution() {
        let dir = temp_dir();
        let backend = DiskBackend::new(dir.clone(), 8, None, 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();

        for i in 0..100 {
            backend
                .insert(format!("key_{i}"), b"v".to_vec(), None, false)
                .unwrap();
        }

        assert_eq!(backend.len(), 100);

        // Check that keys are distributed across multiple partitions
        let mut non_empty = 0;
        for p in &backend.partitions {
            if !p.read().entries.is_empty() {
                non_empty += 1;
            }
        }
        assert!(non_empty > 1, "keys should be distributed across partitions");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_extra_counts() {
        let dir = temp_dir();
        let backend = DiskBackend::new(dir.clone(), 4, None, 10, 60000, false, AofFsyncPolicy::EverySecond).unwrap();

        backend
            .insert("a".into(), b"val".to_vec(), None, false)
            .unwrap();
        backend
            .insert("b".into(), b"val".to_vec(), None, true)
            .unwrap();

        let stats = backend.stats_extra();
        assert_eq!(stats.keys, 2);
        assert_eq!(stats.compressed_keys, 1);
        assert_eq!(stats.backend_type, "disk");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_nonexistent_returns_none() {
        let dir = temp_dir();
        let backend = DiskBackend::new(dir.clone(), 4, None, 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();

        assert_eq!(backend.delete("nope").unwrap(), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_on_drop_persists_data() {
        let dir = temp_dir();

        {
            let backend = DiskBackend::new(dir.clone(), 4, None, 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();
            backend
                .insert("dropped".into(), b"value".to_vec(), None, false)
                .unwrap();
            // No explicit flush — Drop should handle it
        }

        let backend = DiskBackend::new(dir.clone(), 4, None, 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();
        assert_eq!(
            backend.get("dropped").unwrap(),
            Some((b"value".to_vec(), false))
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compressed_flag_preserved() {
        let dir = temp_dir();

        {
            let backend = DiskBackend::new(dir.clone(), 4, None, 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();
            backend
                .insert("comp".into(), b"data".to_vec(), None, true)
                .unwrap();
            backend.flush().unwrap();
        }

        let backend = DiskBackend::new(dir.clone(), 4, None, 0, 60000, false, AofFsyncPolicy::EverySecond).unwrap();
        let (_, compressed) = backend.get("comp").unwrap().unwrap();
        assert!(compressed);

        let _ = fs::remove_dir_all(&dir);
    }

    // --- AOF integration tests ---

    #[test]
    fn aof_recovers_inserts_after_crash() {
        let dir = temp_dir();

        {
            let backend = DiskBackend::new(
                dir.clone(), 4, None, 0, 60000, true, AofFsyncPolicy::EveryWrite,
            ).unwrap();

            backend.insert("k1".into(), b"v1".to_vec(), None, false).unwrap();
            backend.insert("k2".into(), b"v2".to_vec(), None, false).unwrap();

            // Simulate crash: skip Drop (which would flush+truncate AOF)
            std::mem::forget(backend);
        }

        // AOF file should exist with records
        assert!(dir.join("data.aof").exists());

        // Reload — AOF should be replayed
        let backend = DiskBackend::new(
            dir.clone(), 4, None, 0, 60000, true, AofFsyncPolicy::EveryWrite,
        ).unwrap();

        assert_eq!(backend.get("k1").unwrap(), Some((b"v1".to_vec(), false)));
        assert_eq!(backend.get("k2").unwrap(), Some((b"v2".to_vec(), false)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn aof_recovers_deletes_after_crash() {
        let dir = temp_dir();

        // First: create a snapshot with k1 and k2
        {
            let backend = DiskBackend::new(
                dir.clone(), 4, None, 0, 60000, true, AofFsyncPolicy::EveryWrite,
            ).unwrap();
            backend.insert("k1".into(), b"v1".to_vec(), None, false).unwrap();
            backend.insert("k2".into(), b"v2".to_vec(), None, false).unwrap();
            backend.flush().unwrap(); // snapshot written, AOF truncated
        }

        // Second: delete k1, then crash
        {
            let backend = DiskBackend::new(
                dir.clone(), 4, None, 0, 60000, true, AofFsyncPolicy::EveryWrite,
            ).unwrap();
            backend.delete("k1").unwrap();

            std::mem::forget(backend);
        }

        // Reload — AOF should replay the delete
        let backend = DiskBackend::new(
            dir.clone(), 4, None, 0, 60000, true, AofFsyncPolicy::EveryWrite,
        ).unwrap();

        assert_eq!(backend.get("k1").unwrap(), None);
        assert_eq!(backend.get("k2").unwrap(), Some((b"v2".to_vec(), false)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn aof_truncated_after_flush() {
        let dir = temp_dir();

        let backend = DiskBackend::new(
            dir.clone(), 4, None, 0, 60000, true, AofFsyncPolicy::EveryWrite,
        ).unwrap();

        backend.insert("k".into(), b"v".to_vec(), None, false).unwrap();

        // AOF should have data
        let aof_size_before = fs::metadata(dir.join("data.aof")).unwrap().len();
        assert!(aof_size_before > 0);

        // Flush triggers AOF truncation
        backend.flush().unwrap();

        let aof_size_after = fs::metadata(dir.join("data.aof")).unwrap().len();
        assert_eq!(aof_size_after, 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn aof_with_encryption() {
        let dir = temp_dir();
        let key = [99u8; 32];

        {
            let backend = DiskBackend::new(
                dir.clone(), 4, Some(key), 0, 60000, true, AofFsyncPolicy::EveryWrite,
            ).unwrap();

            backend.insert("secret".into(), b"data".to_vec(), None, false).unwrap();

            std::mem::forget(backend);
        }

        // Reload with same key — should recover
        let backend = DiskBackend::new(
            dir.clone(), 4, Some(key), 0, 60000, true, AofFsyncPolicy::EveryWrite,
        ).unwrap();

        assert_eq!(backend.get("secret").unwrap(), Some((b"data".to_vec(), false)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn aof_partial_record_skipped() {
        let dir = temp_dir();

        {
            let backend = DiskBackend::new(
                dir.clone(), 4, None, 0, 60000, true, AofFsyncPolicy::EveryWrite,
            ).unwrap();

            backend.insert("good".into(), b"val".to_vec(), None, false).unwrap();

            std::mem::forget(backend);
        }

        // Append garbage to simulate a partial write from crash
        let aof_path = dir.join("data.aof");
        let mut f = OpenOptions::new().append(true).open(&aof_path).unwrap();
        f.write_all(&[0xFF, 0xFE, 0xFD]).unwrap();

        // Reload — should recover the valid record and skip garbage
        let backend = DiskBackend::new(
            dir.clone(), 4, None, 0, 60000, true, AofFsyncPolicy::EveryWrite,
        ).unwrap();

        assert_eq!(backend.get("good").unwrap(), Some((b"val".to_vec(), false)));

        let _ = fs::remove_dir_all(&dir);
    }
}
