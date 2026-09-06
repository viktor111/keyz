//! Ordered, bounded admission to a dedicated blocking storage thread.
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
};

use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore, TryAcquireError};

use super::{
    error::{KeyzError, Result},
    health::Health,
    store::Store,
};

type Job = Box<dyn FnOnce(&Store) + Send + 'static>;

struct QueuedJob {
    job: Job,
    slot: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub struct StorageWorker {
    inner: Arc<Inner>,
}

struct Inner {
    // The lock only protects admission; storage work never runs under it.
    // Removing the sole sender seals the queue and lets the thread drain it.
    sender: Mutex<Option<mpsc::Sender<QueuedJob>>>,
    // Fair reservations protect waiting internal work from a stream of new
    // immediate admissions. Waiters never clone the queue's sole sender.
    slots: Arc<Semaphore>,
    completion: watch::Receiver<Option<std::result::Result<(), String>>>,
    metrics: Arc<Metrics>,
    capacity: usize,
    health: Health,
}

#[derive(Default)]
struct Metrics {
    accepted: AtomicU64,
    completed: AtomicU64,
    rejected: AtomicU64,
    waiting: AtomicU64,
    publication: Mutex<()>,
}

struct WaitingAdmission<'a>(&'a AtomicU64);

impl Drop for WaitingAdmission<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Metrics {
    fn publish(&self, health: &Health, depth: impl FnOnce() -> usize, capacity: usize) {
        let _publication = self.publication.lock();
        health.set_worker_stats(
            depth(),
            capacity,
            self.rejected.load(Ordering::Relaxed),
            self.completed.load(Ordering::Relaxed),
        );
    }
}

#[derive(Debug, Serialize)]
pub struct WorkerStats {
    pub queue_capacity: usize,
    pub queued_operations: usize,
    pub accepted_operations: u64,
    pub completed_operations: u64,
    pub rejected_operations: u64,
    pub waiting_operations: u64,
    pub accepting: bool,
}

