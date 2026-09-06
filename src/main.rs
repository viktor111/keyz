use std::{process::ExitCode, sync::Arc, time::Duration};

use keyz::{
    config::{Config, ReplicationRole},
    server::{
        error::{KeyzError, Result},
        helpers::create_listener,
        init::{start_until, ServerOptions},
        replication::{
            follower::run_follower_until,
            leader::{run_leader_until, FollowerRegistry, ReplicationBroadcaster},
            ReplicationOptions,
        },
        store::Store,
        worker::StorageWorker,
    },
    transport::Transport,
};
use tokio::{sync::watch, task::JoinSet};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            event("server_failed", &error.to_string());
            ExitCode::FAILURE
        }
    }
}

fn event(name: &str, message: &str) {
    eprintln!("{}", serde_json::json!({"event": name, "message": message}));
}

async fn run() -> Result<()> {
    let mut config = Config::load::<&str>(None)?;
    config.validate()?;
    let transport = Transport::new(&config.security)?;
    let listener = create_listener(config.server.socket_addr()?).await?;
    let socket_addr = listener.local_addr()?;
    let replication_listener = if config.replication.role == ReplicationRole::Leader {
        Some(create_listener(config.replication.bind_addr()?).await?)
    } else {
        None
    };

    let mut store = Store::with_config(config.store.clone())?;
    store.set_replication_role(config.replication.role.clone());
    let health = store.health();
    let broadcaster = Arc::new(ReplicationBroadcaster::new());
    let registry = Arc::new(FollowerRegistry::new());
    match config.replication.role {
        ReplicationRole::Leader => {
            store.validate_replication_entries()?;
            store.set_broadcaster(Arc::clone(&broadcaster));
            store.set_follower_registry(Arc::clone(&registry));
            health.set_replication_state("leader");
        }
        ReplicationRole::Follower => health.set_replication_state("connecting"),
        ReplicationRole::Disabled => {}
    }
    let worker = StorageWorker::new(store, config.server.work_queue_capacity)?;
    let server_options = ServerOptions {
        protocol: Arc::new(config.protocol.clone()),
        transport: transport.clone(),
        auth_token: config.security.resolve_auth_token(),
        read_only: config.replication.role == ReplicationRole::Follower,
        max_connections: config.server.max_connections,
        write_timeout: Duration::from_secs(config.server.write_timeout_secs),
        shutdown_timeout: Duration::from_secs(config.server.shutdown_timeout_secs),
    };
    let replication_options = ReplicationOptions {
        transport,
        auth_token: config.security.resolve_replication_token(),
        max_connections: config.server.max_connections,
        max_snapshot_bytes: config.store.max_bytes,
        write_timeout: server_options.write_timeout,
        shutdown_timeout: server_options.shutdown_timeout,
        health: health.clone(),
    };
    let (shutdown, receiver) = watch::channel(false);
    let mut tasks = JoinSet::<Result<()>>::new();
    if let Some(listener) = replication_listener {
        let worker = worker.clone();
        let receiver = receiver.clone();
        tasks.spawn(async move {
            run_leader_until(
                &listener,
                worker,
                broadcaster,
                registry,
                replication_options,
                receiver,
            )
            .await
        });
    } else if config.replication.role == ReplicationRole::Follower {
        // Preserve the configured hostname for TLS certificate verification.
        let leader =
            config.replication.leader_address.clone().ok_or_else(|| {
                KeyzError::InvalidConfig("follower requires leader_address".into())
            })?;
        let worker = worker.clone();
        let receiver = receiver.clone();
        tasks.spawn(async move {
            run_follower_until(leader, socket_addr, worker, replication_options, receiver).await
        });
    }
    let serving_worker = worker.clone();
    tasks.spawn(
        async move { start_until(&listener, serving_worker, server_options, receiver).await },
    );
    event("server_listening", &socket_addr.to_string());

    let mut failure = tokio::select! {
        result = shutdown_signal() => result.err(),
        result = tasks.join_next() => Some(match result {
            Some(Ok(Err(error))) => error,
            Some(Err(error)) => KeyzError::Backend(format!("server task failed: {error}")),
            _ => KeyzError::Backend("server task stopped unexpectedly".into()),
        }),
    };
    health.stopping();
    event("server_stopping", "draining connections and storage work");
    let _ = shutdown.send(true);
    while let Some(result) = tasks.join_next().await {
        let error = match result {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => Some(KeyzError::Backend(format!("server task failed: {error}"))),
        };
        if failure.is_none() {
            failure = error;
        }
    }
    // The storage thread drains accepted work and performs its final durable flush.
    if let Err(error) = worker.shutdown().await {
        event("storage_shutdown_failed", &error.to_string());
        failure = Some(error);
    }
    match failure {
        Some(error) => Err(error),
        None => {
            event("server_stopped", "storage flush completed");
            Ok(())
        }
    }
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(windows)]
    {
        let mut interrupt = tokio::signal::windows::ctrl_break()?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = interrupt.recv() => {},
        }
    }
    #[cfg(not(any(unix, windows)))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
