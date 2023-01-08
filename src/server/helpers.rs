use std::{
    error::Error,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

use tokio::{io::AsyncReadExt, io::AsyncWriteExt, net::TcpListener, net::TcpStream};

/// Create a listener on a port
///
/// # Arguments
///
/// * `port` - The port to bind to
///
/// # Returns
///
/// * Result<TcpListener, Box<dyn Error>> - The listener

pub async fn create_listener(addr: SocketAddr) -> Result<TcpListener, Box<dyn Error>> {
    let listener = TcpListener::bind(addr).await;

    match listener {
        Ok(listener) => Ok(listener),
        Err(e) => Err(e.into()),
    }
}

/// Accept a connection from a client
///
/// # Arguments
///
/// * `listener` - The listener to accept a connection from
///
/// # Returns
///
/// * Result<TcpStream, Box<dyn Error>> - The stream to the client
///
/// # Errors
///
/// * If the connection cannot be accepted

pub async fn listener_accept_conn(
    listener: &TcpListener,
) -> Result<(TcpStream, SocketAddr), Box<dyn Error>> {
    let accepted = listener.accept().await;

    match accepted {
        Ok((stream, addr)) => Ok((stream, addr)),
        Err(e) => Err(e.into()),
    }
}

/// Read a message from a TCP stream and return it as a String
///
/// # Arguments
///
/// * `stream` - The stream to read from
///
/// # Returns
///
/// * Result<String, Box<dyn Error>> - The message
///
/// # Errors
///
/// * If the message cannot be read

pub async fn read_message(stream: &mut TcpStream) -> Result<String, Box<dyn Error>> {
    let mut buffer = [0; 1024];
    let bytes_read = stream.read(&mut buffer).await;

    match bytes_read {
        Ok(bytes_read) => {
            println!("bytes_read: {}", bytes_read);
            let message = String::from_utf8_lossy(&buffer[..bytes_read])
                .trim()
                .to_string();
            Ok(message)
        }
        Err(e) => Err(e.into()),
    }
}

/// Write a message to a TCP stream
///
/// # Arguments
///
/// * `stream` - The stream to write to
/// * `message` - The message to write
///
/// # Returns
///
/// * Result<(), Box<dyn Error>> - Empty result
///
/// # Errors
///
/// * If the message cannot be written

pub async fn write_message(stream: &mut TcpStream, message: String) -> Result<(), Box<dyn Error>> {
    let written = stream.write(message.as_bytes()).await;

    match written {
        Ok(_) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Create a SocketAddr from IP and port as string in from of "0.0.0.0:1111"
///
/// # Arguments
///
/// * `ip` - The IP address as string
///
/// # Returns
///
/// * Result<SocketAddr, Box<dyn Error>> - The SocketAddr
///
/// # Errors
///
/// * If the IP address argument is invalid

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
