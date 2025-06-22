use std::{
    error::Error,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use tokio::{io::AsyncReadExt, io::AsyncWriteExt, net::TcpListener, net::TcpStream};

pub async fn create_listener(addr: SocketAddr) -> Result<TcpListener, Box<dyn Error>> {
    let listener = TcpListener::bind(addr).await;

    match listener {
        Ok(listener) => Ok(listener),
        Err(e) => Err(e.into()),
    }
}

pub async fn listener_accept_conn(
    listener: &TcpListener,
) -> Result<(TcpStream, SocketAddr), Box<dyn Error>> {
    let accepted = listener.accept().await;

    match accepted {
        Ok((stream, addr)) => Ok((stream, addr)),
        Err(e) => Err(e.into()),
    }
}


pub async fn read_message(stream: &mut TcpStream) -> Result<String, Box<dyn Error>> {
    let mut len_bytes = [0; 4];
    let bytes_read = stream.read(&mut len_bytes).await?;

    if bytes_read < 4 {
        return Err("Failed to read the length of the message".into());
    }
    let len = u32::from_be_bytes(len_bytes);
    let mut buffer = vec![0; len as usize];
    stream.read_exact(&mut buffer).await?;
    let message = String::from_utf8_lossy(&buffer);

    Ok(message.to_string())
}

pub async fn write_message(stream: &mut TcpStream, message: &str) -> Result<(), Box<dyn Error>> {
    let len = message.len() as u32;
    let len_bytes = len.to_be_bytes();
    stream.write_all(&len_bytes).await?;
    stream.write_all(message.as_bytes()).await?;
    Ok(())
}

pub fn socket_address_from_string_ip(ip: String) -> Result<SocketAddr, Box<dyn Error>> {
    const INVALID_IP_ERROR: &str = "Invalid IP address - should be in format: 127.0.0.1:8080";

    let ip = ip.split(":").collect::<Vec<&str>>();
    let port = ip[1].parse::<u16>().expect(INVALID_IP_ERROR);

    let ip_parts = ip[0].split(".").collect::<Vec<&str>>();

    if ip_parts.len() != 4 {
        return Err(INVALID_IP_ERROR.into());
    }

    let mut ip_parts_u8 = Vec::new();
    for part in ip_parts {
        let part_u8 = part.parse::<u8>();
        if part_u8.is_err() {
            return Err(INVALID_IP_ERROR.into());
        }
        ip_parts_u8.push(part_u8.unwrap());
    }

    let ip_addr = IpAddr::V4(Ipv4Addr::new(
        ip_parts_u8[0],
        ip_parts_u8[1],
        ip_parts_u8[2],
        ip_parts_u8[3],
    ));

    let socket_addr = SocketAddr::new(ip_addr, port);

    return Ok(socket_addr);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpStream};
    use tokio::time::{sleep, Duration};

    #[test]
    fn parses_valid_socket_addr() {
        let addr = socket_address_from_string_ip("127.0.0.1:8080".to_string()).unwrap();
        assert_eq!(addr, "127.0.0.1:8080".parse().unwrap());
    }

    #[test]
    fn rejects_invalid_socket_addr() {
        assert!(socket_address_from_string_ip("300.0.0.1:80".to_string()).is_err());
    }

    #[tokio::test]
    async fn create_listener_and_transfer() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = create_listener(addr).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut client = TcpStream::connect(addr).await.unwrap();
        let (mut server_stream, _) = listener_accept_conn(&listener).await.unwrap();

        write_message(&mut client, "hello").await.unwrap();
        let msg = read_message(&mut server_stream).await.unwrap();
        assert_eq!(msg, "hello");

        sleep(Duration::from_millis(10)).await; // ensure cleanup
    }
}
