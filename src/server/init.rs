use std::sync::Arc;

use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    time::{sleep, timeout, Duration},
};

use crate::{
    config::ProtocolConfig,
    server::{
        dispatcher::dispatcher,
        error::{KeyzError, Result},
        helpers,
        store::Store,
    },
};

const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

pub async fn start(listener: &TcpListener, store: Store, protocol: Arc<ProtocolConfig>) {
    loop {
        match helpers::listener_accept_conn(listener).await {
            Ok((stream, _addr)) => {
                let store = store.clone();
                let protocol = Arc::clone(&protocol);
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(stream, store, protocol).await {
                        if !matches!(
                            err,
                            KeyzError::ClientDisconnected | KeyzError::ClientTimeout
                        ) {
                            eprintln!("connection terminated with error: {err}");
                        }
                    }
                });
            }
            Err(err) => {
                eprintln!("listener accept error: {err}");
                sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    store: Store,
    protocol: Arc<ProtocolConfig>,
) -> Result<()> {
    let idle_timeout = protocol.idle_timeout();
    let max_len = protocol.max_message_bytes;
    let close_command = protocol.close_command.clone();
    let timeout_response = protocol.timeout_response.clone();
    let invalid_response = protocol.invalid_command_response.clone();

    loop {
        let command = match timeout(idle_timeout, helpers::read_message(&mut stream, max_len)).await
        {
            Ok(Ok(command)) => command,
            Ok(Err(KeyzError::ClientDisconnected)) => return Err(KeyzError::ClientDisconnected),
            Ok(Err(KeyzError::InvalidCommand(_))) => {
                send_response(&mut stream, &invalid_response).await?;
                continue;
            }
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                let _ = send_response(&mut stream, &timeout_response).await;
                return Err(KeyzError::ClientTimeout);
            }
        };

        if command.trim().is_empty() {
            send_response(&mut stream, &invalid_response).await?;
            continue;
        }

        if command == close_command {
            send_response(&mut stream, "Closing connection").await?;
            stream.shutdown().await.map_err(KeyzError::from)?;
            return Ok(());
        }

        let response = match dispatcher(command, &store, protocol.as_ref()).await {
            Ok(response) => response,
            Err(KeyzError::InvalidCommand(_)) => invalid_response.clone(),
            Err(err) => return Err(err),
        };

        send_response(&mut stream, &response).await?;
    }
}

async fn send_response(stream: &mut TcpStream, message: &str) -> Result<()> {
    helpers::write_message(stream, message).await
}
