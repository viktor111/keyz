use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};

use crate::config::AofFsyncPolicy;
use crate::server::error::{KeyzError, Result};

use super::aof::{AofRecord, AofWriter};
use super::encryption;
use super::format::{
    self, Entry, FileHeader, PartitionTableEntry, COMPRESSION_GZIP, HEADER_SIZE,
    PARTITION_TABLE_ENTRY_SIZE,
};
use super::{aof, check_capacity, entry_bytes, BackendStats, SnapshotEntry, StorageBackend};

pub struct DiskBackend {
    dir: PathBuf,
    // Never unlink this file: aliases and other processes must lock the same
    // inode/file object. The OS releases the lock when this handle is closed,
    // including abrupt process termination. Keep it through Drop's final flush.
    _directory_lock: File,
    partition_count: u32,
    partitions: Vec<RwLock<PartitionState>>,
    used_bytes: AtomicU64,
    max_bytes: AtomicU64,
    flush_interval: Duration,
    encryption_key: Option<[u8; 32]>,
    aof: Option<Mutex<AofWriter>>,
    aof_fsync_policy: AofFsyncPolicy,
    // All operations take this lock before AOF and partition locks.
    // Flush and replacement hold it exclusively through durable log retirement.
    operations: RwLock<()>,
    initialized: bool,
    replacement_pending: AtomicBool,
    #[cfg(test)]
    flush_copied_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    insert_published_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    snapshot_partition_limit: AtomicU64,
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

// The source must have been sync_all'd before calling this. On Windows,
// WRITE_THROUGH makes the replacement durable before the recovery log is retired.
#[cfg(windows)]
pub(super) fn durable_replace(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // Both paths are owned, NUL-terminated buffers that live through the call.
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0x1 | 0x8) } == 0 {
        return Err(KeyzError::Backend(format!(
            "failed to durably replace snapshot: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
pub(super) fn durable_replace(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)
        .map_err(|e| KeyzError::Backend(format!("failed to replace snapshot: {e}")))?;
    File::open(destination.parent().unwrap())
        .and_then(|directory| directory.sync_all())
        .map_err(|e| KeyzError::Backend(format!("failed to sync snapshot directory: {e}")))?;
    Ok(())
}

fn write_durable_file(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = File::create(path)
        .map_err(|e| KeyzError::Backend(format!("failed to create snapshot: {e}")))?;
    file.write_all(data)
        .and_then(|_| file.sync_all())
        .map_err(|e| KeyzError::Backend(format!("failed to write and sync snapshot: {e}")))
}

impl DiskBackend {
    pub fn new(
        dir: PathBuf,
        partition_count: u32,
        encryption_key: Option<[u8; 32]>,
        _cache_max_entries: usize,
        flush_interval_ms: u64,
        aof_enabled: bool,
        aof_fsync: AofFsyncPolicy,
    ) -> Result<Self> {
        if partition_count == 0 {
            return Err(KeyzError::Backend(
                "partition count must be positive".into(),
            ));
        }
        fs::create_dir_all(&dir).map_err(|e| {
            KeyzError::Backend(format!("failed to create data dir {}: {e}", dir.display()))
        })?;
        let directory_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(".keyz.lock"))
            .map_err(|e| KeyzError::Backend(format!("failed to open data directory lock: {e}")))?;
        directory_lock.try_lock().map_err(|e| {
            KeyzError::Backend(format!(
                "data directory {} is already in use or cannot be exclusively locked: {e}",
                dir.display()
            ))
        })?;

        let mut partitions = Vec::with_capacity(partition_count as usize);
        for _ in 0..partition_count {
            partitions.push(RwLock::new(PartitionState {
                entries: HashMap::new(),
                dirty: false,
            }));
        }

        let mut backend = Self {
            dir,
            _directory_lock: directory_lock,
            partition_count,
            partitions,
            // Partitions already retain every payload in memory; another LRU
            // duplicates keys/payloads and adds locks without avoiding any I/O.
            // Accept cache_max_entries for configuration compatibility.
            used_bytes: AtomicU64::new(0),
            max_bytes: AtomicU64::new(0),
            flush_interval: Duration::from_millis(flush_interval_ms.max(1)),
            encryption_key,
            aof: None,
            aof_fsync_policy: aof_fsync,
            operations: RwLock::new(()),
            initialized: false,
            replacement_pending: AtomicBool::new(false),
            #[cfg(test)]
            flush_copied_hook: Mutex::new(None),
            #[cfg(test)]
            insert_published_hook: Mutex::new(None),
            #[cfg(test)]
            snapshot_partition_limit: AtomicU64::new(u64::from(u32::MAX)),
        };

        backend.load_from_disk()?;

        // A committed replacement supersedes both the old snapshot and old log.
        // Validate its entire contents before changing either source file.
        if backend.replacement_pending.load(Ordering::Relaxed) {
            backend.finish_replacement_locked()?;
        }

        if aof_enabled {
            let aof_path = backend.aof_file_path();

            // Replay existing AOF records
            if aof_path.exists() {
                let records = aof::replay_aof(&aof_path, encryption_key.as_ref())?;
                if !records.is_empty() {
                    backend.apply_aof_records(&records);
                }
            }

            // Even an empty recovered log needs a durable snapshot before it can
            // be retired (including recovery from the legacy backup swap).
            backend.flush()?;

            let mut writer = AofWriter::new(aof_path, encryption_key)?;
            // Truncate in case we just replayed+flushed, or to start clean
            writer.truncate()?;
            backend.aof = Some(Mutex::new(writer));
        }

        if !backend.data_file_path().exists()
            && (backend.dir.join("data.keyz.bak").exists() || backend.temp_file_path().exists())
        {
            backend.flush()?;
        }

        let bytes = backend
            .partitions
            .iter()
            .map(|partition| {
                partition
                    .read()
                    .entries
                    .iter()
                    .map(|(key, value)| entry_bytes(key, &value.payload))
                    .sum::<u64>()
            })
            .sum();
        backend.used_bytes.store(bytes, Ordering::Relaxed);
        backend.initialized = true;

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

    fn replacement_file_path(&self) -> PathBuf {
        self.dir.join("data.keyz.replace")
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
        let path = if self.replacement_file_path().exists() {
            self.replacement_pending.store(true, Ordering::Relaxed);
            self.replacement_file_path()
        } else if self.data_file_path().exists() {
            self.data_file_path()
        } else if self.dir.join("data.keyz.bak").exists() {
            self.dir.join("data.keyz.bak")
        } else if self.temp_file_path().exists() {
            self.temp_file_path()
        } else {
            return Ok(());
        };

        let data = fs::read(&path)
            .map_err(|e| KeyzError::Backend(format!("failed to read {}: {e}", path.display())))?;

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

        let table_entries = format::validate_partition_table(&data, &header)?;

        for (i, te) in table_entries.iter().enumerate() {
            if te.length == 0 {
                continue;
            }

            let start = te.offset as usize;
            let end = start + te.length as usize;
            let partition_data = &data[start..end];

            let plaintext = if header.encryption_enabled {
                let key = self.encryption_key.as_ref().unwrap();
                encryption::decrypt_partition(partition_data, key)?
            } else {
                partition_data.to_vec()
            };

            let entries =
                format::deserialize_partition_entries_counted(&plaintext, te.entry_count)?;

            let mut partition = self.partitions[i].write();
            for entry in entries {
                if partition_for_key(&entry.key, self.partition_count) as usize != i
                    || partition.entries.contains_key(&entry.key)
                {
                    return Err(KeyzError::CorruptDataFile(format!(
                        "duplicate or misplaced key in partition {i}"
                    )));
                }
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
        let _operations = self.operations.write();
        self.finish_replacement_locked()?;
        let log_needs_recovery = self
            .aof
            .as_ref()
            .is_some_and(|aof| aof.lock().needs_recovery());
        if self.has_dirty_partitions() || !self.data_file_path().exists() || log_needs_recovery {
            self.flush_locked()?;
        }
        Ok(())
    }

    fn flush_locked(&self) -> Result<()> {
        // Reject unrepresentable partitions before copying the resident dataset.
        // The exclusive operations lock keeps these sizes stable through flush.
        for partition in &self.partitions {
            self.partition_serialized_size(&partition.read().entries)?;
        }
        let partitions: Vec<_> = self
            .partitions
            .iter()
            .map(|p| p.read().entries.clone())
            .collect();
        let file_data = self.serialize_snapshot(&partitions)?;

        #[cfg(test)]
        if let Some(hook) = self.flush_copied_hook.lock().take() {
            hook();
        }

        write_durable_file(&self.temp_file_path(), &file_data)?;
        durable_replace(&self.temp_file_path(), &self.data_file_path())?;

        // Only discard the recovery log after snapshot data and replacement are
        // durable. No mutation can cross this boundary while operations is held.
        if let Some(ref aof) = self.aof {
            aof.lock().truncate()?;
        }
        for partition in &self.partitions {
            partition.write().dirty = false;
        }
        Ok(())
    }

    fn partition_serialized_size(
        &self,
        partition: &HashMap<String, DiskValueEntry>,
    ) -> Result<(u32, u32)> {
        let entry_count =
            format::checked_u32_length(partition.len() as u64, "partition entry count")?;
        if partition.is_empty() {
            return Ok((0, 0));
        }
        let plaintext_size = partition.iter().try_fold(0usize, |size, (key, value)| {
            size.checked_add(format::serialized_entry_length(
                key.len(),
                value.payload.len(),
            )?)
            .ok_or_else(|| KeyzError::Backend("serialized partition size overflow".into()))
        })?;
        // AES-GCM adds a 12-byte nonce and a 16-byte authentication tag.
        let encoded_size = plaintext_size
            .checked_add(if self.encryption_key.is_some() { 28 } else { 0 })
            .ok_or_else(|| KeyzError::Backend("encrypted partition size overflow".into()))?;
        let length = format::checked_u32_length(encoded_size as u64, "partition length")?;
        #[cfg(test)]
        if encoded_size as u64 > self.snapshot_partition_limit.load(Ordering::Relaxed) {
            return Err(KeyzError::Backend(
                "partition length exceeds test format limit".into(),
            ));
        }
        Ok((entry_count, length))
    }

    fn serialize_snapshot(
        &self,
        partitions: &[HashMap<String, DiskValueEntry>],
    ) -> Result<Vec<u8>> {
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
        let table_end = (self.partition_count as usize)
            .checked_mul(PARTITION_TABLE_ENTRY_SIZE)
            .and_then(|size| table_start.checked_add(size))
            .ok_or_else(|| KeyzError::Backend("partition table size overflow".into()))?;
        file_data.resize(table_end, 0);

        let mut table_entries = Vec::with_capacity(self.partition_count as usize);

        for partition in partitions {
            if partition.is_empty() {
                table_entries.push(PartitionTableEntry {
                    offset: 0,
                    length: 0,
                    entry_count: 0,
                });
                continue;
            }

            let (entry_count, length) = self.partition_serialized_size(partition)?;

            let entries: Vec<Entry> = partition
                .iter()
                .map(|(k, v)| Entry {
                    key: k.clone(),
                    value: v.payload.clone(),
                    expires_at: v.expires_at.unwrap_or(0),
                    compressed: v.compressed,
                })
                .collect();

            let serialized = format::serialize_partition_entries(&entries)?;

            let partition_bytes = if encryption_enabled {
                let key = self.encryption_key.as_ref().unwrap();
                encryption::encrypt_partition(&serialized, key)?
            } else {
                serialized
            };

            let offset = file_data.len() as u64;
            debug_assert_eq!(partition_bytes.len() as u64, u64::from(length));

            table_entries.push(PartitionTableEntry {
                offset,
                length,
                entry_count,
            });

            file_data.extend(partition_bytes);
        }

        // Write partition table
        for (i, te) in table_entries.iter().enumerate() {
            let offset = table_start + i * PARTITION_TABLE_ENTRY_SIZE;
            let serialized = te.serialize();
            file_data[offset..offset + PARTITION_TABLE_ENTRY_SIZE].copy_from_slice(&serialized);
        }

        Ok(file_data)
    }

    fn has_dirty_partitions(&self) -> bool {
        self.partitions.iter().any(|p| p.read().dirty)
    }

    fn finish_replacement_locked(&self) -> Result<()> {
        if !self.replacement_pending.load(Ordering::Relaxed) {
            return Ok(());
        }
        if self.replacement_file_path().exists() {
            fs::OpenOptions::new()
                .write(true)
                .open(self.replacement_file_path())
                .and_then(|f| f.sync_all())
                .map_err(|e| {
                    KeyzError::Backend(format!("failed to sync replacement intent: {e}"))
                })?;
            #[cfg(not(windows))]
            File::open(&self.dir)
                .and_then(|d| d.sync_all())
                .map_err(|e| {
                    KeyzError::Backend(format!("failed to sync replacement intent directory: {e}"))
                })?;
        }
        if let Some(ref aof) = self.aof {
            aof.lock().truncate()?;
        } else if self.aof_file_path().exists() {
            // Initialization (including configurations with AOF disabled) must
            // retire an old log only after the replacement intent is durable.
            AofWriter::new(self.aof_file_path(), self.encryption_key)?.truncate()?;
        }
        // A previous rename may have succeeded but its directory sync failed.
        // Keep the log empty until the final snapshot name is durably committed.
        if self.replacement_file_path().exists() {
            durable_replace(&self.replacement_file_path(), &self.data_file_path())?;
        } else {
            fs::OpenOptions::new()
                .write(true)
                .open(self.data_file_path())
                .and_then(|f| f.sync_all())
                .map_err(|e| {
                    KeyzError::Backend(format!("failed to sync replacement snapshot: {e}"))
                })?;
            #[cfg(not(windows))]
            File::open(&self.dir)
                .and_then(|d| d.sync_all())
                .map_err(|e| {
                    KeyzError::Backend(format!("failed to sync replacement directory: {e}"))
                })?;
        }
        self.replacement_pending.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn ensure_replacement_finished(&self) -> Result<()> {
        if self.replacement_pending.load(Ordering::Relaxed) {
            let _operations = self.operations.write();
            self.finish_replacement_locked()?;
        }
        Ok(())
    }

    fn check_replacement_finished(&self) -> Result<()> {
        if self.replacement_pending.load(Ordering::Relaxed) {
            return Err(KeyzError::Backend(
                "snapshot replacement needs recovery before writing".into(),
            ));
        }
        Ok(())
    }

    fn remove_if_expired(&self, key: &str, now: u64) {
        let idx = partition_for_key(key, self.partition_count) as usize;
        let mut partition = self.partitions[idx].write();
        if partition
            .entries
            .get(key)
            .and_then(|v| v.expires_at)
            .is_some_and(|expiry| now >= expiry)
        {
            if let Some(entry) = partition.entries.remove(key) {
                self.used_bytes
                    .fetch_sub(entry_bytes(key, &entry.payload), Ordering::Relaxed);
            }
            partition.dirty = true;
        }
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
        self.ensure_replacement_finished()?;
        let _operations = self.operations.write();
        self.check_replacement_finished()?;
        let partition_idx = partition_for_key(&key, self.partition_count) as usize;

        // Keep the AOF and partition in one commit order. Append before
        // publishing the value so a failed write does not leave an unlogged SET.
        let mut writer = self.aof.as_ref().map(|aof| aof.lock());
        let mut partition = self.partitions[partition_idx].write();
        let used = self.used_bytes();
        let previous = partition
            .entries
            .get(&key)
            .map_or(0, |entry| entry_bytes(&key, &entry.payload));
        let next = used - previous + entry_bytes(&key, &value);
        check_capacity(used, next, self.max_bytes.load(Ordering::Relaxed))?;
        if let Some(ref mut writer) = writer {
            let record = AofRecord::Set {
                key: key.clone(),
                value: value.clone(),
                expires_at: expires_at.unwrap_or(0),
                compressed,
            };
            writer.append_synced(&record, self.aof_fsync_policy == AofFsyncPolicy::EveryWrite)?;
        }

        partition.entries.insert(
            key,
            DiskValueEntry {
                payload: value,
                expires_at,
                compressed,
            },
        );
        self.used_bytes.store(next, Ordering::Relaxed);
        partition.dirty = true;

        #[cfg(test)]
        if let Some(hook) = self.insert_published_hook.lock().take() {
            hook();
        }

        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, bool)>> {
        let _operations = self.operations.read();
        let partition_idx = partition_for_key(key, self.partition_count) as usize;
        let partition = self.partitions[partition_idx].read();
        let now = current_epoch_seconds()?;

        if let Some(entry) = partition.entries.get(key) {
            if let Some(exp) = entry.expires_at {
                if now >= exp {
                    drop(partition);
                    self.remove_if_expired(key, now);
                    return Ok(None);
                }
            }

            let result = (entry.payload.clone(), entry.compressed);

            return Ok(Some(result));
        }

        Ok(None)
    }

    fn delete(&self, key: &str) -> Result<Option<String>> {
        self.ensure_replacement_finished()?;
        let _operations = self.operations.read();
        self.check_replacement_finished()?;
        let partition_idx = partition_for_key(key, self.partition_count) as usize;
        let now = current_epoch_seconds()?;

        let mut writer = self.aof.as_ref().map(|aof| aof.lock());
        let mut partition = self.partitions[partition_idx].write();
        let Some(entry) = partition.entries.get(key) else {
            return Ok(None);
        };
        let was_live = entry.expires_at.is_none_or(|expiry| now < expiry);
        if was_live {
            if let Some(ref mut writer) = writer {
                writer.append_synced(
                    &AofRecord::Del {
                        key: key.to_string(),
                    },
                    self.aof_fsync_policy == AofFsyncPolicy::EveryWrite,
                )?;
            }
        }
        let removed = partition.entries.remove(key).expect("entry remains locked");
        self.used_bytes
            .fetch_sub(entry_bytes(key, &removed.payload), Ordering::Relaxed);
        partition.dirty = true;
        Ok(was_live.then(|| key.to_string()))
    }

    fn expires_in(&self, key: &str) -> Result<Option<u64>> {
        let _operations = self.operations.read();
        let partition_idx = partition_for_key(key, self.partition_count) as usize;
        let now = current_epoch_seconds()?;

        let partition = self.partitions[partition_idx].read();
        if let Some(entry) = partition.entries.get(key) {
            match entry.expires_at {
                Some(expiry) if now < expiry => Ok(Some(expiry - now)),
                Some(_) => {
                    drop(partition);
                    self.remove_if_expired(key, now);
                    Ok(None)
                }
                None => Ok(None),
            }
        } else {
            Ok(None)
        }
    }

    fn len(&self) -> usize {
        let _operations = self.operations.read();
        self.partitions.iter().map(|p| p.read().entries.len()).sum()
    }

    fn purge_expired(&self) -> Result<()> {
        let _operations = self.operations.write();
        self.finish_replacement_locked()?;
        let now = current_epoch_seconds()?;

        for partition in &self.partitions {
            let mut p = partition.write();
            let before = p.entries.len();
            p.entries.retain(|key, v| {
                if let Some(exp) = v.expires_at {
                    if now >= exp {
                        self.used_bytes
                            .fetch_sub(entry_bytes(key, &v.payload), Ordering::Relaxed);
                    }
                    now < exp
                } else {
                    true
                }
            });
            if p.entries.len() != before {
                p.dirty = true;
            }
        }

        Ok(())
    }

    fn used_bytes(&self) -> u64 {
        self.used_bytes.load(Ordering::Relaxed)
    }

    fn set_max_bytes(&self, max_bytes: u64) -> Result<()> {
        let _operations = self.operations.write();
        let used = self.used_bytes();
        check_capacity(used, used, max_bytes)?;
        self.max_bytes.store(max_bytes, Ordering::Relaxed);
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        DiskBackend::flush(self)
    }

    fn checkpoint_interval(&self) -> Option<Duration> {
        Some(self.flush_interval)
    }

    fn sync_log(&self) -> Result<()> {
        let _operations = self.operations.read();
        if self.aof_fsync_policy == AofFsyncPolicy::EverySecond {
            if let Some(ref aof) = self.aof {
                aof.lock().fsync()?;
            }
        }
        Ok(())
    }

    fn stats_extra(&self) -> BackendStats {
        let _operations = self.operations.read();
        let mut keys = 0;
        let mut compressed_keys = 0;

        for partition in &self.partitions {
            let p = partition.read();
            keys += p.entries.len();
            compressed_keys += p.entries.values().filter(|v| v.compressed).count();
        }

        BackendStats {
            keys,
            compressed_keys,
            backend_type: "disk".to_string(),
            cache_hits: 0,
            cache_misses: 0,
        }
    }

    fn snapshot_entries(&self) -> Vec<SnapshotEntry> {
        let _operations = self.operations.write();
        let now = current_epoch_seconds().unwrap_or(0);
        let mut entries = Vec::new();
        for partition in &self.partitions {
            let p = partition.read();
            for (key, value) in &p.entries {
                let expired = value.expires_at.is_some_and(|exp| now >= exp);
                if !expired {
                    entries.push((
                        key.clone(),
                        value.payload.clone(),
                        value.expires_at,
                        value.compressed,
                    ));
                }
            }
        }
        entries
    }

    fn replace_entries(&self, entries: Vec<SnapshotEntry>) -> Result<()> {
        let mut replacement: Vec<HashMap<String, DiskValueEntry>> =
            (0..self.partition_count).map(|_| HashMap::new()).collect();
        for (key, payload, expires_at, compressed) in entries {
            let idx = partition_for_key(&key, self.partition_count) as usize;
            replacement[idx].insert(
                key,
                DiskValueEntry {
                    payload,
                    expires_at,
                    compressed,
                },
            );
        }
        let file_data = self.serialize_snapshot(&replacement)?;
        let _operations = self.operations.write();
        self.finish_replacement_locked()?;
        let bytes = replacement
            .iter()
            .map(|partition| {
                partition
                    .iter()
                    .map(|(key, entry)| entry_bytes(key, &entry.payload))
                    .sum::<u64>()
            })
            .sum();
        check_capacity(
            self.used_bytes(),
            bytes,
            self.max_bytes.load(Ordering::Relaxed),
        )?;

        // Once this intent is durable, recovery treats it as authoritative and
        // ignores the old AOF. Retirement must precede its final rename so there
        // is no crash window in which stale log entries resurrect absent keys.
        write_durable_file(&self.temp_file_path(), &file_data)?;
        let committed = durable_replace(&self.temp_file_path(), &self.replacement_file_path());
        if committed.is_err() && !self.replacement_file_path().exists() {
            return committed;
        }
        self.replacement_pending.store(true, Ordering::Relaxed);
        for (partition, entries) in self.partitions.iter().zip(replacement) {
            let mut partition = partition.write();
            partition.entries = entries;
            partition.dirty = false;
        }
        self.used_bytes.store(bytes, Ordering::Relaxed);
        // If syncing the intent rename failed, keep both the intent and old log.
        // A subsequent mutation retries finalization before it can append.
        committed?;
        self.finish_replacement_locked()
    }
}

impl Drop for DiskBackend {
    fn drop(&mut self) {
        if !self.initialized {
            return;
        }
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

    fn open_test_backend(dir: &Path, key: Option<[u8; 32]>) -> DiskBackend {
        DiskBackend::new(
            dir.to_path_buf(),
            4,
            key,
            32,
            60000,
            true,
            AofFsyncPolicy::EveryWrite,
        )
        .unwrap()
    }

    fn crash_backend(mut backend: DiskBackend) {
        // Close OS handles without the clean-shutdown snapshot, as a crash does.
        backend.initialized = false;
    }

    #[test]
    fn a_second_directory_opener_cannot_touch_snapshot_or_log() {
        let dir = temp_dir();
        let backend = open_test_backend(&dir, None);
        backend
            .insert("accepted".into(), b"value".to_vec(), None, false)
            .unwrap();
        let snapshot = fs::read(dir.join("data.keyz")).unwrap();
        let log = fs::read(dir.join("data.aof")).unwrap();
        for alias in [dir.clone(), dir.join(".")] {
            assert!(
                DiskBackend::new(alias, 4, None, 0, 60000, true, AofFsyncPolicy::EveryWrite)
                    .is_err()
            );
            assert_eq!(fs::read(dir.join("data.keyz")).unwrap(), snapshot);
            assert_eq!(fs::read(dir.join("data.aof")).unwrap(), log);
        }
        crash_backend(backend);
        assert!(dir.join(".keyz.lock").exists());
        let reopened = open_test_backend(&dir, None);
        assert_eq!(
            reopened.get("accepted").unwrap(),
            Some((b"value".to_vec(), false))
        );
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_snapshot_metadata_preserves_all_recovery_sources() {
        for encryption_key in [None, Some([61; 32])] {
            let dir = temp_dir();
            let backend = open_test_backend(&dir, encryption_key);
            backend
                .insert("saved".into(), b"original".to_vec(), None, false)
                .unwrap();
            backend.flush().unwrap();
            backend
                .insert("logged".into(), b"accepted later".to_vec(), None, false)
                .unwrap();
            crash_backend(backend);
            let original = fs::read(dir.join("data.keyz")).unwrap();
            let log = fs::read(dir.join("data.aof")).unwrap();
            let header = FileHeader::deserialize(&original).unwrap();
            let rows = format::validate_partition_table(&original, &header).unwrap();
            let index = rows.iter().position(|row| row.length != 0).unwrap();
            let valid = rows[index].clone();
            for corrupted in [
                PartitionTableEntry {
                    length: 0,
                    ..valid.clone()
                },
                PartitionTableEntry {
                    offset: u64::MAX,
                    ..valid.clone()
                },
                PartitionTableEntry {
                    offset: HEADER_SIZE as u64,
                    ..valid.clone()
                },
                PartitionTableEntry {
                    entry_count: 0,
                    ..valid.clone()
                },
                PartitionTableEntry {
                    entry_count: valid.entry_count + 1,
                    ..valid.clone()
                },
                PartitionTableEntry {
                    length: valid.length + 1,
                    ..valid.clone()
                },
            ] {
                let mut data = original.clone();
                let offset = HEADER_SIZE + index * PARTITION_TABLE_ENTRY_SIZE;
                data[offset..offset + PARTITION_TABLE_ENTRY_SIZE]
                    .copy_from_slice(&corrupted.serialize());
                fs::write(dir.join("data.keyz"), &data).unwrap();
                assert!(DiskBackend::new(
                    dir.clone(),
                    4,
                    encryption_key,
                    0,
                    60000,
                    true,
                    AofFsyncPolicy::EveryWrite
                )
                .is_err());
                assert_eq!(fs::read(dir.join("data.keyz")).unwrap(), data);
                assert_eq!(fs::read(dir.join("data.aof")).unwrap(), log);
            }
            fs::write(dir.join("data.keyz"), original).unwrap();
            let backend = open_test_backend(&dir, encryption_key);
            assert_eq!(
                backend.get("saved").unwrap(),
                Some((b"original".to_vec(), false))
            );
            assert_eq!(
                backend.get("logged").unwrap(),
                Some((b"accepted later".to_vec(), false))
            );
            drop(backend);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn unrepresentable_checkpoint_keeps_snapshot_and_acknowledged_log() {
        for encryption_key in [None, Some([63; 32])] {
            let dir = temp_dir();
            let backend = open_test_backend(&dir, encryption_key);
            backend
                .insert("pending".into(), b"acknowledged".to_vec(), None, false)
                .unwrap();
            let snapshot = fs::read(dir.join("data.keyz")).unwrap();
            let log = fs::read(dir.join("data.aof")).unwrap();
            // Exercise the actual pre-commit size failure without allocating 4 GiB.
            backend
                .snapshot_partition_limit
                .store(16, Ordering::Relaxed);
            assert!(backend.flush().is_err());
            assert!(backend.has_dirty_partitions());
            assert!(backend
                .replace_entries(vec![("replacement".into(), b"new".to_vec(), None, false)])
                .is_err());
            assert_eq!(fs::read(dir.join("data.keyz")).unwrap(), snapshot);
            assert_eq!(fs::read(dir.join("data.aof")).unwrap(), log);
            assert!(!backend.replacement_file_path().exists());
            crash_backend(backend);
            let backend = open_test_backend(&dir, encryption_key);
            assert_eq!(
                backend.get("pending").unwrap(),
                Some((b"acknowledged".to_vec(), false))
            );
            assert_eq!(backend.get("replacement").unwrap(), None);
            drop(backend);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn durable_checkpoint_recovers_a_poisoned_log_without_committing_failed_writes() {
        for encryption_key in [None, Some([65; 32])] {
            let dir = temp_dir();
            let backend = open_test_backend(&dir, encryption_key);
            backend
                .insert("before".into(), b"accepted".to_vec(), None, false)
                .unwrap();
            backend.flush().unwrap();
            assert!(!backend.has_dirty_partitions());
            backend
                .aof
                .as_ref()
                .unwrap()
                .lock()
                .inject_faults(aof::AofFaults {
                    append_after: Some(7),
                    rollback: true,
                    ..Default::default()
                });
            assert!(backend
                .insert("failed".into(), b"uncommitted".to_vec(), None, false)
                .is_err());
            assert!(backend
                .insert("blocked".into(), b"uncommitted".to_vec(), None, false)
                .is_err());
            assert_eq!(backend.get("failed").unwrap(), None);
            backend.flush().unwrap();
            backend
                .insert("after".into(), b"accepted".to_vec(), None, false)
                .unwrap();
            crash_backend(backend);
            let backend = open_test_backend(&dir, encryption_key);
            for key in ["before", "after"] {
                assert_eq!(
                    backend.get(key).unwrap(),
                    Some((b"accepted".to_vec(), false))
                );
            }
            for key in ["failed", "blocked"] {
                assert_eq!(backend.get(key).unwrap(), None);
            }
            drop(backend);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn failed_partition_validation_preserves_original_snapshot() {
        let dir = temp_dir();
        let backend = open_test_backend(&dir, None);
        backend
            .insert("preserved".into(), b"value".to_vec(), None, false)
            .unwrap();
        drop(backend);
        let original = fs::read(dir.join("data.keyz")).unwrap();
        assert!(DiskBackend::new(
            dir.clone(),
            8,
            None,
            0,
            60000,
            true,
            AofFsyncPolicy::EveryWrite
        )
        .is_err());
        assert_eq!(fs::read(dir.join("data.keyz")).unwrap(), original);
        let backend = open_test_backend(&dir, None);
        assert_eq!(
            backend.get("preserved").unwrap(),
            Some((b"value".to_vec(), false))
        );
        drop(backend);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wrong_aof_key_preserves_snapshot_and_recovery_log() {
        let dir = temp_dir();
        let key = [37; 32];
        let backend = open_test_backend(&dir, Some(key));
        backend
            .insert("recover".into(), b"secret".to_vec(), None, false)
            .unwrap();
        crash_backend(backend);
        let snapshot = fs::read(dir.join("data.keyz")).unwrap();
        let log = fs::read(dir.join("data.aof")).unwrap();
        for wrong_key in [Some([73; 32]), None] {
            assert!(DiskBackend::new(
                dir.clone(),
                4,
                wrong_key,
                32,
                60000,
                true,
                AofFsyncPolicy::EveryWrite
            )
            .is_err());
            assert_eq!(fs::read(dir.join("data.keyz")).unwrap(), snapshot);
            assert_eq!(fs::read(dir.join("data.aof")).unwrap(), log);
        }
        let backend = open_test_backend(&dir, Some(key));
        assert_eq!(
            backend.get("recover").unwrap(),
            Some((b"secret".to_vec(), false))
        );
        drop(backend);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn backup_recovery_preserves_baseline_and_replays_later_writes() {
        let dir = temp_dir();
        let backend = open_test_backend(&dir, None);
        backend
            .insert("baseline".into(), b"old".to_vec(), None, false)
            .unwrap();
        backend.flush().unwrap();
        backend
            .insert("later".into(), b"new".to_vec(), None, false)
            .unwrap();
        crash_backend(backend);
        fs::rename(dir.join("data.keyz"), dir.join("data.keyz.bak")).unwrap();
        fs::write(dir.join("data.keyz.tmp"), b"incomplete temporary snapshot").unwrap();
        let backend = open_test_backend(&dir, None);
        assert_eq!(
            backend.get("baseline").unwrap(),
            Some((b"old".to_vec(), false))
        );
        assert_eq!(
            backend.get("later").unwrap(),
            Some((b"new".to_vec(), false))
        );
        assert!(dir.join("data.keyz").exists());
        drop(backend);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_snapshot_write_keeps_log_and_dirty_state() {
        let dir = temp_dir();
        let backend = open_test_backend(&dir, None);
        backend
            .insert("pending".into(), b"acknowledged".to_vec(), None, false)
            .unwrap();
        let log = fs::read(dir.join("data.aof")).unwrap();
        let snapshot = fs::read(dir.join("data.keyz")).unwrap();
        fs::create_dir(dir.join("data.keyz.tmp")).unwrap();
        assert!(backend.flush().is_err());
        assert!(backend.has_dirty_partitions());
        assert_eq!(fs::read(dir.join("data.aof")).unwrap(), log);
        assert_eq!(fs::read(dir.join("data.keyz")).unwrap(), snapshot);
        fs::remove_dir(dir.join("data.keyz.tmp")).unwrap();
        crash_backend(backend);
        let backend = open_test_backend(&dir, None);
        assert_eq!(
            backend.get("pending").unwrap(),
            Some((b"acknowledged".to_vec(), false))
        );
        drop(backend);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_flush_cannot_retire_a_later_acknowledged_write() {
        use std::sync::{mpsc, Arc};
        use std::time::Duration;
        let dir = temp_dir();
        let backend = Arc::new(open_test_backend(&dir, None));
        backend
            .insert("before".into(), b"snapshot".to_vec(), None, false)
            .unwrap();
        let (copied_tx, copied_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *backend.flush_copied_hook.lock() = Some(Box::new(move || {
            copied_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        let flushing = Arc::clone(&backend);
        let flush = std::thread::spawn(move || flushing.flush().unwrap());
        copied_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let writing = Arc::clone(&backend);
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            writing
                .insert("during".into(), b"acknowledged".to_vec(), None, false)
                .unwrap();
            done_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        release_tx.send(()).unwrap();
        flush.join().unwrap();
        writer.join().unwrap();
        let backend = Arc::try_unwrap(backend).ok().unwrap();
        crash_backend(backend);
        let backend = open_test_backend(&dir, None);
        assert_eq!(
            backend.get("before").unwrap(),
            Some((b"snapshot".to_vec(), false))
        );
        assert_eq!(
            backend.get("during").unwrap(),
            Some((b"acknowledged".to_vec(), false))
        );
        drop(backend);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cache_publication_cannot_follow_a_later_delete() {
        use std::sync::{mpsc, Arc};
        use std::time::Duration;
        let dir = temp_dir();
        let backend = Arc::new(open_test_backend(&dir, None));
        let (published_tx, published_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *backend.insert_published_hook.lock() = Some(Box::new(move || {
            published_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        let writing = Arc::clone(&backend);
        let writer = std::thread::spawn(move || {
            writing
                .insert("race".into(), b"old".to_vec(), None, false)
                .unwrap()
        });
        published_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let deleting = Arc::clone(&backend);
        let (done_tx, done_rx) = mpsc::channel();
        let deletion = std::thread::spawn(move || {
            assert_eq!(deleting.delete("race").unwrap(), Some("race".into()));
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        release_tx.send(()).unwrap();
        writer.join().unwrap();
        deletion.join().unwrap();
        assert_eq!(backend.get("race").unwrap(), None);
        let backend = Arc::try_unwrap(backend).ok().unwrap();
        drop(backend);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expiry_cleanup_rechecks_replacement_before_removing_it() {
        let dir = temp_dir();
        let backend = open_test_backend(&dir, None);
        backend
            .insert("race".into(), b"expired".to_vec(), Some(1), false)
            .unwrap();
        let observed_now = current_epoch_seconds().unwrap();
        // Model a SET after the reader observed expiry and dropped its guard.
        backend
            .insert(
                "race".into(),
                b"replacement".to_vec(),
                Some(future_epoch(60)),
                false,
            )
            .unwrap();
        backend.remove_if_expired("race", observed_now);
        assert_eq!(
            backend.get("race").unwrap(),
            Some((b"replacement".to_vec(), false))
        );
        assert!(backend.expires_in("race").unwrap().is_some());
        drop(backend);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn full_replacement_removes_cached_and_logged_keys_across_restart() {
        for key in [None, Some([49; 32])] {
            let dir = temp_dir();
            let backend = open_test_backend(&dir, key);
            backend
                .insert("baseline".into(), b"old".to_vec(), None, false)
                .unwrap();
            backend.flush().unwrap();
            backend
                .insert("ghost".into(), b"cached and logged".to_vec(), None, false)
                .unwrap();
            let expiry = future_epoch(120);
            backend
                .replace_entries(vec![(
                    "new".into(),
                    b"snapshot".to_vec(),
                    Some(expiry),
                    true,
                )])
                .unwrap();
            assert_eq!(backend.get("baseline").unwrap(), None);
            assert_eq!(backend.get("ghost").unwrap(), None);
            assert_eq!(
                backend.get("new").unwrap(),
                Some((b"snapshot".to_vec(), true))
            );
            crash_backend(backend);
            let backend = open_test_backend(&dir, key);
            assert_eq!(
                backend.snapshot_entries(),
                vec![("new".into(), b"snapshot".to_vec(), Some(expiry), true)]
            );
            backend.replace_entries(vec![]).unwrap();
            crash_backend(backend);
            let backend = open_test_backend(&dir, key);
            assert_eq!(backend.len(), 0);
            drop(backend);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn committed_replacement_recovers_before_or_after_log_retirement() {
        for retire_log in [false, true] {
            let dir = temp_dir();
            let backend = open_test_backend(&dir, None);
            backend
                .insert("ghost".into(), b"logged".to_vec(), None, false)
                .unwrap();
            let empty = backend
                .serialize_snapshot(&vec![HashMap::new(); 4])
                .unwrap();
            write_durable_file(&backend.replacement_file_path(), &empty).unwrap();
            if retire_log {
                backend.aof.as_ref().unwrap().lock().truncate().unwrap();
            }
            crash_backend(backend);
            let backend = open_test_backend(&dir, None);
            assert_eq!(backend.get("ghost").unwrap(), None);
            assert!(!backend.replacement_file_path().exists());
            backend
                .insert("after".into(), b"new log".to_vec(), None, false)
                .unwrap();
            crash_backend(backend);
            let backend = open_test_backend(&dir, None);
            assert_eq!(backend.get("ghost").unwrap(), None);
            assert_eq!(
                backend.get("after").unwrap(),
                Some((b"new log".to_vec(), false))
            );
            drop(backend);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn pending_replacement_rejects_new_writes_until_log_retirement_succeeds() {
        let dir = temp_dir();
        let backend = open_test_backend(&dir, None);
        backend
            .insert("ghost".into(), b"old".to_vec(), None, false)
            .unwrap();
        backend
            .aof
            .as_ref()
            .unwrap()
            .lock()
            .inject_faults(aof::AofFaults {
                truncate_sync: true,
                ..Default::default()
            });
        assert!(backend.replace_entries(vec![]).is_err());
        assert!(backend.replacement_pending.load(Ordering::Relaxed));
        assert_eq!(backend.get("ghost").unwrap(), None);
        backend
            .aof
            .as_ref()
            .unwrap()
            .lock()
            .inject_faults(aof::AofFaults {
                truncate_sync: true,
                ..Default::default()
            });
        assert!(backend
            .insert(
                "blocked".into(),
                b"must not be acknowledged".to_vec(),
                None,
                false
            )
            .is_err());
        crash_backend(backend);
        let backend = open_test_backend(&dir, None);
        assert_eq!(backend.len(), 0);
        drop(backend);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_replacement_intent_preserves_all_recovery_sources() {
        let dir = temp_dir();
        let key = [67; 32];
        let backend = open_test_backend(&dir, Some(key));
        backend
            .insert("baseline".into(), b"old".to_vec(), None, false)
            .unwrap();
        backend.flush().unwrap();
        backend
            .insert("logged".into(), b"later".to_vec(), None, false)
            .unwrap();
        let mut replacement = vec![HashMap::new(); 4];
        replacement[0].insert(
            "replacement".into(),
            DiskValueEntry {
                payload: b"new".to_vec(),
                expires_at: None,
                compressed: false,
            },
        );
        let intent = backend.serialize_snapshot(&replacement).unwrap();
        write_durable_file(&backend.replacement_file_path(), &intent).unwrap();
        crash_backend(backend);
        let snapshot = fs::read(dir.join("data.keyz")).unwrap();
        let log = fs::read(dir.join("data.aof")).unwrap();
        assert!(DiskBackend::new(
            dir.clone(),
            4,
            Some([76; 32]),
            0,
            60000,
            true,
            AofFsyncPolicy::EveryWrite
        )
        .is_err());
        assert_eq!(fs::read(dir.join("data.keyz")).unwrap(), snapshot);
        assert_eq!(fs::read(dir.join("data.aof")).unwrap(), log);
        assert_eq!(fs::read(dir.join("data.keyz.replace")).unwrap(), intent);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replacement_recovery_retires_old_log_even_when_aof_is_disabled() {
        let dir = temp_dir();
        let backend = open_test_backend(&dir, None);
        backend
            .insert("ghost".into(), b"old log".to_vec(), None, false)
            .unwrap();
        let empty = backend
            .serialize_snapshot(&vec![HashMap::new(); 4])
            .unwrap();
        write_durable_file(&backend.replacement_file_path(), &empty).unwrap();
        crash_backend(backend);
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            false,
            AofFsyncPolicy::EveryWrite,
        )
        .unwrap();
        assert_eq!(backend.get("ghost").unwrap(), None);
        assert_eq!(fs::metadata(dir.join("data.aof")).unwrap().len(), 0);
        crash_backend(backend);
        let backend = open_test_backend(&dir, None);
        assert_eq!(backend.get("ghost").unwrap(), None);
        drop(backend);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn startup_preserves_log_when_replacement_intent_cannot_be_synced() {
        let dir = temp_dir();
        let backend = open_test_backend(&dir, None);
        backend
            .insert("ghost".into(), b"old log".to_vec(), None, false)
            .unwrap();
        let intent_path = backend.replacement_file_path();
        let empty = backend
            .serialize_snapshot(&vec![HashMap::new(); 4])
            .unwrap();
        write_durable_file(&intent_path, &empty).unwrap();
        crash_backend(backend);
        let log = fs::read(dir.join("data.aof")).unwrap();
        let permissions = fs::metadata(&intent_path).unwrap().permissions();
        let mut read_only = permissions.clone();
        read_only.set_readonly(true);
        fs::set_permissions(&intent_path, read_only).unwrap();
        let result = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            true,
            AofFsyncPolicy::EveryWrite,
        );
        // Restore permissions before assertions so failed tests can be cleaned up.
        fs::set_permissions(&intent_path, permissions).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read(dir.join("data.aof")).unwrap(), log);
        assert_eq!(fs::read(&intent_path).unwrap(), empty);
        fs::remove_dir_all(dir).unwrap();
    }

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
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();

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
            let backend = DiskBackend::new(
                dir.clone(),
                4,
                None,
                0,
                60000,
                false,
                AofFsyncPolicy::EverySecond,
            )
            .unwrap();
            backend
                .insert("persist".into(), b"data".to_vec(), None, false)
                .unwrap();
            backend.flush().unwrap();
        }

        // Reload from same directory
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();
        let result = backend.get("persist").unwrap();
        assert_eq!(result, Some((b"data".to_vec(), false)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistence_with_encryption() {
        let dir = temp_dir();
        let key = [42u8; 32];

        {
            let backend = DiskBackend::new(
                dir.clone(),
                4,
                Some(key),
                0,
                60000,
                false,
                AofFsyncPolicy::EverySecond,
            )
            .unwrap();
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
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            Some(key),
            0,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();
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
            let backend = DiskBackend::new(
                dir.clone(),
                4,
                Some(key),
                0,
                60000,
                false,
                AofFsyncPolicy::EverySecond,
            )
            .unwrap();
            backend
                .insert("k".into(), b"v".to_vec(), None, false)
                .unwrap();
            backend.flush().unwrap();
        }

        let original = fs::read(dir.join("data.keyz")).unwrap();
        let result = DiskBackend::new(
            dir.clone(),
            4,
            Some(wrong_key),
            0,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        );
        assert!(result.is_err());
        assert_eq!(fs::read(dir.join("data.keyz")).unwrap(), original);
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            Some(key),
            0,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();
        assert_eq!(backend.get("k").unwrap(), Some((b"v".to_vec(), false)));
        drop(backend);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_cache_configuration_does_not_duplicate_resident_payloads() {
        let dir = temp_dir();
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            100,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();

        backend
            .insert("ck".into(), b"cv".to_vec(), None, false)
            .unwrap();

        assert_eq!(backend.get("ck").unwrap(), Some((b"cv".to_vec(), false)));
        assert_eq!(backend.get("ck").unwrap(), Some((b"cv".to_vec(), false)));

        let stats = backend.stats_extra();
        assert_eq!(stats.cache_hits, 0);
        assert_eq!(stats.cache_misses, 0);
        assert_eq!(backend.used_bytes(), 4);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_entries_purged() {
        let dir = temp_dir();
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();

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
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();

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
        let backend = DiskBackend::new(
            dir.clone(),
            8,
            None,
            0,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();

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
        assert!(
            non_empty > 1,
            "keys should be distributed across partitions"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_extra_counts() {
        let dir = temp_dir();
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            10,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();

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
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();

        assert_eq!(backend.delete("nope").unwrap(), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_on_drop_persists_data() {
        let dir = temp_dir();

        {
            let backend = DiskBackend::new(
                dir.clone(),
                4,
                None,
                0,
                60000,
                false,
                AofFsyncPolicy::EverySecond,
            )
            .unwrap();
            backend
                .insert("dropped".into(), b"value".to_vec(), None, false)
                .unwrap();
            // No explicit flush — Drop should handle it
        }

        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();
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
            let backend = DiskBackend::new(
                dir.clone(),
                4,
                None,
                0,
                60000,
                false,
                AofFsyncPolicy::EverySecond,
            )
            .unwrap();
            backend
                .insert("comp".into(), b"data".to_vec(), None, true)
                .unwrap();
            backend.flush().unwrap();
        }

        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            false,
            AofFsyncPolicy::EverySecond,
        )
        .unwrap();
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
                dir.clone(),
                4,
                None,
                0,
                60000,
                true,
                AofFsyncPolicy::EveryWrite,
            )
            .unwrap();

            backend
                .insert("k1".into(), b"v1".to_vec(), None, false)
                .unwrap();
            backend
                .insert("k2".into(), b"v2".to_vec(), None, false)
                .unwrap();

            // Simulate crash: skip Drop (which would flush+truncate AOF)
            crash_backend(backend);
        }

        // AOF file should exist with records
        assert!(dir.join("data.aof").exists());

        // Reload — AOF should be replayed
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            true,
            AofFsyncPolicy::EveryWrite,
        )
        .unwrap();

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
                dir.clone(),
                4,
                None,
                0,
                60000,
                true,
                AofFsyncPolicy::EveryWrite,
            )
            .unwrap();
            backend
                .insert("k1".into(), b"v1".to_vec(), None, false)
                .unwrap();
            backend
                .insert("k2".into(), b"v2".to_vec(), None, false)
                .unwrap();
            backend.flush().unwrap(); // snapshot written, AOF truncated
        }

        // Second: delete k1, then crash
        {
            let backend = DiskBackend::new(
                dir.clone(),
                4,
                None,
                0,
                60000,
                true,
                AofFsyncPolicy::EveryWrite,
            )
            .unwrap();
            backend.delete("k1").unwrap();

            crash_backend(backend);
        }

        // Reload — AOF should replay the delete
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            true,
            AofFsyncPolicy::EveryWrite,
        )
        .unwrap();

        assert_eq!(backend.get("k1").unwrap(), None);
        assert_eq!(backend.get("k2").unwrap(), Some((b"v2".to_vec(), false)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn aof_truncated_after_flush() {
        let dir = temp_dir();

        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            true,
            AofFsyncPolicy::EveryWrite,
        )
        .unwrap();

        backend
            .insert("k".into(), b"v".to_vec(), None, false)
            .unwrap();

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
                dir.clone(),
                4,
                Some(key),
                0,
                60000,
                true,
                AofFsyncPolicy::EveryWrite,
            )
            .unwrap();

            backend
                .insert("secret".into(), b"data".to_vec(), None, false)
                .unwrap();

            crash_backend(backend);
        }

        // Reload with same key — should recover
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            Some(key),
            0,
            60000,
            true,
            AofFsyncPolicy::EveryWrite,
        )
        .unwrap();

        assert_eq!(
            backend.get("secret").unwrap(),
            Some((b"data".to_vec(), false))
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn aof_partial_record_skipped() {
        let dir = temp_dir();

        {
            let backend = DiskBackend::new(
                dir.clone(),
                4,
                None,
                0,
                60000,
                true,
                AofFsyncPolicy::EveryWrite,
            )
            .unwrap();

            backend
                .insert("good".into(), b"val".to_vec(), None, false)
                .unwrap();

            crash_backend(backend);
        }

        // Append a genuinely incomplete record to simulate a torn final write.
        let aof_path = dir.join("data.aof");
        let mut f = OpenOptions::new().append(true).open(&aof_path).unwrap();
        let partial = AofRecord::Set {
            key: "partial".into(),
            value: b"uncommitted".to_vec(),
            expires_at: 0,
            compressed: false,
        }
        .serialize();
        f.write_all(&partial[..partial.len() - 3]).unwrap();

        // Reload — should recover the valid record and skip garbage
        let backend = DiskBackend::new(
            dir.clone(),
            4,
            None,
            0,
            60000,
            true,
            AofFsyncPolicy::EveryWrite,
        )
        .unwrap();

        assert_eq!(backend.get("good").unwrap(), Some((b"val".to_vec(), false)));

        let _ = fs::remove_dir_all(&dir);
    }
}
