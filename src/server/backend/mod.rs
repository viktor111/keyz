pub mod aof;
pub mod cache;
pub mod disk;
pub mod encryption;
pub mod format;
pub mod memory;

use std::any::Any;

use serde::Serialize;

use crate::server::error::{KeyzError, Result};

pub type SnapshotEntry = (String, Vec<u8>, Option<u64>, bool);

pub trait StorageBackend: Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn insert(
        &self,
        key: String,
        value: Vec<u8>,
        expires_at: Option<u64>,
        compressed: bool,
    ) -> Result<()>;

    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, bool)>>;

    fn delete(&self, key: &str) -> Result<Option<String>>;

    fn expires_in(&self, key: &str) -> Result<Option<u64>>;

    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn purge_expired(&self) -> Result<()>;

    /// Bytes occupied by stored keys and payloads, including expired entries
    /// until their removal. Container/allocator overhead is excluded.
    fn used_bytes(&self) -> u64 {
        self.snapshot_entries()
            .iter()
            .map(|(key, value, _, _)| entry_bytes(key, value))
            .sum()
    }

    fn set_max_bytes(&self, max_bytes: u64) -> Result<()> {
        let used = self.used_bytes();
        check_capacity(used, used, max_bytes)
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }

    /// Checkpoint and AOF synchronization have independent maintenance clocks.
    fn checkpoint_interval(&self) -> Option<std::time::Duration> {
        None
    }
    fn sync_log(&self) -> Result<()> {
        Ok(())
    }

    fn stats_extra(&self) -> BackendStats;

    fn snapshot_entries(&self) -> Vec<SnapshotEntry>;

    /// Replace the complete dataset atomically. Persistent backends must also
    /// retire the old recovery history before accepting subsequent mutations.
    fn replace_entries(&self, entries: Vec<SnapshotEntry>) -> Result<()>;
}

pub(crate) fn entry_bytes(key: &str, payload: &[u8]) -> u64 {
    key.len() as u64 + payload.len() as u64
}

pub(crate) fn check_capacity(used_bytes: u64, requested_bytes: u64, max_bytes: u64) -> Result<()> {
    if max_bytes != 0 && requested_bytes > max_bytes {
        return Err(KeyzError::CapacityExceeded {
            used_bytes,
            requested_bytes,
            max_bytes,
        });
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct BackendStats {
    pub keys: usize,
    pub compressed_keys: usize,
    pub backend_type: String,
    pub cache_hits: u64,
    pub cache_misses: u64,
}
