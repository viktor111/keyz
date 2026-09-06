//! Shared TLS transport for the server, replication, and synchronous clients.
//! TLS uses explicit CA trust and always verifies the peer name. There is no
//! insecure verification or automatic plaintext fallback.
use std::{
    fs::File,
    io::{self, BufReader, Read, Write},
    net::{TcpStream as StdTcpStream, ToSocketAddrs},
    sync::Arc,
    time::{Duration, Instant},
};

use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, ServerName},
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, StreamOwned,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::{
    config::{SecurityConfig, TlsConfig},
    server::error::{KeyzError, Result},
};

pub trait AsyncIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncIo for T {}
pub type AsyncStream = Box<dyn AsyncIo>;

#[derive(Clone)]
pub struct Transport {
    acceptor: Option<TlsAcceptor>,
    connector: Option<TlsConnector>,
    tls: TlsConfig,
}

impl Transport {
    /// Load server certificate/key once at startup. Clients only need CA trust.
    pub fn new(security: &SecurityConfig) -> Result<Self> {
        let tls = &security.tls;
        if !tls.enabled {
            return Ok(Self {
                acceptor: None,
                connector: None,
                tls: tls.clone(),
            });
        }
        let cert_path = required(&tls.cert_path, "security.tls.cert_path")?;
        let key_path = required(&tls.key_path, "security.tls.key_path")?;
        let certificates = certificates(cert_path)?;
        let key = private_key(key_path)?;
        let server = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .map_err(|e| KeyzError::Tls(e.to_string()))?;
        let connector = if tls.ca_path.is_some() {
            Some(TlsConnector::from(client_config(tls)?))
        } else {
            None
        };
        Ok(Self {
            acceptor: Some(TlsAcceptor::from(Arc::new(server))),
            connector,
            tls: tls.clone(),
        })
    }

    /// Callers bound the entire handshake with their configured timeout.
    pub async fn accept(&self, stream: TcpStream) -> Result<AsyncStream> {
        stream.set_nodelay(true)?;
        match &self.acceptor {
            Some(acceptor) => Ok(Box::new(acceptor.accept(stream).await?)),
            None => Ok(Box::new(stream)),
        }
    }

    pub async fn connect(&self, address: &str) -> Result<AsyncStream> {
        if self.tls.enabled {
            let connector = self.connector.as_ref().ok_or_else(|| {
                KeyzError::InvalidConfig(
                    "security.tls.ca_path is required for outgoing TLS connections".into(),
                )
            })?;
            let name = server_name(address, &self.tls)?;
            let stream = TcpStream::connect(address).await?;
            stream.set_nodelay(true)?;
            Ok(Box::new(connector.connect(name, stream).await?))
        } else {
            let stream = TcpStream::connect(address).await?;
            stream.set_nodelay(true)?;
            Ok(Box::new(stream))
        }
    }
}

fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str> {
    value
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| KeyzError::InvalidConfig(format!("{name} is required when TLS is enabled")))
}

fn certificates(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(File::open(path)?);
    let certificates = rustls_pemfile::certs(&mut reader).collect::<io::Result<Vec<_>>>()?;
    if certificates.is_empty() {
        return Err(KeyzError::Tls("PEM file contains no certificates".into()));
    }
    Ok(certificates)
}

fn private_key(path: &str) -> Result<PrivateKeyDer<'static>> {
    rustls_pemfile::private_key(&mut BufReader::new(File::open(path)?))?
        .ok_or_else(|| KeyzError::Tls("PEM file contains no private key".into()))
}

fn client_config(tls: &TlsConfig) -> Result<Arc<ClientConfig>> {
    let ca = required(&tls.ca_path, "security.tls.ca_path")?;
    let mut roots = RootCertStore::empty();
    for certificate in certificates(ca)? {
        roots
            .add(certificate)
            .map_err(|e| KeyzError::Tls(e.to_string()))?;
    }
    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

fn server_name(address: &str, tls: &TlsConfig) -> Result<ServerName<'static>> {
    let name = match &tls.server_name {
        Some(name) => name.clone(),
        None => address
            .rsplit_once(':')
            .map(|(host, _)| host.trim_matches(['[', ']']).to_owned())
            .ok_or(KeyzError::InvalidSocketAddress)?,
    };
    ServerName::try_from(name).map_err(|_| KeyzError::Tls("invalid TLS server name".into()))
}

