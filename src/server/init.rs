use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    time::{sleep, timeout, Duration},
};

use crate::server::{
    dispatcher::dispatcher,
    error::{KeyzError, Result},
    helpers,
    store::Store,
};

const CLIENT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

pub async fn start(listener: &TcpListener) {
    let store = Store::new();
    loop {
        match helpers::listener_accept_conn(listener).await {
            Ok((stream, _addr)) => {
                let store = store.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_connection(stream, store).await {
                        if !matches!(err, KeyzError::ClientDisconnected | KeyzError::ClientTimeout)
                        {
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

async fn handle_connection(mut stream: TcpStream, store: Store) -> Result<()> {
    loop {
        let command =
            match timeout(CLIENT_IDLE_TIMEOUT, helpers::read_message(&mut stream)).await {
                Ok(Ok(command)) => command,
                Ok(Err(KeyzError::ClientDisconnected)) => {
                    return Err(KeyzError::ClientDisconnected)
                }
                Ok(Err(KeyzError::InvalidCommand(_))) => {
                    send_response(&mut stream, "error:invalid command").await?;
                    continue;
                }
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    let _ = send_response(&mut stream, "error:timeout").await;
                    return Err(KeyzError::ClientTimeout);
                }
            };

        if command.trim().is_empty() {
            send_response(&mut stream, "error:invalid command").await?;
            continue;
        }

        if command == "CLOSE" {
            send_response(&mut stream, "Closing connection").await?;
            stream.shutdown().await.map_err(KeyzError::from)?;
            return Ok(());
        }

        let response = match dispatcher(command, &store).await {
            Ok(response) => response,
            Err(KeyzError::InvalidCommand(_)) => "error:invalid command".into(),
            Err(err) => return Err(err),
        };

        send_response(&mut stream, &response).await?;
    }
}

async fn send_response(stream: &mut TcpStream, message: &str) -> Result<()> {
    helpers::write_message(stream, message).await
}
