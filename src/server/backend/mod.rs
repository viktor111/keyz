pub mod aof;
pub mod cache;
pub mod disk;
pub mod encryption;
pub mod format;
pub mod memory;

use std::any::Any;

use serde::Serialize;

use crate::server::error::Result;

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

    fn purge_expired(&self) -> Result<()>;

    fn stats_extra(&self) -> BackendStats;
}

#[derive(Debug, Serialize)]
pub struct BackendStats {
    pub keys: usize,
    pub compressed_keys: usize,
    pub backend_type: String,
    pub cache_hits: u64,
    pub cache_misses: u64,
}
