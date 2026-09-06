mod support;
use keyz::{
    client::KeyzClient,
    config::{ProtocolConfig, SecurityConfig, TlsConfig},
};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

fn read_frame(stream: &mut impl Read) -> String {
    let mut header = [0; 4];
    stream.read_exact(&mut header).unwrap();
    let mut body = vec![0; u32::from_be_bytes(header) as usize];
    stream.read_exact(&mut body).unwrap();
    String::from_utf8(body).unwrap()
}

fn write_frame(stream: &mut impl Write, body: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn client(listener: &TcpListener, security: SecurityConfig, timeout: Duration) -> KeyzClient {
    KeyzClient::new(
        listener.local_addr().unwrap().to_string(),
        Duration::from_secs(2),
        timeout,
        &ProtocolConfig::default(),
    )
    .with_security(security)
}

fn trickle_response(stream: &mut (impl Read + Write), slow_header: bool) {
    assert_eq!(read_frame(stream), "SET key value");
    let mut bytes = Vec::from(8u32.to_be_bytes());
    bytes.extend_from_slice(b"response");
    if !slow_header {
        stream.write_all(&bytes[..4]).unwrap();
        stream.flush().unwrap();
        bytes.drain(..4);
    }
    for byte in bytes {
        if stream
            .write_all(&[byte])
            .and_then(|()| stream.flush())
            .is_err()
        {
            break;
        }
        thread::sleep(Duration::from_millis(120));
    }
}

#[test]
fn slow_header_and_body_cannot_extend_plaintext_or_tls_deadlines() {
    let root = support::TempDir::new("client-deadlines");
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let cert_path = root.0.join("ca.pem");
    fs::write(&cert_path, cert.pem()).unwrap();
    let server_config = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap(),
    );
    for tls in [false, true] {
        for slow_header in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let security = SecurityConfig {
                tls: TlsConfig {
                    enabled: tls,
                    ca_path: Some(cert_path.to_string_lossy().into()),
                    server_name: Some("localhost".into()),
                    ..TlsConfig::default()
                },
                ..SecurityConfig::default()
            };
            let client = client(&listener, security, Duration::from_millis(300));
            let server_config = server_config.clone();
            let worker = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                if tls {
                    let conn = rustls::ServerConnection::new(server_config).unwrap();
                    trickle_response(&mut rustls::StreamOwned::new(conn, socket), slow_header);
                } else {
                    trickle_response(&mut socket, slow_header);
                }
            });
            let start = Instant::now();
            assert!(
                client.send("SET key value").is_err(),
                "accepted a trickled reply: tls={tls}, header={slow_header}"
            );
            assert!(
                start.elapsed() < Duration::from_millis(800),
                "deadline was extended"
            );
            assert!(!client.is_connected());
            worker.join().unwrap();
        }
    }
}

#[test]
fn authentication_and_command_share_one_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = client(
        &listener,
        SecurityConfig {
            auth_token: Some("local-test-token".into()),
            ..SecurityConfig::default()
        },
        Duration::from_millis(300),
    );
    let worker = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(read_frame(&mut stream), "AUTH local-test-token");
        thread::sleep(Duration::from_millis(180));
        write_frame(&mut stream, b"ok").unwrap();
        assert_eq!(read_frame(&mut stream), "SET key value");
        thread::sleep(Duration::from_millis(180));
        let _ = write_frame(&mut stream, b"ok");
    });
    assert!(client.send("SET key value").is_err());
    assert!(!client.is_connected());
    worker.join().unwrap();
}

#[test]
fn discovered_targets_require_exact_explicit_trust_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    for security in [
        SecurityConfig::default(),
        SecurityConfig {
            auth_token: Some("local-test-token".into()),
            tls: TlsConfig {
                enabled: true,
                ..TlsConfig::default()
            },
            ..SecurityConfig::default()
        },
    ] {
        let source = client(&listener, security, Duration::from_secs(1));
        assert!(source.for_discovered_address(&address.to_string()).is_err());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
    let security = SecurityConfig {
        trusted_cluster_addresses: vec![address, "[::1]:7669".parse().unwrap()],
        ..SecurityConfig::default()
    };
    let source = client(&listener, security, Duration::from_secs(1));
    assert!(source.for_discovered_address(&address.to_string()).is_ok());
    assert!(source
        .for_discovered_address("[0:0:0:0:0:0:0:1]:7669")
        .is_ok());
    for untrusted in [
        "localhost:7669",
        "[::1]:7670",
        "127.0.0.1:7669",
        "[::ffff:127.0.0.1]:7669",
        "http://127.0.0.1:7669",
    ] {
        assert!(
            source.for_discovered_address(untrusted).is_err(),
            "accepted {untrusted}"
        );
    }
}

#[test]
fn trusted_discovery_preserves_authenticated_connections() {
    let root = support::TempDir::new("trusted-discovery");
    let port = support::free_port();
    let security = SecurityConfig {
        auth_token: Some("local-test-token".into()),
        trusted_cluster_addresses: vec![format!("127.0.0.1:{port}").parse().unwrap()],
        ..SecurityConfig::default()
    };
    let _server = support::Server::start(
        &root.0,
        port,
        "[security]\nauth_token='local-test-token'",
        security.clone(),
    );
    let source = support::client(port, security);
    let discovered = source
        .for_discovered_address(&format!("127.0.0.1:{port}"))
        .unwrap();
    assert_eq!(discovered.send("SET key value").unwrap(), "ok");
    assert_eq!(discovered.send("GET key").unwrap(), "value");
    assert!(discovered.is_connected());
}

#[test]
fn malformed_close_keeps_connection_usable_and_valid_close_ends_it() {
    let root = support::TempDir::new("strict-close");
    let port = support::free_port();
    let _server = support::Server::start(&root.0, port, "", SecurityConfig::default());
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write_frame(
        &mut stream,
        br#"KEYZ/1 {"id":2,"command":{"op":"close","extra":0}}"#,
    )
    .unwrap();
    let reply = keyz::protocol::decode_response(&read_frame(&mut stream)).unwrap();
    assert!(
        matches!(reply.result, keyz::protocol::ResponseBody::Error { code, .. } if code == "invalid_request")
    );
    write_frame(&mut stream, b"INFO").unwrap();
    assert!(serde_json::from_str::<serde_json::Value>(&read_frame(&mut stream)).is_ok());
    write_frame(&mut stream, br#"KEYZ/1 {"id":3,"command":{"op":"close"}}"#).unwrap();
    assert_eq!(
        keyz::protocol::decode_response(&read_frame(&mut stream))
            .unwrap()
            .result,
        keyz::protocol::ResponseBody::Closing
    );
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
}

#[test]
fn discovery_configuration_accepts_only_numeric_endpoints() {
    for address in [
        "localhost:7667",
        "*.example.com:7667",
        "127.0.0.1",
        "127.0.0.1:65536",
    ] {
        assert!(toml::from_str::<SecurityConfig>(&format!(
            "trusted_cluster_addresses=['{address}']"
        ))
        .is_err());
    }
    let security: SecurityConfig =
        toml::from_str("trusted_cluster_addresses=['127.0.0.1:7667','[::1]:7667']").unwrap();
    assert_eq!(security.trusted_cluster_addresses.len(), 2);
}