/// Compare tokens without content-dependent early exits for equal lengths.
pub fn authenticate(expected: &str, candidate: &str) -> bool {
    bool::from(expected.as_bytes().ct_eq(candidate.as_bytes()))
}

/// Enforce an absolute deadline on every socket operation, including operations
/// performed internally by rustls while assembling a TLS record.
pub struct DeadlineSocket {
    socket: StdTcpStream,
    deadline: Option<Instant>,
}

impl DeadlineSocket {
    fn remaining(&self) -> io::Result<Option<Duration>> {
        self.deadline
            .map(|deadline| {
                deadline
                    .checked_duration_since(Instant::now())
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::TimedOut, "request deadline expired")
                    })
            })
            .transpose()
    }
}

impl Read for DeadlineSocket {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(remaining) = self.remaining()? {
            self.socket.set_read_timeout(Some(remaining))?;
        }
        self.socket.read(buffer)
    }
}

impl Write for DeadlineSocket {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if let Some(remaining) = self.remaining()? {
            self.socket.set_write_timeout(Some(remaining))?;
        }
        self.socket.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.remaining()?;
        self.socket.flush()?;
        self.remaining()?;
        Ok(())
    }
}

pub enum SyncStream {
    Plain(DeadlineSocket),
    Tls(Box<StreamOwned<ClientConnection, DeadlineSocket>>),
}

impl SyncStream {
    pub fn connect(address: &str, timeout: Duration, tls: &TlsConfig) -> Result<Self> {
        if timeout.is_zero() {
            return Err(KeyzError::InvalidConfig(
                "connect timeout must be positive".into(),
            ));
        }
        // Validate trust before opening a socket.
        let tls_client = if tls.enabled {
            Some((client_config(tls)?, server_name(address, tls)?))
        } else {
            None
        };
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| KeyzError::InvalidConfig("connect timeout is too large".into()))?;
        let mut connected = None;
        let mut last_error =
            io::Error::new(io::ErrorKind::AddrNotAvailable, "no resolved addresses");
        for address in address.to_socket_addrs()? {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(KeyzError::ClientTimeout);
            }
            match StdTcpStream::connect_timeout(&address, remaining) {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(error) => last_error = error,
            }
        }
        let mut socket = DeadlineSocket {
            socket: connected.ok_or(last_error)?,
            deadline: Some(deadline),
        };
        socket.socket.set_nodelay(true)?;
        if let Some((config, name)) = tls_client {
            let mut connection =
                ClientConnection::new(config, name).map_err(|e| KeyzError::Tls(e.to_string()))?;
            while connection.is_handshaking() {
                connection.complete_io(&mut socket)?;
            }
            socket.remaining()?;
            socket.deadline = None;
            Ok(Self::Tls(Box::new(StreamOwned::new(connection, socket))))
        } else {
            socket.remaining()?;
            socket.deadline = None;
            Ok(Self::Plain(socket))
        }
    }

    pub fn socket(&self) -> &StdTcpStream {
        match self {
            Self::Plain(socket) => &socket.socket,
            Self::Tls(stream) => &stream.sock.socket,
        }
    }
    pub fn set_deadline(&mut self, deadline: Instant) {
        match self {
            Self::Plain(socket) => socket.deadline = Some(deadline),
            Self::Tls(stream) => stream.sock.deadline = Some(deadline),
        }
    }
    /// Enforce the deadline even when TLS can satisfy an operation from its
    /// plaintext buffers without making another underlying socket call.
    pub fn check_deadline(&self) -> io::Result<()> {
        match self {
            Self::Plain(socket) => socket.remaining().map(|_| ()),
            Self::Tls(stream) => stream.sock.remaining().map(|_| ()),
        }
    }
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket().set_read_timeout(timeout)
    }
    pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket().set_write_timeout(timeout)
    }
    pub fn set_nodelay(&self, value: bool) -> io::Result<()> {
        self.socket().set_nodelay(value)
    }
}

