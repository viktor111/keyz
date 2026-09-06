use std::sync::Arc;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::{watch, Semaphore},
    task::JoinSet,
    time::{timeout, Duration},
};

use crate::{
    config::{ProtocolConfig, SecurityConfig, ServerConfig},
    protocol,
    server::{
        dispatcher::{dispatch_request_with_close, error_code, public_error_message},
        error::{KeyzError, Result},
        helpers,
        store::Store,
        worker::StorageWorker,
    },
    transport::{authenticate, AsyncStream, Transport},
};

#[derive(Clone)]
pub struct ServerOptions {
    pub protocol: Arc<ProtocolConfig>,
    pub transport: Transport,
    pub auth_token: Option<String>,
    pub read_only: bool,
    pub max_connections: usize,
    pub write_timeout: Duration,
    pub shutdown_timeout: Duration,
}

struct ConnectionHealth(crate::server::health::Health);

impl Drop for ConnectionHealth {
    fn drop(&mut self) {
        self.0.connection_closed();
    }
}

pub(crate) async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

pub(crate) fn log_event(event: &str, message: &str) {
    eprintln!(
        "{}",
        serde_json::json!({"event": event, "message": message})
    );
}

/// Accept connections until shutdown, then finish commands already being executed.
pub async fn start_until(
    listener: &TcpListener,
    worker: StorageWorker,
    options: ServerOptions,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let health = worker.execute(|store| Ok(store.health())).await?;
    let connections = Arc::new(Semaphore::new(options.max_connections));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => break,
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = joined {
                    log_event("connection_task_failed", &error.to_string());
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
                    drop(stream);
                    health.connection_rejected();
                    log_event("connection_limit", "connection rejected");
                    continue;
                };
                let worker = worker.clone();
                let options = options.clone();
                let shutdown = shutdown.clone();
                health.connection_opened();
                let connection_health = ConnectionHealth(health.clone());
                tasks.spawn(async move {
                    let _permit = permit;
                    let _health = connection_health;
                    if let Err(error) = handle_connection(stream, worker, options, shutdown).await {
                        if !matches!(error, KeyzError::ClientDisconnected | KeyzError::ClientTimeout | KeyzError::Unauthorized) {
                            log_event("connection_failed", &error.to_string());
                        }
                    }
                });
            }
        }
    }
    if timeout(options.shutdown_timeout, async {
        while let Some(joined) = tasks.join_next().await {
            if let Err(error) = joined {
                log_event("connection_task_failed", &error.to_string());
            }
        }
    })
    .await
    .is_err()
    {
        log_event(
            "connection_drain_timeout",
            "closing remaining connections; accepted storage work will still drain",
        );
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    Ok(())
}

/// Compatibility entry point for embedded users without a shutdown signal.
pub async fn start(
    listener: &TcpListener,
    store: Store,
    protocol: Arc<ProtocolConfig>,
    read_only: bool,
) {
    let defaults = ServerConfig::default();
    let worker = match StorageWorker::new(store, defaults.work_queue_capacity) {
        Ok(worker) => worker,
        Err(error) => {
            log_event("worker_start_failed", &error.to_string());
            return;
        }
    };
    let options = ServerOptions {
        protocol,
        transport: Transport::new(&SecurityConfig::default()).expect("default transport"),
        auth_token: None,
        read_only,
        max_connections: defaults.max_connections,
        write_timeout: Duration::from_secs(defaults.write_timeout_secs),
        shutdown_timeout: Duration::from_secs(defaults.shutdown_timeout_secs),
    };
    let (_shutdown, receiver) = watch::channel(false);
    if let Err(error) = start_until(listener, worker.clone(), options, receiver).await {
        log_event("listener_failed", &error.to_string());
    }
    if let Err(error) = worker.shutdown().await {
        log_event("shutdown_failed", &error.to_string());
    }
}

