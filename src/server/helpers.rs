use std::net::SocketAddr;

use tokio::{
    io::AsyncReadExt,
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};

use crate::server::error::{KeyzError, Result};

pub async fn create_listener(addr: SocketAddr) -> Result<TcpListener> {
    TcpListener::bind(addr).await.map_err(KeyzError::from)
}

pub async fn listener_accept_conn(
    listener: &TcpListener,
) -> Result<(TcpStream, SocketAddr)> {
    listener.accept().await.map_err(KeyzError::from)
}

pub async fn read_message(stream: &mut TcpStream, max_len: u32) -> Result<String> {
    let mut len_bytes = [0; 4];
    stream
        .read_exact(&mut len_bytes)
        .await
        .map_err(map_io_error)?;

    let len = u32::from_be_bytes(len_bytes);
    if len == 0 || len > max_len {
        return Err(KeyzError::InvalidCommand("message length out of bounds".into()));
    }

    let mut buffer = vec![0; len as usize];
    stream
        .read_exact(&mut buffer)
        .await
        .map_err(map_io_error)?;

    let message = String::from_utf8(buffer)?;
    Ok(message)
}

pub async fn write_message(stream: &mut TcpStream, message: &str) -> Result<()> {
    let len = message.len() as u32;
    let len_bytes = len.to_be_bytes();

    stream
        .write_all(&len_bytes)
        .await
        .map_err(map_io_error)?;
    stream
        .write_all(message.as_bytes())
        .await
        .map_err(map_io_error)?;
    Ok(())
}

pub fn socket_address_from_string_ip(ip: String) -> Result<SocketAddr> {
    ip.parse()
        .map_err(|_| KeyzError::InvalidSocketAddress)
}

fn map_io_error(err: std::io::Error) -> KeyzError {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::UnexpectedEof
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionAborted
        | ErrorKind::BrokenPipe => KeyzError::ClientDisconnected,
        _ => KeyzError::Io(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;
    use tokio::time::{sleep, Duration};

    #[test]
    fn parses_valid_socket_addr() {
        match socket_address_from_string_ip("127.0.0.1:8080".to_string()) {
            Ok(addr) => assert_eq!(addr, SocketAddr::from(([127, 0, 0, 1], 8080))),
            Err(err) => panic!("expected valid socket address, got {err:?}"),
        }
    }

    #[test]
    fn rejects_invalid_socket_addr() {
        assert!(socket_address_from_string_ip("300.0.0.1:80".to_string()).is_err());
    }

    #[tokio::test]
    async fn create_listener_and_transfer() -> Result<()> {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = create_listener(addr).await?;
        let addr = listener.local_addr()?;

        let mut client = TcpStream::connect(addr).await?;
        let (mut server_stream, _) = listener_accept_conn(&listener).await?;

        write_message(&mut client, "hello").await?;
        let msg = read_message(&mut server_stream, 4 * 1024 * 1024).await?;
        assert_eq!(msg, "hello");

        sleep(Duration::from_millis(10)).await; // ensure cleanup
        Ok(())
    }
}