impl Read for SyncStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.check_deadline()?;
        match self {
            Self::Plain(stream) => stream.read(buffer),
            Self::Tls(stream) => stream.read(buffer),
        }
    }
}
impl Write for SyncStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.check_deadline()?;
        match self {
            Self::Plain(stream) => stream.write(buffer),
            Self::Tls(stream) => stream.write(buffer),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.check_deadline()?;
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }?;
        self.check_deadline()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffered_tls_stream() -> (SyncStream, StdTcpStream) {
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(certificate.der().clone()).unwrap();
        let client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        let mut client = ClientConnection::new(
            Arc::new(client_config),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let mut server = rustls::ServerConnection::new(Arc::new(server_config)).unwrap();
        // Exchange TLS handshake bytes in memory so the regression does not
        // depend on packet timing, scheduler delays, or sleep durations.
        for _ in 0..10 {
            let mut bytes = Vec::new();
            client.write_tls(&mut bytes).unwrap();
            if !bytes.is_empty() {
                server.read_tls(&mut io::Cursor::new(bytes)).unwrap();
                server.process_new_packets().unwrap();
            }
            let mut bytes = Vec::new();
            server.write_tls(&mut bytes).unwrap();
            if !bytes.is_empty() {
                client.read_tls(&mut io::Cursor::new(bytes)).unwrap();
                client.process_new_packets().unwrap();
            }
            if !client.is_handshaking() && !server.is_handshaking() && !client.wants_write() {
                break;
            }
        }
        assert!(!client.is_handshaking());
        assert!(!server.is_handshaking());
        server.writer().write_all(b"buffered plaintext").unwrap();
        let mut bytes = Vec::new();
        server.write_tls(&mut bytes).unwrap();
        client.read_tls(&mut io::Cursor::new(bytes)).unwrap();
        assert!(
            client
                .process_new_packets()
                .unwrap()
                .plaintext_bytes_to_read()
                > 0
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = StdTcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (peer, _) = listener.accept().unwrap();
        (
            SyncStream::Tls(Box::new(StreamOwned::new(
                client,
                DeadlineSocket {
                    socket,
                    deadline: None,
                },
            ))),
            peer,
        )
    }

    #[test]
    fn expired_deadline_rejects_already_buffered_tls_plaintext() {
        let (mut stream, _peer) = buffered_tls_stream();
        let mut first = [0; 1];
        stream.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"b");
        stream.set_deadline(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            stream.read(&mut [0; 16]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        // The rejected read must not consume the buffered application bytes.
        stream.set_deadline(Instant::now() + Duration::from_secs(2));
        let mut rest = [0; 17];
        stream.read_exact(&mut rest).unwrap();
        assert_eq!(&rest, b"uffered plaintext");
    }

    #[test]
    fn expired_deadline_rejects_buffered_tls_writes_and_noop_flushes() {
        let (mut stream, _peer) = buffered_tls_stream();
        stream.check_deadline().unwrap();
        stream.set_deadline(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            stream.check_deadline().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(stream.flush().unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            stream.write(b"not accepted").unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn rejects_tls_without_server_identity_or_client_trust() {
        let mut security = SecurityConfig::default();
        security.tls.enabled = true;
        assert!(Transport::new(&security).is_err());
        assert!(
            SyncStream::connect("127.0.0.1:1", Duration::from_millis(5), &security.tls).is_err()
        );
    }

    #[test]
    fn token_comparison_does_not_accept_prefixes_or_suffixes() {
        assert!(authenticate("correct token", "correct token"));
        assert!(!authenticate("correct token", "correct"));
        assert!(!authenticate("correct token", "wrongxx token"));
        assert!(!authenticate("correct token", "correct token "));
    }

    #[test]
    fn certificate_names_support_dns_and_ipv6() {
        let tls = TlsConfig::default();
        assert!(server_name("localhost:7667", &tls).is_ok());
        assert!(server_name("[::1]:7667", &tls).is_ok());
        assert!(server_name("not a host:7667", &tls).is_err());
    }
}
