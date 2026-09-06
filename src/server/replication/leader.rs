use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{timeout, Duration};

use crate::config::SecurityConfig;
use crate::server::backend::aof::AofRecord;
use crate::server::backend::SnapshotEntry;
use crate::server::error::{KeyzError, Result};
use crate::server::init::{log_event, wait_for_shutdown};
use crate::server::store::Store;
use crate::server::worker::StorageWorker;
use crate::transport::{authenticate, AsyncStream, Transport};

use super::{write_replication_frame, ReplicationOptions, SYNC_COMPLETE_MARKER};

const BROADCAST_CAPACITY: usize = 4096;

pub struct ReplicationBroadcaster {
    sender: broadcast::Sender<Vec<u8>>,
}

impl Default for ReplicationBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplicationBroadcaster {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self { sender }
    }

    pub fn broadcast(&self, record: &AofRecord) {
        let serialized = record.serialize();
        let mut frame = Vec::with_capacity(4 + serialized.len());
        frame.extend_from_slice(&(serialized.len() as u32).to_be_bytes());
        frame.extend(serialized);
        let _ = self.sender.send(frame);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.sender.subscribe()
    }
}

struct FollowerInfo {
    connection_id: u64,
    address: SocketAddr,
    connected_at: Instant,
}

pub struct FollowerRegistry {
    followers: Mutex<Vec<FollowerInfo>>,
    next_connection_id: AtomicU64,
}

impl Default for FollowerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

struct FollowerRegistration {
    registry: Arc<FollowerRegistry>,
    connection_id: u64,
}

impl Drop for FollowerRegistration {
    fn drop(&mut self) {
        self.registry.remove(self.connection_id);
    }
}

impl FollowerRegistry {
    pub fn new() -> Self {
        Self {
            followers: Mutex::new(Vec::new()),
            next_connection_id: AtomicU64::new(0),
        }
    }

    fn add(self: &Arc<Self>, address: SocketAddr) -> FollowerRegistration {
        let connection_id = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let mut followers = self.followers.lock();
        followers.retain(|f| f.address != address);
        followers.push(FollowerInfo {
            connection_id,
            address,
            connected_at: Instant::now(),
        });
        FollowerRegistration {
            registry: Arc::clone(self),
            connection_id,
        }
    }

    fn remove(&self, connection_id: u64) {
        self.followers
            .lock()
            .retain(|f| f.connection_id != connection_id);
    }

    pub fn list(&self) -> Vec<(SocketAddr, f64)> {
        self.followers
            .lock()
            .iter()
            .map(|f| (f.address, f.connected_at.elapsed().as_secs_f64()))
            .collect()
    }
}

pub async fn run_leader(
    listener: &TcpListener,
    store: Store,
    broadcaster: Arc<ReplicationBroadcaster>,
    registry: Arc<FollowerRegistry>,
) {
    let health = store.health();
    let Ok(worker) = StorageWorker::new(store, 1024) else {
        return;
    };
    let options = ReplicationOptions {
        transport: Transport::new(&SecurityConfig::default()).expect("default transport"),
        auth_token: None,
        max_connections: 1024,
        max_snapshot_bytes: 0,
        write_timeout: Duration::from_secs(30),
        shutdown_timeout: Duration::from_secs(30),
        health,
    };
    let (_signal, shutdown) = watch::channel(false);
    if let Err(error) = run_leader_until(
        listener,
        worker.clone(),
        broadcaster,
        registry,
        options,
        shutdown,
    )
    .await
    {
        log_event("replication_listener_failed", &error.to_string());
    }
    if let Err(error) = worker.shutdown().await {
        log_event("replication_shutdown_failed", &error.to_string());
    }
}

pub async fn run_leader_until(
    listener: &TcpListener,
    worker: StorageWorker,
    broadcaster: Arc<ReplicationBroadcaster>,
    registry: Arc<FollowerRegistry>,
    options: ReplicationOptions,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let permits = Arc::new(Semaphore::new(options.max_connections));
    let mut followers = JoinSet::new();
    options.health.set_replication_state("leader");
    loop {
        tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => break,
            joined = followers.join_next(), if !followers.is_empty() => {
                if let Some(Err(error)) = joined {
                    log_event("replication_task_failed", &error.to_string());
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    drop(stream);
                    log_event("replication_connection_limit", "follower connection rejected");
                    continue;
                };
                let broadcaster = broadcaster.clone();
                let worker = worker.clone();
                let registry = registry.clone();
                let options = options.clone();
                let shutdown = shutdown.clone();
                followers.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve_follower(stream, worker, broadcaster, registry, options, shutdown).await {
                        log_event("replication_follower_disconnected", &error.to_string());
                    }
                });
            }
        }
    }
    if timeout(options.shutdown_timeout, async {
        while followers.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        followers.abort_all();
        while followers.join_next().await.is_some() {}
    }
    Ok(())
}

