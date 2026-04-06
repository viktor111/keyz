use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;

use crate::server::error::Result;

use super::{BackendStats, StorageBackend};

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
    data: DashMap<String, ValueEntry>,
}

impl InMemoryBackend {
    pub fn new() -> Self {
        Self {
            data: DashMap::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_compressed(&self, key: &str) -> Option<bool> {
        self.data.get(key).map(|entry| entry.compressed)
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
        self.data.insert(
            key,
            ValueEntry {
                payload: value,
                expires_at,
                compressed,
            },
        );
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, bool)>> {
        let now = current_epoch_seconds()?;
        if let Some(entry) = self.data.get(key) {
            if entry.is_expired(now) {
                drop(entry);
                self.data.remove(key);
                return Ok(None);
            }
            return Ok(Some((entry.payload.clone(), entry.compressed)));
        }
        Ok(None)
    }

    fn delete(&self, key: &str) -> Result<Option<String>> {
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

    fn expires_in(&self, key: &str) -> Result<Option<u64>> {
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

    fn len(&self) -> usize {
        self.data.len()
    }

    fn purge_expired(&self) -> Result<()> {
        let now = current_epoch_seconds()?;
        self.data.retain(|_, value| !value.is_expired(now));
        Ok(())
    }

    fn stats_extra(&self) -> BackendStats {
        let keys = self.data.len();
        let compressed_keys = self.data.iter().filter(|entry| entry.compressed).count();
        BackendStats {
            keys,
            compressed_keys,
            backend_type: "memory".to_string(),
            cache_hits: 0,
            cache_misses: 0,
        }
    }
}