impl StorageWorker {
    pub fn new(store: Store, capacity: usize) -> Result<Self> {
        if capacity == 0 || capacity > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(KeyzError::InvalidConfig(
                "work_queue_capacity must be positive and within Tokio's semaphore limit".into(),
            ));
        }
        let (sender, mut receiver) = mpsc::channel::<QueuedJob>(capacity);
        let (finished, completion) = watch::channel(None);
        let metrics = Arc::new(Metrics::default());
        let thread_metrics = Arc::clone(&metrics);
        let health = store.health();
        health.set_worker_stats(0, capacity, 0, 0);
        let thread_health = health.clone();
        thread::Builder::new()
            .name("keyz-storage".into())
            .spawn(move || {
                while let Some(QueuedJob { job, slot }) = receiver.blocking_recv() {
                    drop(slot);
                    thread_metrics.publish(&thread_health, || receiver.len(), capacity);
                    job(&store);
                    thread_metrics.completed.fetch_add(1, Ordering::Relaxed);
                    thread_metrics.publish(&thread_health, || receiver.len(), capacity);
                }
                let result = store.shutdown().map_err(|error| error.to_string());
                // Release storage before acknowledging shutdown. Disk backend Drop
                // is only a fallback; the explicit flush above reports errors.
                drop(store);
                thread_metrics.publish(&thread_health, || 0, capacity);
                let _ = finished.send(Some(result));
            })?;
        Ok(Self {
            inner: Arc::new(Inner {
                sender: Mutex::new(Some(sender)),
                slots: Arc::new(Semaphore::new(capacity)),
                completion,
                metrics,
                capacity,
                health,
            }),
        })
    }

    pub async fn execute<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.execute_with_admission(work, false).await
    }

    /// Wait fairly for one bounded queue slot instead of rejecting overload.
    /// Cancelling before admission submits nothing; accepted work still runs.
    /// Shutdown wakes waiters and never waits for them to release a sender.
    pub async fn execute_wait<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Store) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.execute_with_admission(work, true).await
    }

    async fn execute_with_admission<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Store) -> Result<T> + Send + 'static,
        wait: bool,
    ) -> Result<T> {
        let slots = Arc::clone(&self.inner.slots);
        let slot = if wait {
            self.inner.metrics.waiting.fetch_add(1, Ordering::Relaxed);
            let _waiting = WaitingAdmission(&self.inner.metrics.waiting);
            slots
                .acquire_owned()
                .await
                .map_err(|_| KeyzError::ShuttingDown)?
        } else {
            match slots.try_acquire_owned() {
                Ok(slot) => slot,
                Err(TryAcquireError::Closed) => return Err(KeyzError::ShuttingDown),
                Err(TryAcquireError::NoPermits) => {
                    let sender = self.inner.sender.lock();
                    let sender = sender.as_ref().ok_or(KeyzError::ShuttingDown)?;
                    self.inner.metrics.rejected.fetch_add(1, Ordering::Relaxed);
                    self.inner.metrics.publish(
                        &self.inner.health,
                        || self.inner.capacity - sender.capacity(),
                        self.inner.capacity,
                    );
                    return Err(KeyzError::Overloaded);
                }
            }
        };
        let (reply, response) = oneshot::channel();
        let job: Job = Box::new(move |store| {
            let result = catch_unwind(AssertUnwindSafe(|| work(store)))
                .unwrap_or_else(|_| Err(KeyzError::Backend("storage operation panicked".into())));
            // Dropping a client or cancelling its future never cancels a job
            // that the worker already accepted, including acknowledged writes.
            let _ = reply.send(result);
        });
        {
            let sender = self.inner.sender.lock();
            let sender = sender.as_ref().ok_or(KeyzError::ShuttingDown)?;
            match sender.try_send(QueuedJob { job, slot }) {
                Ok(()) => {
                    self.inner.metrics.accepted.fetch_add(1, Ordering::Relaxed);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.inner.metrics.rejected.fetch_add(1, Ordering::Relaxed);
                    self.inner.metrics.publish(
                        &self.inner.health,
                        || self.inner.capacity - sender.capacity(),
                        self.inner.capacity,
                    );
                    return Err(KeyzError::Overloaded);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(KeyzError::ShuttingDown),
            }
            self.inner.metrics.publish(
                &self.inner.health,
                || self.inner.capacity - sender.capacity(),
                self.inner.capacity,
            );
        }
        response
            .await
            .map_err(|_| KeyzError::Backend("storage worker stopped before replying".into()))?
    }

    pub fn stats(&self) -> WorkerStats {
        let sender = self.inner.sender.lock();
        WorkerStats {
            queue_capacity: self.inner.capacity,
            queued_operations: sender
                .as_ref()
                .map_or(0, |sender| self.inner.capacity - sender.capacity()),
            accepted_operations: self.inner.metrics.accepted.load(Ordering::Relaxed),
            completed_operations: self.inner.metrics.completed.load(Ordering::Relaxed),
            rejected_operations: self.inner.metrics.rejected.load(Ordering::Relaxed),
            waiting_operations: self.inner.metrics.waiting.load(Ordering::Relaxed),
            accepting: sender.is_some(),
        }
    }

    /// Seal admission, drain queued work, stop maintenance, and flush storage.
    /// Calling this concurrently or cancelling one shutdown waiter is safe.
    pub async fn shutdown(&self) -> Result<()> {
        {
            let mut sender = self.inner.sender.lock();
            self.inner.slots.close();
            sender.take();
        }
        let mut completion = self.inner.completion.clone();
        loop {
            if let Some(result) = completion.borrow().clone() {
                return result.map_err(KeyzError::Backend);
            }
            completion
                .changed()
                .await
                .map_err(|_| KeyzError::Backend("storage worker shutdown failed".into()))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{poll_fn, Future};
    use std::task::Poll;
    use std::time::Duration;

    async fn blocked_worker() -> (
        Store,
        StorageWorker,
        std::sync::mpsc::Sender<()>,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let store = Store::new();
        let worker = StorageWorker::new(store.clone(), 1).unwrap();
        let (started, running) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let handle = worker.clone();
        let task = tokio::spawn(async move {
            handle
                .execute(move |_| {
                    let _ = started.send(());
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                })
                .await
        });
        running.await.unwrap();
        (store, worker, release, task)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn waiting_admission_is_fair_and_accepted_cancelled_work_still_runs() {
        let (store, worker, release, first) = blocked_worker().await;
        let (second_started, second_running) = oneshot::channel();
        let (second_release, second_blocked) = std::sync::mpsc::channel();
        let mut second = Box::pin(worker.execute(move |_| {
            second_started.send(()).unwrap();
            second_blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        }));
        assert!(poll_fn(|cx| Poll::Ready(second.as_mut().poll(cx).is_pending())).await);
        let mut waiting = Box::pin(
            worker.execute_wait(|store| store.insert("waiting".into(), b"kept".to_vec(), 0)),
        );
        assert!(poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending())).await);
        assert_eq!(worker.stats().accepted_operations, 2);
        release.send(()).unwrap();
        second_running.await.unwrap();
        // The waiting operation owns the next slot even before its future is
        // repolled, so new external clients cannot repeatedly jump the queue.
        assert!(matches!(
            worker.execute(|_| Ok(())).await,
            Err(KeyzError::Overloaded)
        ));
        assert!(poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending())).await);
        assert_eq!(worker.stats().accepted_operations, 3);
        drop(waiting);
        second_release.send(()).unwrap();
        first.await.unwrap().unwrap();
        second.await.unwrap();
        worker.shutdown().await.unwrap();
        assert_eq!(store.get("waiting").unwrap(), Some(b"kept".to_vec()));
        assert_eq!(worker.stats().completed_operations, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_before_admission_submits_nothing() {
        let (store, worker, release, first) = blocked_worker().await;
        let mut queued = Box::pin(worker.execute(|_| Ok(())));
        assert!(poll_fn(|cx| Poll::Ready(queued.as_mut().poll(cx).is_pending())).await);
        let mut waiting = Box::pin(
            worker.execute_wait(|store| store.insert("cancelled".into(), b"never".to_vec(), 0)),
        );
        assert!(poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending())).await);
        drop(waiting);
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        queued.await.unwrap();
        worker.shutdown().await.unwrap();
        assert_eq!(worker.stats().accepted_operations, 2);
        assert_eq!(store.get("cancelled").unwrap(), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_drains_without_waiting_for_pending_admissions_to_be_polled() {
        let (_, worker, release, first) = blocked_worker().await;
        let mut queued = Box::pin(worker.execute(|_| Ok(())));
        assert!(poll_fn(|cx| Poll::Ready(queued.as_mut().poll(cx).is_pending())).await);
        let mut waiting = Box::pin(worker.execute_wait(|_| Ok(())));
        assert!(poll_fn(|cx| Poll::Ready(waiting.as_mut().poll(cx).is_pending())).await);
        let mut shutdown = Box::pin(worker.shutdown());
        assert!(poll_fn(|cx| Poll::Ready(shutdown.as_mut().poll(cx).is_pending())).await);
        release.send(()).unwrap();
        // Deliberately do not poll the waiting future until shutdown completes.
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(waiting.await, Err(KeyzError::ShuttingDown)));
        first.await.unwrap().unwrap();
        queued.await.unwrap();
        assert_eq!(worker.stats().accepted_operations, 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_queue_rejects_immediately_and_drain_keeps_cancelled_writes() {
        let store = Store::new();
        let worker = StorageWorker::new(store.clone(), 1).unwrap();
        let (started, running) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let handle = worker.clone();
        let first = tokio::spawn(async move {
            handle
                .execute(move |_| {
                    let _ = started.send(());
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                })
                .await
        });
        running.await.unwrap();
        let handle = worker.clone();
        let second = tokio::spawn(async move {
            handle
                .execute(|store| store.insert("accepted".into(), b"write".to_vec(), 0))
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while worker.stats().queued_operations != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            worker.execute(|_| Ok(())).await,
            Err(KeyzError::Overloaded)
        ));
        assert_eq!(worker.stats().rejected_operations, 1);
        assert_eq!(store.health().snapshot().work_queue_depth, 1);
        assert_eq!(store.health().snapshot().work_queue_capacity, 1);
        assert_eq!(store.health().snapshot().rejected_operations, 1);
        second.abort();
        let handle = worker.clone();
        let shutdown = tokio::spawn(async move { handle.shutdown().await });
        tokio::task::yield_now().await;
        assert!(matches!(
            worker.execute(|_| Ok(())).await,
            Err(KeyzError::ShuttingDown)
        ));
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(store.get("accepted").unwrap(), Some(b"write".to_vec()));
        assert_eq!(worker.stats().completed_operations, 2);
        assert_eq!(store.health().snapshot().work_queue_depth, 0);
        assert_eq!(store.health().snapshot().completed_operations, 2);
        worker.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_work_leaves_the_async_runtime_responsive() {
        let worker = StorageWorker::new(Store::new(), 1).unwrap();
        let (started, running) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let handle = worker.clone();
        let job = tokio::spawn(async move {
            handle
                .execute(move |_| {
                    started.send(()).unwrap();
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                })
                .await
        });
        running.await.unwrap();
        // A current-thread runtime can still schedule this task while storage blocks.
        tokio::time::timeout(Duration::from_millis(100), tokio::task::yield_now())
            .await
            .unwrap();
        release.send(()).unwrap();
        job.await.unwrap().unwrap();
        worker.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_reports_durable_flush_failure_to_all_waiters() {
        use crate::config::{BackendType, DiskConfig, StoreConfig};
        let dir =
            std::env::temp_dir().join(format!("keyz_worker_shutdown_{}", rand::random::<u64>()));
        let store = Store::with_config(StoreConfig {
            backend: BackendType::Disk,
            cleanup_interval_ms: 86_400_000,
            disk: DiskConfig {
                data_dir: dir.to_string_lossy().into(),
                aof_enabled: false,
                flush_interval_ms: 86_400_000,
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        let worker = StorageWorker::new(store.clone(), 4).unwrap();
        worker
            .execute(|store| store.insert("pending".into(), b"write".to_vec(), 0))
            .await
            .unwrap();
        std::fs::create_dir(dir.join("data.keyz.tmp")).unwrap();
        let (first, second) = tokio::join!(worker.shutdown(), worker.shutdown());
        assert!(first.is_err());
        assert!(second.is_err());
        assert_eq!(
            first.unwrap_err().to_string(),
            second.unwrap_err().to_string()
        );
        assert_eq!(store.stats().maintenance.flush_failures, 1);
        // Retain the Store to repair the failure and verify it remains recoverable.
        std::fs::remove_dir(dir.join("data.keyz.tmp")).unwrap();
        store.flush().unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