async fn serve_follower(
    stream: TcpStream,
    worker: StorageWorker,
    broadcaster: Arc<ReplicationBroadcaster>,
    registry: Arc<FollowerRegistry>,
    options: ReplicationOptions,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let handshake = async {
        let mut stream = options.transport.accept(stream).await?;
        if let Some(token) = &options.auth_token {
            let frame = crate::server::helpers::read_message(&mut stream, 4101).await?;
            if !frame
                .strip_prefix("AUTH ")
                .is_some_and(|supplied| authenticate(supplied, token))
            {
                crate::server::helpers::write_message(&mut stream, "error:unauthorized").await?;
                return Err(KeyzError::Unauthorized);
            }
            crate::server::helpers::write_message(&mut stream, "ok").await?;
        }
        // Addresses are small; do not accept a full replication-sized handshake allocation.
        let address = crate::server::helpers::read_message(&mut stream, 128).await?;
        let address: SocketAddr = address
            .parse()
            .map_err(|_| KeyzError::InvalidSocketAddress)?;
        Ok::<_, KeyzError>((stream, address))
    };
    let (stream, address) = tokio::select! {
        _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
        result = timeout(options.write_timeout, handshake) => result.map_err(|_| KeyzError::ClientTimeout)??,
    };
    let _registration = registry.add(address);
    let (snapshot, rx) = tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
        result = worker.execute_wait(move |store| Ok(store.replication_snapshot(&broadcaster))) => result?,
    };
    handle_follower(stream, rx, snapshot, worker, options, shutdown).await
}

