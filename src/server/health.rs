//! Shared lifecycle and replication health, independent of storage execution.
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub live: bool,
    pub ready: bool,
    pub storage_healthy: bool,
    pub replication_state: String,
    pub applied_records: u64,
    pub last_apply_unix_secs: Option<u64>,
    pub reconnects: u64,
    pub lagged_records: u64,
    pub last_replication_error: Option<String>,
    pub active_connections: u64,
    pub accepted_connections: u64,
    pub rejected_connections: u64,
    pub work_queue_depth: usize,
    pub work_queue_capacity: usize,
    pub rejected_operations: u64,
    pub completed_operations: u64,
}

#[derive(Clone)]
pub struct Health(Arc<Mutex<HealthSnapshot>>);

impl Default for Health {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(HealthSnapshot {
            live: true,
            ready: true,
            storage_healthy: true,
            replication_state: "disabled".into(),
            applied_records: 0,
            last_apply_unix_secs: None,
            reconnects: 0,
            lagged_records: 0,
            last_replication_error: None,
            active_connections: 0,
            accepted_connections: 0,
            rejected_connections: 0,
            work_queue_depth: 0,
            work_queue_capacity: 0,
            rejected_operations: 0,
            completed_operations: 0,
        })))
    }
}

impl Health {
    pub fn snapshot(&self) -> HealthSnapshot {
        let mut result = self.0.lock().clone();
        result.ready = result.live
            && result.storage_healthy
            && matches!(
                result.replication_state.as_str(),
                "disabled" | "leader" | "ready"
            );
        result
    }

    pub fn set_replication_state(&self, state: &str) {
        self.0.lock().replication_state = state.into();
    }

    pub fn set_storage_healthy(&self, healthy: bool) {
        self.0.lock().storage_healthy = healthy;
    }

    pub fn set_worker_stats(&self, depth: usize, capacity: usize, rejected: u64, completed: u64) {
        let mut health = self.0.lock();
        health.work_queue_depth = depth;
        health.work_queue_capacity = capacity;
        health.rejected_operations = rejected;
        health.completed_operations = completed;
    }

    pub(crate) fn connection_opened(&self) {
        let mut health = self.0.lock();
        health.active_connections = health.active_connections.saturating_add(1);
        health.accepted_connections = health.accepted_connections.saturating_add(1);
    }

    pub(crate) fn connection_closed(&self) {
        let mut health = self.0.lock();
        health.active_connections = health.active_connections.saturating_sub(1);
    }

    pub(crate) fn connection_rejected(&self) {
        let mut health = self.0.lock();
        health.rejected_connections = health.rejected_connections.saturating_add(1);
    }

    pub fn applied(&self) {
        let mut health = self.0.lock();
        health.applied_records = health.applied_records.saturating_add(1);
        health.last_apply_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs());
    }

    pub fn replication_failure(&self, message: &str) {
        let mut health = self.0.lock();
        health.replication_state = "disconnected".into();
        health.reconnects = health.reconnects.saturating_add(1);
        health.last_replication_error = Some(message.into());
    }

    pub fn lagged(&self, records: u64) {
        let mut health = self.0.lock();
        health.lagged_records = health.lagged_records.saturating_add(records);
    }

    pub fn stopping(&self) {
        let mut health = self.0.lock();
        health.live = false;
        health.replication_state = "stopping".into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_requires_complete_replication_and_healthy_storage() {
        let health = Health::default();
        assert!(health.snapshot().ready);
        for state in ["connecting", "syncing", "disconnected"] {
            health.set_replication_state(state);
            assert!(!health.snapshot().ready);
            assert!(health.snapshot().live);
        }
        health.set_replication_state("ready");
        health.applied();
        assert!(health.snapshot().ready);
        assert_eq!(health.snapshot().applied_records, 1);
        health.set_storage_healthy(false);
        assert!(!health.snapshot().ready);
        health.set_storage_healthy(true);
        assert!(health.snapshot().ready);
        health.stopping();
        assert!(!health.snapshot().live);
        assert!(!health.snapshot().ready);
    }
}
