use std::io::Cursor;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::timeout;

use crate::config::SecurityConfig;
use crate::server::backend::aof::AofRecord;
use crate::server::error::{KeyzError, Result};
use crate::server::init::{log_event, wait_for_shutdown};
use crate::server::store::Store;
use crate::server::worker::StorageWorker;
use crate::transport::Transport;

use super::{
    is_sync_complete, read_live_replication_frame, read_replication_frame, write_replication_frame,
    ReplicationOptions,
};

pub async fn run_follower(leader_addr: SocketAddr, server_addr: SocketAddr, store: Store) {
    let options = default_options(&store);
    let Ok(worker) = StorageWorker::new(store, 1024) else {
        return;
    };
    let (_signal, receiver) = watch::channel(false);
    let _ = run_follower_until(
        leader_addr.to_string(),
        server_addr,
        worker.clone(),
        options,
        receiver,
    )
    .await;
    if let Err(error) = worker.shutdown().await {
        log_event("replication_shutdown_failed", &error.to_string());
    }
}

fn default_options(store: &Store) -> ReplicationOptions {
    ReplicationOptions {
        transport: Transport::new(&SecurityConfig::default()).expect("default transport"),
        auth_token: None,
        max_connections: 1024,
        max_snapshot_bytes: 0,
        write_timeout: Duration::from_secs(30),
        shutdown_timeout: Duration::from_secs(30),
        health: store.health(),
    }
}

pub async fn run_follower_until(
    leader_address: String,
    server_addr: SocketAddr,
    worker: StorageWorker,
    options: ReplicationOptions,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        options.health.set_replication_state("connecting");
        let result = replicate_session(
            &leader_address,
            server_addr,
            &worker,
            &options,
            shutdown.clone(),
        )
        .await;
        if *shutdown.borrow() {
            return Ok(());
        }
        let error = result
            .err()
            .map_or_else(|| "leader disconnected".into(), |e| e.to_string());
        options.health.replication_failure(&error);
        log_event("replication_reconnect", &error);
        tokio::select! {
            _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {},
        }
    }
}

#[cfg(test)]
async fn connect_and_replicate(
    leader_addr: SocketAddr,
    server_addr: SocketAddr,
    store: &Store,
) -> Result<()> {
    let worker = StorageWorker::new(store.clone(), 1024)?;
    let options = default_options(store);
    let (_signal, receiver) = watch::channel(false);
    let result = replicate_session(
        &leader_addr.to_string(),
        server_addr,
        &worker,
        &options,
        receiver,
    )
    .await;
    worker.shutdown().await?;
    result
}

async fn replicate_session(
    leader_address: &str,
    server_addr: SocketAddr,
    worker: &StorageWorker,
    options: &ReplicationOptions,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let handshake = async {
        let mut stream = options.transport.connect(leader_address).await?;
        if let Some(token) = &options.auth_token {
            crate::server::helpers::write_message(&mut stream, &format!("AUTH {token}")).await?;
            if crate::server::helpers::read_message(&mut stream, 1024).await? != "ok" {
                return Err(KeyzError::Unauthorized);
            }
        }
        write_replication_frame(&mut stream, server_addr.to_string().as_bytes()).await?;
        Ok::<_, KeyzError>(stream)
    };
    let mut stream = tokio::select! {
        _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
        result = timeout(options.write_timeout, handshake) => result.map_err(|_| KeyzError::ClientTimeout)??,
    };

    options.health.set_replication_state("syncing");

    // Publish only a complete snapshot; interrupted resyncs retain the last dataset.
    let mut snapshot = Vec::new();
    let mut snapshot_bytes = 0u64;
    loop {
        let frame = tokio::select! {
            _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
            result = timeout(options.write_timeout, read_replication_frame(&mut stream)) => result.map_err(|_| KeyzError::ClientTimeout)??,
        };
        if is_sync_complete(&frame) {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
                result = worker.execute_wait(move |store| store.replace_replicated_snapshot(snapshot)) => result?,
            }
            options.health.set_replication_state("ready");
            log_event("replication_snapshot_complete", "full snapshot committed");
            break;
        }
        let record = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
            result = worker.execute_wait(move |_| decode_record(&frame)) => result?,
        };
        match record {
            AofRecord::Set {
                key,
                value,
                expires_at,
                compressed,
            } => {
                snapshot_bytes = snapshot_bytes
                    .saturating_add(key.len() as u64)
                    .saturating_add(value.len() as u64);
                if options.max_snapshot_bytes > 0 && snapshot_bytes > options.max_snapshot_bytes {
                    return Err(KeyzError::CapacityExceeded {
                        used_bytes: 0,
                        requested_bytes: snapshot_bytes,
                        max_bytes: options.max_snapshot_bytes,
                    });
                }
                let expires_at = if expires_at == 0 {
                    None
                } else {
                    Some(expires_at)
                };
                snapshot.push((key, value, expires_at, compressed));
            }
            AofRecord::Del { .. } => {
                return Err(KeyzError::CorruptDataFile(
                    "unexpected deletion in replication snapshot".into(),
                ));
            }
        }
    }

    // Phase 2: Live stream
    loop {
        let frame = tokio::select! {
            _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
            result = read_live_replication_frame(&mut stream, options.write_timeout) => result?,
        };
        tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
            result = worker.execute_wait(move |store| apply_record(decode_record(&frame)?, store)) => result?,
        }
        options.health.applied();
    }
}