async fn handle_follower(
    mut stream: AsyncStream,
    mut rx: broadcast::Receiver<Vec<u8>>,
    snapshot: Vec<SnapshotEntry>,
    worker: StorageWorker,
    options: ReplicationOptions,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    tokio::select! {
        _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
        result = send_snapshot(&mut stream, snapshot, &worker, options.write_timeout) => result?,
    }
    timeout(
        options.write_timeout,
        write_replication_frame(&mut stream, &[SYNC_COMPLETE_MARKER]),
    )
    .await
    .map_err(|_| KeyzError::ClientTimeout)??;

    let (mut reader, mut writer) = tokio::io::split(stream);
    let mut peer_data = [0u8; 1];
    loop {
        tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
            read = reader.read(&mut peer_data) => {
                if read? == 0 {
                    return Ok(());
                }
                return Err(KeyzError::Backend("unexpected follower data after handshake".into()));
            }
            received = rx.recv() => match received {
                Ok(frame) => {
                    timeout(options.write_timeout, async {
                        writer.write_all(&frame).await?;
                        writer.flush().await
                    }).await.map_err(|_| KeyzError::ClientTimeout)??;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    options.health.lagged(n);
                    log_event("replication_lag", &format!("follower missed {n} records; resync required"));
                    return Ok(());
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

async fn send_snapshot(
    stream: &mut AsyncStream,
    entries: Vec<SnapshotEntry>,
    worker: &StorageWorker,
    write_timeout: Duration,
) -> Result<()> {
    for (key, payload, expires_at, compressed) in entries {
        let record = AofRecord::Set {
            key,
            value: payload,
            expires_at: expires_at.unwrap_or(0),
            compressed,
        };
        // CRC calculation and record allocation can be large; keep both off
        // the network runtime without staging a second full serialized snapshot.
        let serialized = worker.execute_wait(move |_| Ok(record.serialize())).await?;
        timeout(write_timeout, write_replication_frame(stream, &serialized))
            .await
            .map_err(|_| KeyzError::ClientTimeout)??;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StoreConfig;
    use crate::server::replication::read_replication_frame;
    use std::time::Duration;

    #[test]
    fn closing_old_connection_preserves_its_replacement_registration() {
        let registry = Arc::new(FollowerRegistry::new());
        let address = "127.0.0.1:7980".parse().unwrap();
        let old_connection = registry.add(address);
        let replacement = registry.add(address);
        assert_eq!(registry.list().len(), 1);
        drop(old_connection);
        assert_eq!(registry.list()[0].0, address);
        drop(replacement);
        assert!(registry.list().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_waits_for_capacity_and_shutdown_cancels_waiting_capture() -> Result<()> {
        for cancel_capture in [false, true] {
            let mut store = Store::new();
            let broadcaster = Arc::new(ReplicationBroadcaster::new());
            store.set_broadcaster(broadcaster.clone());
            store.insert("snapshot".into(), b"value".to_vec(), 0)?;
            let health = store.health();
            let worker = StorageWorker::new(store, 1)?;
            let registry = Arc::new(FollowerRegistry::new());
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let (signal, shutdown) = watch::channel(false);
            let server_worker = worker.clone();
            let server_registry = registry.clone();
            let leader = tokio::spawn(async move {
                run_leader_until(
                    &listener,
                    server_worker,
                    broadcaster,
                    server_registry,
                    ReplicationOptions {
                        transport: Transport::new(&SecurityConfig::default()).unwrap(),
                        auth_token: None,
                        max_connections: 2,
                        max_snapshot_bytes: 0,
                        write_timeout: Duration::from_secs(2),
                        shutdown_timeout: Duration::from_secs(2),
                        health,
                    },
                    shutdown,
                )
                .await
            });
            let (started, running) = tokio::sync::oneshot::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            let running_worker = worker.clone();
            let first = tokio::spawn(async move {
                running_worker
                    .execute(move |_| {
                        started.send(()).unwrap();
                        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                        Ok(())
                    })
                    .await
            });
            running.await.unwrap();
            let queued_worker = worker.clone();
            let queued = tokio::spawn(async move { queued_worker.execute(|_| Ok(())).await });
            timeout(Duration::from_secs(2), async {
                while worker.stats().queued_operations != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let mut follower = TcpStream::connect(address).await?;
            write_replication_frame(&mut follower, b"127.0.0.1:7980").await?;
            timeout(Duration::from_secs(2), async {
                while worker.stats().waiting_operations != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(registry.list().len(), 1);
            if cancel_capture {
                signal.send(true).unwrap();
                timeout(Duration::from_secs(1), leader)
                    .await
                    .unwrap()
                    .unwrap()?;
                assert!(registry.list().is_empty());
                assert_eq!(worker.stats().waiting_operations, 0);
                release.send(()).unwrap();
            } else {
                release.send(()).unwrap();
                assert!(
                    timeout(
                        Duration::from_secs(2),
                        read_replication_frame(&mut follower)
                    )
                    .await
                    .unwrap()?
                    .len()
                        > 1
                );
                assert_eq!(
                    read_replication_frame(&mut follower).await?,
                    [SYNC_COMPLETE_MARKER]
                );
                assert_eq!(worker.stats().rejected_operations, 0);
                signal.send(true).unwrap();
                leader.await.unwrap()?;
            }
            first.await.unwrap()?;
            queued.await.unwrap()?;
            worker.shutdown().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn idle_disconnection_removes_follower_without_any_mutations() {
        let mut store = Store::with_config(StoreConfig {
            cleanup_interval_ms: 1,
            ..StoreConfig::default()
        })
        .unwrap();
        let broadcaster = Arc::new(ReplicationBroadcaster::new());
        store.set_broadcaster(Arc::clone(&broadcaster));
        let registry = Arc::new(FollowerRegistry::new());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let leader_registry = Arc::clone(&registry);
        let leader = tokio::spawn(async move {
            run_leader(&listener, store, broadcaster, leader_registry).await
        });

        let mut follower = TcpStream::connect(address).await.unwrap();
        write_replication_frame(&mut follower, b"127.0.0.1:7980")
            .await
            .unwrap();
        assert_eq!(
            read_replication_frame(&mut follower).await.unwrap(),
            [SYNC_COMPLETE_MARKER]
        );
        assert_eq!(registry.list().len(), 1);
        drop(follower);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !registry.list().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        leader.abort();
        let _ = leader.await;
    }

    #[tokio::test]
    async fn authentication_precedes_snapshot_and_shutdown_closes_followers() -> Result<()> {
        let mut store = Store::new();
        let broadcaster = Arc::new(ReplicationBroadcaster::new());
        store.set_broadcaster(broadcaster.clone());
        store.insert("private".into(), b"value".to_vec(), 0)?;
        let health = store.health();
        let worker = StorageWorker::new(store, 16)?;
        let registry = Arc::new(FollowerRegistry::new());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (signal, receiver) = watch::channel(false);
        let server_worker = worker.clone();
        let server_registry = registry.clone();
        let leader = tokio::spawn(async move {
            run_leader_until(
                &listener,
                server_worker,
                broadcaster,
                server_registry,
                ReplicationOptions {
                    transport: Transport::new(&SecurityConfig::default()).unwrap(),
                    auth_token: Some("replication-secret".into()),
                    max_connections: 2,
                    max_snapshot_bytes: 0,
                    write_timeout: Duration::from_secs(2),
                    shutdown_timeout: Duration::from_secs(2),
                    health,
                },
                receiver,
            )
            .await
        });
        let mut unauthenticated = TcpStream::connect(address).await?;
        write_replication_frame(&mut unauthenticated, b"127.0.0.1:7980").await?;
        assert_eq!(
            read_replication_frame(&mut unauthenticated).await?,
            b"error:unauthorized"
        );
        assert!(registry.list().is_empty());

        let mut follower = TcpStream::connect(address).await?;
        write_replication_frame(&mut follower, b"AUTH replication-secret").await?;
        assert_eq!(read_replication_frame(&mut follower).await?, b"ok");
        write_replication_frame(&mut follower, b"127.0.0.1:7980").await?;
        assert!(read_replication_frame(&mut follower).await?.len() > 1);
        assert_eq!(
            read_replication_frame(&mut follower).await?,
            [SYNC_COMPLETE_MARKER]
        );
        assert_eq!(registry.list().len(), 1);
        signal.send(true).unwrap();
        timeout(Duration::from_secs(2), leader)
            .await
            .unwrap()
            .unwrap()?;
        assert!(registry.list().is_empty());
        assert_eq!(follower.read(&mut [0u8; 1]).await?, 0);
        worker.shutdown().await?;
        Ok(())
    }
}
