use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use dashmap::DashMap;
use parking_lot::RwLock;

use crate::server::error::Result;

use super::{check_capacity, entry_bytes, BackendStats, SnapshotEntry, StorageBackend};

pub struct ValueEntry {
    pub(crate) payload: Vec<u8>,
    pub(crate) expires_at: Option<u64>,
    pub(crate) compressed: bool,
}

impl ValueEntry {
    fn is_expired(&self, now: u64) -> bool {
        match self.expires_at {
            Some(expiry) => now >= expiry,
            None => false,
        }
    }
}

pub struct InMemoryBackend {
    // Reads/deletes/cleanup share this guard and retain DashMap shard concurrency.
    // Inserts and replacements take it exclusively to commit byte accounting.
    data: RwLock<DashMap<String, ValueEntry>>,
    used_bytes: AtomicU64,
    max_bytes: AtomicU64,
}

impl Default for InMemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryBackend {
    pub fn new() -> Self {
        Self {
            data: RwLock::new(DashMap::new()),
            used_bytes: AtomicU64::new(0),
            max_bytes: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_compressed(&self, key: &str) -> Option<bool> {
        self.data.read().get(key).map(|entry| entry.compressed)
    }
}

fn current_epoch_seconds() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

impl StorageBackend for InMemoryBackend {
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
        // Serialize accounting with mutations and full snapshot replacement.
        // Readers retain shared access; no payload copy or full-map scan is needed.
        let data = self.data.write();
        let previous = data
            .get(&key)
            .map_or(0, |entry| entry_bytes(&key, &entry.payload));
        let used = self.used_bytes.load(Ordering::Relaxed);
        let next = used - previous + entry_bytes(&key, &value);
        check_capacity(used, next, self.max_bytes.load(Ordering::Relaxed))?;
        data.insert(
            key,
            ValueEntry {
                payload: value,
                expires_at,
                compressed,
            },
        );
        self.used_bytes.store(next, Ordering::Relaxed);
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, bool)>> {
        let now = current_epoch_seconds()?;
        let data = self.data.read();
        if let Some(entry) = data.get(key) {
            if entry.is_expired(now) {
                // The cleaner removes expired entries under their shard lock.
                // A read must not remove a replacement installed after its check.
                return Ok(None);
            }
            return Ok(Some((entry.payload.clone(), entry.compressed)));
        }
        Ok(None)
    }

    fn delete(&self, key: &str) -> Result<Option<String>> {
        let now = current_epoch_seconds()?;
        let data = self.data.read();
        match data.remove(key) {
            Some((removed_key, entry)) => {
                self.used_bytes
                    .fetch_sub(entry_bytes(&removed_key, &entry.payload), Ordering::Relaxed);
                if entry.is_expired(now) {
                    return Ok(None);
                }
                Ok(Some(removed_key))
            }
            None => Ok(None),
        }
    }

    fn expires_in(&self, key: &str) -> Result<Option<u64>> {
        let now = current_epoch_seconds()?;
        let data = self.data.read();
        let result = if let Some(entry) = data.get(key) {
            match entry.expires_at {
                Some(expiry) if now < expiry => Ok(Some(expiry - now)),
                Some(_) => Ok(None),
                None => Ok(None),
            }
        } else {
            Ok(None)
        };
        result
    }

    fn len(&self) -> usize {
        self.data.read().len()
    }

    fn purge_expired(&self) -> Result<()> {
        let now = current_epoch_seconds()?;
        self.data.read().retain(|key, value| {
            if value.is_expired(now) {
                self.used_bytes
                    .fetch_sub(entry_bytes(key, &value.payload), Ordering::Relaxed);
                false
            } else {
                true
            }
        });
        Ok(())
    }

    fn used_bytes(&self) -> u64 {
        self.used_bytes.load(Ordering::Relaxed)
    }

    fn set_max_bytes(&self, max_bytes: u64) -> Result<()> {
        let _data = self.data.write();
        let used = self.used_bytes();
        check_capacity(used, used, max_bytes)?;
        self.max_bytes.store(max_bytes, Ordering::Relaxed);
        Ok(())
    }

    fn stats_extra(&self) -> BackendStats {
        let data = self.data.read();
        let keys = data.len();
        let compressed_keys = data.iter().filter(|entry| entry.compressed).count();
        BackendStats {
            keys,
            compressed_keys,
            backend_type: "memory".to_string(),
            cache_hits: 0,
            cache_misses: 0,
        }
    }

    fn snapshot_entries(&self) -> Vec<SnapshotEntry> {
        let now = current_epoch_seconds().unwrap_or(0);
        self.data
            .write()
            .iter()
            .filter(|entry| !entry.is_expired(now))
            .map(|entry| {
                (
                    entry.key().clone(),
                    entry.payload.clone(),
                    entry.expires_at,
                    entry.compressed,
                )
            })
            .collect()
    }

    fn replace_entries(&self, entries: Vec<SnapshotEntry>) -> Result<()> {
        let replacement: DashMap<String, ValueEntry> = entries
            .into_iter()
            .map(|(key, payload, expires_at, compressed)| {
                (
                    key,
                    ValueEntry {
                        payload,
                        expires_at,
                        compressed,
                    },
                )
            })
            .collect();
        let bytes = replacement
            .iter()
            .map(|entry| entry_bytes(entry.key(), &entry.payload))
            .sum();
        let mut data = self.data.write();
        check_capacity(
            self.used_bytes(),
            bytes,
            self.max_bytes.load(Ordering::Relaxed),
        )?;
        *data = replacement;
        self.used_bytes.store(bytes, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn replacement_removes_absent_keys_and_preserves_metadata() -> Result<()> {
        let backend = InMemoryBackend::new();
        backend.insert("old".into(), b"obsolete".to_vec(), None, false)?;
        let expiry = current_epoch_seconds()? + 60;
        backend.replace_entries(vec![(
            "new".into(),
            b"payload".to_vec(),
            Some(expiry),
            true,
        )])?;
        assert_eq!(backend.get("old")?, None);
        assert_eq!(backend.get("new")?, Some((b"payload".to_vec(), true)));
        assert!(backend.expires_in("new")?.unwrap() <= 60);
        backend.replace_entries(Vec::new())?;
        assert_eq!(backend.len(), 0);
        Ok(())
    }

    #[test]
    fn concurrent_snapshots_never_observe_partial_replacement() -> Result<()> {
        let backend = Arc::new(InMemoryBackend::new());
        let entries = |generation: u8| {
            (0..32)
                .map(|i| (format!("key{i}"), vec![generation], None, false))
                .collect()
        };
        backend.replace_entries(entries(0))?;
        let reader = Arc::clone(&backend);
        let start = Arc::new(Barrier::new(2));
        let ready = Arc::clone(&start);
        let worker = thread::spawn(move || {
            ready.wait();
            for _ in 0..500 {
                let snapshot = reader.snapshot_entries();
                assert_eq!(snapshot.len(), 32);
                let generation = &snapshot[0].1;
                assert!(snapshot.iter().all(|entry| &entry.1 == generation));
            }
        });
        start.wait();
        for generation in 1..100 {
            backend.replace_entries(entries(generation))?;
        }
        worker.join().unwrap();
        Ok(())
    }

    #[test]
    fn expiry_queries_cannot_remove_a_concurrent_live_replacement() -> Result<()> {
        let backend = Arc::new(InMemoryBackend::new());
        let start = Arc::new(Barrier::new(2));
        let reader = Arc::clone(&backend);
        let ready = Arc::clone(&start);
        let worker = thread::spawn(move || {
            ready.wait();
            for i in 0..2_000 {
                let key = format!("key{i}");
                let _ = reader.get(&key).unwrap();
                let _ = reader.expires_in(&key).unwrap();
            }
        });
        for i in 0..2_000 {
            backend.insert(format!("key{i}"), b"expired".to_vec(), Some(1), false)?;
        }
        start.wait();
        for i in 0..2_000 {
            backend.insert(format!("key{i}"), b"live".to_vec(), None, false)?;
        }
        worker.join().unwrap();
        for i in 0..2_000 {
            assert_eq!(
                backend.get(&format!("key{i}"))?,
                Some((b"live".to_vec(), false))
            );
        }
        Ok(())
    }

    #[test]
    fn expired_values_are_hidden_and_cleaned_without_affecting_live_values() -> Result<()> {
        let backend = InMemoryBackend::new();
        backend.insert("old".into(), b"old".to_vec(), Some(1), false)?;
        backend.insert("live".into(), b"live".to_vec(), None, false)?;
        assert_eq!(backend.get("old")?, None);
        assert_eq!(backend.expires_in("old")?, None);
        backend.purge_expired()?;
        assert_eq!(
            backend.snapshot_entries(),
            vec![("live".into(), b"live".to_vec(), None, false)]
        );
        assert_eq!(backend.delete("live")?, Some("live".into()));
        assert_eq!(backend.delete("live")?, None);
        Ok(())
    }
}