fn decode_record(frame: &[u8]) -> Result<AofRecord> {
    let mut cursor = Cursor::new(frame);
    let record = AofRecord::deserialize(&mut cursor)?
        .ok_or_else(|| KeyzError::CorruptDataFile("incomplete replication record".into()))?;
    if cursor.position() as usize != frame.len() {
        return Err(KeyzError::CorruptDataFile(
            "trailing data in replication record".into(),
        ));
    }
    Ok(record)
}

fn apply_record(record: AofRecord, store: &Store) -> Result<()> {
    match record {
        AofRecord::Set {
            key,
            value,
            expires_at,
            compressed,
        } => store.apply_replicated_insert(key, value, expires_at, compressed),
        AofRecord::Del { key } => {
            store.delete(&key)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StoreConfig;
    use crate::server::replication::SYNC_COMPLETE_MARKER;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;
    use tokio::task::JoinHandle;

    fn test_store() -> Store {
        Store::with_config(StoreConfig {
            cleanup_interval_ms: 1,
            ..StoreConfig::default()
        })
        .unwrap()
    }

    fn set_frame(key: &str, value: &[u8]) -> Vec<u8> {
        AofRecord::Set {
            key: key.into(),
            value: value.to_vec(),
            expires_at: 0,
            compressed: false,
        }
        .serialize()
    }

    async fn wait_until(check: impl Fn() -> bool) {
        timeout(Duration::from_secs(2), async {
            while !check() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    async fn live_follower(
        store: &Store,
        worker: &StorageWorker,
        frame_timeout: Duration,
    ) -> (TcpStream, watch::Sender<bool>, JoinHandle<Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let mut options = default_options(store);
        options.write_timeout = frame_timeout;
        let (signal, shutdown) = watch::channel(false);
        let session_worker = worker.clone();
        let task = tokio::spawn(async move {
            run_follower_until(
                address,
                "127.0.0.1:7980".parse().unwrap(),
                session_worker,
                options,
                shutdown,
            )
            .await
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        read_replication_frame(&mut stream).await.unwrap();
        write_replication_frame(&mut stream, &[SYNC_COMPLETE_MARKER])
            .await
            .unwrap();
        wait_until(|| store.health().snapshot().replication_state == "ready").await;
        (stream, signal, task)
    }

    #[tokio::test]
    async fn incomplete_live_frames_clear_readiness_and_trigger_reconnection() {
        for prefix in [&[0][..], &[0, 0, 0, 100, 1][..]] {
            let store = test_store();
            let worker = StorageWorker::new(store.clone(), 1).unwrap();
            let (mut stream, signal, task) =
                live_follower(&store, &worker, Duration::from_millis(150)).await;
            // A completely idle connection is healthy beyond the frame timeout.
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(store.health().snapshot().ready);
            assert_eq!(store.health().snapshot().reconnects, 0);
            stream.write_all(prefix).await.unwrap();
            wait_until(|| store.health().snapshot().reconnects == 1).await;
            assert!(!store.health().snapshot().ready);
            assert_eq!(store.health().snapshot().replication_state, "disconnected");
            signal.send(true).unwrap();
            finish(task).await.unwrap();
            worker.shutdown().await.unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_full_queue_backpressures_live_replication_without_resynchronizing() {
        let store = test_store();
        let worker = StorageWorker::new(store.clone(), 1).unwrap();
        let (mut stream, signal, task) =
            live_follower(&store, &worker, Duration::from_secs(2)).await;
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
        wait_until(|| worker.stats().queued_operations == 1).await;
        write_replication_frame(&mut stream, &set_frame("live", b"queued update"))
            .await
            .unwrap();
        // Observe actual waiting admission instead of relying on socket timing.
        wait_until(|| worker.stats().waiting_operations == 1).await;
        assert_eq!(store.get("live").unwrap(), None);
        assert!(store.health().snapshot().ready);
        assert_eq!(store.health().snapshot().reconnects, 0);
        assert_eq!(worker.stats().rejected_operations, 0);
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        queued.await.unwrap().unwrap();
        wait_until(|| store.health().snapshot().applied_records == 1).await;
        assert_eq!(store.get("live").unwrap(), Some(b"queued update".to_vec()));
        assert_eq!(store.health().snapshot().reconnects, 0);
        signal.send(true).unwrap();
        finish(task).await.unwrap();
        worker.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_cancels_replication_before_waiting_queue_admission() {
        let store = test_store();
        let worker = StorageWorker::new(store.clone(), 1).unwrap();
        let (mut stream, signal, task) =
            live_follower(&store, &worker, Duration::from_secs(2)).await;
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
        wait_until(|| worker.stats().queued_operations == 1).await;
        write_replication_frame(&mut stream, &set_frame("cancelled", b"not admitted"))
            .await
            .unwrap();
        wait_until(|| worker.stats().waiting_operations == 1).await;
        signal.send(true).unwrap();
        finish(task).await.unwrap();
        assert_eq!(worker.stats().waiting_operations, 0);
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        queued.await.unwrap().unwrap();
        worker.shutdown().await.unwrap();
        assert_eq!(store.get("cancelled").unwrap(), None);
    }

    async fn connect(store: &Store) -> (TcpStream, JoinHandle<Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let leader_addr = listener.local_addr().unwrap();
        let replica = store.clone();
        let task = tokio::spawn(async move {
            connect_and_replicate(leader_addr, "127.0.0.1:7980".parse().unwrap(), &replica).await
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let handshake = read_replication_frame(&mut stream).await.unwrap();
        assert_eq!(handshake, b"127.0.0.1:7980");
        (stream, task)
    }

    async fn finish(task: JoinHandle<Result<()>>) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
    }

    async fn wait_for_key(store: &Store, key: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.get(key).unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn resync_replaces_old_dataset_then_applies_live_records() {
        let store = test_store();
        store.insert("ghost".into(), b"old".to_vec(), 0).unwrap();
        let (mut stream, task) = connect(&store).await;
        write_replication_frame(&mut stream, &set_frame("fresh", b"snapshot"))
            .await
            .unwrap();
        write_replication_frame(&mut stream, &[SYNC_COMPLETE_MARKER])
            .await
            .unwrap();
        write_replication_frame(&mut stream, &set_frame("live", b"after snapshot"))
            .await
            .unwrap();
        wait_for_key(&store, "live").await;
        assert_eq!(store.get("ghost").unwrap(), None);
        assert_eq!(store.get("fresh").unwrap(), Some(b"snapshot".to_vec()));
        drop(stream);
        assert!(finish(task).await.is_err());

        // The next full synchronization may legitimately contain no entries.
        let (mut stream, task) = connect(&store).await;
        write_replication_frame(&mut stream, &[SYNC_COMPLETE_MARKER])
            .await
            .unwrap();
        drop(stream);
        assert!(finish(task).await.is_err());
        assert_eq!(store.len(), 0);
    }

    #[tokio::test]
    async fn interrupted_snapshot_leaves_entire_previous_dataset_intact() {
        let store = test_store();
        store.insert("key".into(), b"old".to_vec(), 0).unwrap();
        let (mut stream, task) = connect(&store).await;
        write_replication_frame(&mut stream, &set_frame("key", b"partial replacement"))
            .await
            .unwrap();
        write_replication_frame(&mut stream, &set_frame("new", b"partial addition"))
            .await
            .unwrap();
        drop(stream);
        assert!(finish(task).await.is_err());
        assert_eq!(store.get("key").unwrap(), Some(b"old".to_vec()));
        assert_eq!(store.get("new").unwrap(), None);
    }

    #[tokio::test]
    async fn malformed_snapshots_are_rejected_without_publishing_partial_state() {
        let good = set_frame("bad", b"value");
        let mut corrupt_crc = good.clone();
        *corrupt_crc.last_mut().unwrap() ^= 1;
        let mut trailing = good.clone();
        trailing.push(0);
        let bad_frames = [
            Vec::new(),
            good[..good.len() - 1].to_vec(),
            corrupt_crc,
            trailing,
            AofRecord::Del { key: "old".into() }.serialize(),
        ];
        for bad in bad_frames {
            let store = test_store();
            store.insert("old".into(), b"retained".to_vec(), 0).unwrap();
            let (mut stream, task) = connect(&store).await;
            write_replication_frame(&mut stream, &set_frame("new", b"pending"))
                .await
                .unwrap();
            write_replication_frame(&mut stream, &bad).await.unwrap();
            assert!(matches!(
                finish(task).await,
                Err(KeyzError::CorruptDataFile(_))
            ));
            assert_eq!(store.get("old").unwrap(), Some(b"retained".to_vec()));
            assert_eq!(store.get("new").unwrap(), None);
        }
    }

    #[tokio::test]
    async fn corrupt_live_record_ends_session_for_full_resynchronization() {
        let store = test_store();
        let (mut stream, task) = connect(&store).await;
        write_replication_frame(&mut stream, &[SYNC_COMPLETE_MARKER])
            .await
            .unwrap();
        write_replication_frame(&mut stream, &set_frame("live", b"valid"))
            .await
            .unwrap();
        wait_for_key(&store, "live").await;
        write_replication_frame(&mut stream, &[]).await.unwrap();
        assert!(matches!(
            finish(task).await,
            Err(KeyzError::CorruptDataFile(_))
        ));
        assert_eq!(store.get("live").unwrap(), Some(b"valid".to_vec()));
    }

    #[tokio::test]
    async fn full_resync_removes_old_persisted_keys_after_restart() {
        use crate::config::{AofFsyncPolicy, BackendType, DiskConfig};

        for aof_enabled in [false, true] {
            let directory =
                std::env::temp_dir().join(format!("keyz_replica_resync_{}", rand::random::<u64>()));
            let config = StoreConfig {
                backend: BackendType::Disk,
                cleanup_interval_ms: 1,
                disk: DiskConfig {
                    data_dir: directory.to_string_lossy().into_owned(),
                    partition_count: 4,
                    aof_enabled,
                    aof_fsync: AofFsyncPolicy::EveryWrite,
                    ..DiskConfig::default()
                },
                ..StoreConfig::default()
            };
            let store = Store::with_config(config.clone()).unwrap();
            store.insert("ghost".into(), b"old".to_vec(), 0).unwrap();
            drop(store);

            let store = Store::with_config(config.clone()).unwrap();
            let (mut stream, task) = connect(&store).await;
            write_replication_frame(&mut stream, &set_frame("fresh", b"replacement"))
                .await
                .unwrap();
            write_replication_frame(&mut stream, &[SYNC_COMPLETE_MARKER])
                .await
                .unwrap();
            drop(stream);
            assert!(finish(task).await.is_err());
            assert_eq!(store.get("ghost").unwrap(), None);
            drop(store);

            let recovered = Store::with_config(config).unwrap();
            assert_eq!(recovered.get("ghost").unwrap(), None);
            assert_eq!(
                recovered.get("fresh").unwrap(),
                Some(b"replacement".to_vec())
            );
            drop(recovered);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn health_tracks_complete_snapshot_live_records_and_cancellable_idle_stream() -> Result<()>
    {
        let store = test_store();
        let health = store.health();
        let worker = StorageWorker::new(store.clone(), 16)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let (signal, receiver) = watch::channel(false);
        let options = default_options(&store);
        let session_worker = worker.clone();
        let task = tokio::spawn(async move {
            replicate_session(
                &address,
                "127.0.0.1:7980".parse().unwrap(),
                &session_worker,
                &options,
                receiver,
            )
            .await
        });
        let (mut stream, _) = listener.accept().await?;
        read_replication_frame(&mut stream).await?;
        write_replication_frame(&mut stream, &set_frame("snapshot", b"value")).await?;
        assert!(!health.snapshot().ready);
        assert_eq!(health.snapshot().replication_state, "syncing");
        write_replication_frame(&mut stream, &[SYNC_COMPLETE_MARKER]).await?;
        write_replication_frame(&mut stream, &set_frame("live", b"value")).await?;
        timeout(Duration::from_secs(2), async {
            while health.snapshot().applied_records == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(health.snapshot().ready);
        assert!(health.snapshot().last_apply_unix_secs.is_some());
        signal.send(true).unwrap();
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()?;
        worker.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn oversized_staged_snapshot_does_not_replace_existing_data() -> Result<()> {
        let store = test_store();
        store.insert("old".into(), b"value".to_vec(), 0)?;
        let worker = StorageWorker::new(store.clone(), 16)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?.to_string();
        let (_signal, receiver) = watch::channel(false);
        let mut options = default_options(&store);
        options.max_snapshot_bytes = 4;
        let session_worker = worker.clone();
        let task = tokio::spawn(async move {
            replicate_session(
                &address,
                "127.0.0.1:7980".parse().unwrap(),
                &session_worker,
                &options,
                receiver,
            )
            .await
        });
        let (mut stream, _) = listener.accept().await?;
        read_replication_frame(&mut stream).await?;
        write_replication_frame(&mut stream, &set_frame("new", b"large value")).await?;
        assert!(matches!(
            timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap(),
            Err(KeyzError::CapacityExceeded { .. })
        ));
        assert_eq!(store.get("old")?, Some(b"value".to_vec()));
        assert_eq!(store.get("new")?, None);
        worker.shutdown().await?;
        Ok(())
    }
}