async fn handle_connection(
    stream: TcpStream,
    worker: StorageWorker,
    options: ServerOptions,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut stream: AsyncStream = tokio::select! {
        _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
        accepted = timeout(options.write_timeout, options.transport.accept(stream)) => accepted.map_err(|_| KeyzError::ClientTimeout)??,
    };
    let idle_timeout = options.protocol.idle_timeout();
    if let Some(token) = &options.auth_token {
        let auth = tokio::select! {
            _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
            result = timeout(idle_timeout, helpers::read_message(&mut stream, 4101)) => result.map_err(|_| KeyzError::ClientTimeout)??,
        };
        if !auth
            .strip_prefix("AUTH ")
            .is_some_and(|supplied| authenticate(supplied, token))
        {
            send_response(&mut stream, "error:unauthorized", options.write_timeout).await?;
            return Err(KeyzError::Unauthorized);
        }
        send_response(&mut stream, "ok", options.write_timeout).await?;
    }
    loop {
        let command = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
            result = timeout(idle_timeout, helpers::read_message(&mut stream, options.protocol.max_message_bytes)) => {
                match result {
                    Ok(Ok(command)) => command,
                    Ok(Err(error @ KeyzError::InvalidCommand(_))) => {
                        send_response(&mut stream, &options.protocol.invalid_command_response, options.write_timeout).await?;
                        // A write-half shutdown delivers the error before unread payload bytes
                        // cause the socket close to reset the connection on Windows.
                        timeout(options.write_timeout, stream.shutdown()).await.map_err(|_| KeyzError::ClientTimeout)??;
                        return Err(error);
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        let _ = send_response(&mut stream, &options.protocol.timeout_response, options.write_timeout).await;
                        return Err(KeyzError::ClientTimeout);
                    }
                }
            }
        };
        let config = Arc::clone(&options.protocol);
        let read_only = options.read_only;
        let error_command = command.clone();
        // Accepted operations run to completion even if shutdown or peer closure follows.
        let (mut response, close) = match worker
            .execute(move |store| dispatch_request_with_close(command, store, &config, read_only))
            .await
        {
            Ok(response) => response,
            Err(KeyzError::InvalidCommand(_)) => (
                protocol::error_response(
                    &error_command,
                    "invalid_command",
                    &options.protocol.invalid_command_response,
                ),
                false,
            ),
            Err(KeyzError::Overloaded) => (
                protocol::error_response(&error_command, "overloaded", "error:overloaded"),
                false,
            ),
            Err(KeyzError::ShuttingDown) => (
                protocol::error_response(&error_command, "shutting_down", "error:shutting down"),
                false,
            ),
            Err(error) => {
                log_event("request_failed", &error.to_string());
                (
                    protocol::error_response(
                        &error_command,
                        error_code(&error),
                        &public_error_message(&error),
                    ),
                    false,
                )
            }
        };
        if response.len() > options.protocol.max_message_bytes as usize {
            response = protocol::error_response(
                &error_command,
                "response_too_large",
                "response exceeds max_message_bytes",
            );
            if response.len() > options.protocol.max_message_bytes as usize {
                return Err(KeyzError::InvalidCommand(
                    "response exceeds frame limit".into(),
                ));
            }
        }
        send_response(&mut stream, &response, options.write_timeout).await?;
        if close {
            timeout(options.write_timeout, stream.shutdown())
                .await
                .map_err(|_| KeyzError::ClientTimeout)??;
            return Ok(());
        }
    }
}

async fn send_response(
    stream: &mut AsyncStream,
    message: &str,
    write_timeout: Duration,
) -> Result<()> {
    timeout(write_timeout, helpers::write_message(stream, message))
        .await
        .map_err(|_| KeyzError::ClientTimeout)?
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn connection(
        store: Store,
        protocol: ProtocolConfig,
    ) -> (TcpStream, tokio::task::JoinHandle<Result<()>>) {
        connection_with_options(store, test_options(protocol)).await
    }

    fn test_options(protocol: ProtocolConfig) -> ServerOptions {
        ServerOptions {
            protocol: Arc::new(protocol),
            transport: Transport::new(&SecurityConfig::default()).unwrap(),
            auth_token: None,
            read_only: false,
            max_connections: 1,
            write_timeout: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(2),
        }
    }

    async fn connection_with_options(
        store: Store,
        options: ServerOptions,
    ) -> (TcpStream, tokio::task::JoinHandle<Result<()>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let worker = StorageWorker::new(store, 8).unwrap();
        let (signal, receiver) = watch::channel(false);
        let handle = tokio::spawn(async move {
            let _signal = signal;
            let result = handle_connection(server, worker.clone(), options, receiver).await;
            worker.shutdown().await?;
            result
        });
        (client, handle)
    }

    #[tokio::test]
    async fn oversized_payload_is_rejected_and_never_dispatched() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig {
            max_message_bytes: 64,
            ..ProtocolConfig::default()
        };
        let (mut client, handler) = connection(store.clone(), protocol).await;
        let embedded = b"SET injected yes";
        let mut frame = 65u32.to_be_bytes().to_vec();
        frame.extend_from_slice(&(embedded.len() as u32).to_be_bytes());
        frame.extend_from_slice(embedded);
        frame.resize(4 + 65, 0);
        client.write_all(&frame).await?;

        assert_eq!(
            helpers::read_message(&mut client, 1024).await?,
            "error:invalid command"
        );
        let result = timeout(Duration::from_secs(2), handler)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(KeyzError::InvalidCommand(_))));
        assert_eq!(store.get("injected")?, None);
        let closed = timeout(Duration::from_secs(2), client.read(&mut [0u8; 4]))
            .await
            .unwrap();
        assert!(matches!(closed, Ok(0) | Err(_)));
        Ok(())
    }

    #[tokio::test]
    async fn invalid_command_and_ttl_overflow_leave_connection_usable() -> Result<()> {
        let store = Store::new();
        let protocol = ProtocolConfig {
            invalid_command_response: "error:bad request".into(),
            ..ProtocolConfig::default()
        };
        let (mut client, handler) = connection(store.clone(), protocol).await;
        helpers::write_message(&mut client, "SET huge value EX 18446744073709551615").await?;
        assert_eq!(
            helpers::read_message(&mut client, 1024).await?,
            "error:bad request"
        );
        assert_eq!(store.get("huge")?, None);
        helpers::write_message(&mut client, "SET valid value").await?;
        assert_eq!(helpers::read_message(&mut client, 1024).await?, "ok");
        helpers::write_message(&mut client, "GET valid").await?;
        assert_eq!(helpers::read_message(&mut client, 1024).await?, "value");
        helpers::write_message(&mut client, "CLOSE").await?;
        assert_eq!(
            helpers::read_message(&mut client, 1024).await?,
            "Closing connection"
        );
        timeout(Duration::from_secs(2), handler)
            .await
            .unwrap()
            .unwrap()?;
        Ok(())
    }

    #[tokio::test]
    async fn authentication_rejects_commands_before_dispatch() -> Result<()> {
        let store = Store::new();
        let mut options = test_options(ProtocolConfig::default());
        options.auth_token = Some("correct-token".into());
        let (mut client, handler) = connection_with_options(store.clone(), options.clone()).await;
        helpers::write_message(&mut client, "SET injected value").await?;
        assert_eq!(
            helpers::read_message(&mut client, 1024).await?,
            "error:unauthorized"
        );
        assert!(matches!(
            handler.await.unwrap(),
            Err(KeyzError::Unauthorized)
        ));
        assert_eq!(store.get("injected")?, None);

        let (mut client, handler) = connection_with_options(Store::new(), options).await;
        helpers::write_message(&mut client, "AUTH correct-token").await?;
        assert_eq!(helpers::read_message(&mut client, 1024).await?, "ok");
        helpers::write_message(&mut client, "SET authenticated value").await?;
        assert_eq!(helpers::read_message(&mut client, 1024).await?, "ok");
        helpers::write_message(&mut client, "CLOSE").await?;
        assert_eq!(
            helpers::read_message(&mut client, 1024).await?,
            "Closing connection"
        );
        handler.await.unwrap()?;
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_closes_idle_connections_and_listener_tasks() -> Result<()> {
        let store = Store::new();
        let worker = StorageWorker::new(store, 8)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut client = TcpStream::connect(listener.local_addr()?).await?;
        let (signal, receiver) = watch::channel(false);
        let serving_worker = worker.clone();
        let task = tokio::spawn(async move {
            start_until(
                &listener,
                serving_worker,
                test_options(ProtocolConfig::default()),
                receiver,
            )
            .await
        });
        helpers::write_message(&mut client, "GET absent").await?;
        assert_eq!(helpers::read_message(&mut client, 1024).await?, "null");
        signal.send(true).unwrap();
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()?;
        assert_eq!(client.read(&mut [0u8; 1]).await?, 0);
        worker.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn connection_limit_rejects_extra_connections() -> Result<()> {
        let store = Store::new();
        let worker = StorageWorker::new(store, 8)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (signal, receiver) = watch::channel(false);
        let serving_worker = worker.clone();
        let task = tokio::spawn(async move {
            start_until(
                &listener,
                serving_worker,
                test_options(ProtocolConfig::default()),
                receiver,
            )
            .await
        });
        let mut first = TcpStream::connect(addr).await?;
        helpers::write_message(&mut first, "GET absent").await?;
        helpers::read_message(&mut first, 1024).await?;
        let mut second = TcpStream::connect(addr).await?;
        let closed = timeout(Duration::from_secs(2), second.read(&mut [0u8; 1]))
            .await
            .unwrap();
        assert!(matches!(closed, Ok(0) | Err(_)));
        signal.send(true).unwrap();
        task.await.unwrap()?;
        worker.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn oversized_typed_response_preserves_request_id_and_connection() -> Result<()> {
        let store = Store::new();
        store.insert("large".into(), vec![b'x'; 400], 0)?;
        let (mut client, task) = connection(
            store,
            ProtocolConfig {
                max_message_bytes: 512,
                ..ProtocolConfig::default()
            },
        )
        .await;
        let request = protocol::encode_request(&protocol::Request {
            id: 77,
            command: protocol::Command::Get {
                key: "large".into(),
            },
        });
        helpers::write_message(&mut client, &request).await?;
        let reply =
            protocol::decode_response(&helpers::read_message(&mut client, 512).await?).unwrap();
        assert_eq!(reply.id, Some(77));
        assert!(
            matches!(reply.result, protocol::ResponseBody::Error { code, .. } if code == "response_too_large")
        );
        helpers::write_message(&mut client, "GET large").await?;
        assert_eq!(
            helpers::read_message(&mut client, 512).await?,
            "x".repeat(400)
        );
        helpers::write_message(&mut client, "CLOSE").await?;
        helpers::read_message(&mut client, 512).await?;
        task.await.unwrap()?;
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_completes_an_already_accepted_write() -> Result<()> {
        let store = Store::new();
        let worker = StorageWorker::new(store.clone(), 4)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (signal, receiver) = watch::channel(false);
        let serving_worker = worker.clone();
        let server = tokio::spawn(async move {
            start_until(
                &listener,
                serving_worker,
                test_options(ProtocolConfig::default()),
                receiver,
            )
            .await
        });
        let mut client = TcpStream::connect(address).await?;
        helpers::write_message(&mut client, "GET absent").await?;
        helpers::read_message(&mut client, 1024).await?;
        let (started, blocked) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let blocking_worker = worker.clone();
        let blocker = tokio::spawn(async move {
            blocking_worker
                .execute(move |_| {
                    started.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                })
                .await
        });
        blocked.await.unwrap();
        helpers::write_message(&mut client, "SET accepted value").await?;
        timeout(Duration::from_secs(2), async {
            while worker.stats().queued_operations == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        signal.send(true).unwrap();
        release.send(()).unwrap();
        assert_eq!(helpers::read_message(&mut client, 1024).await?, "ok");
        blocker.await.unwrap()?;
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()?;
        worker.shutdown().await?;
        assert_eq!(store.get("accepted")?, Some(b"value".to_vec()));
        Ok(())
    }
}
