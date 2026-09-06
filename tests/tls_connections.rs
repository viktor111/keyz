mod support;
use keyz::{
    config::{SecurityConfig, TlsConfig},
    protocol::{Command, Request, ResponseBody},
};
use std::fs;
use support::*;

fn identity(root: &std::path::Path, name: &str) -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = rcgen::CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let cert_path = root.join(format!("{name}.pem"));
    let key_path = root.join(format!("{name}-key.pem"));
    fs::write(&cert_path, certificate.pem()).unwrap();
    fs::write(&key_path, key.serialize_pem()).unwrap();
    (
        cert_path.to_string_lossy().replace('\\', "/"),
        key_path.to_string_lossy().replace('\\', "/"),
    )
}

#[test]
fn tls_and_authentication_protect_client_and_replication_connections() {
    let root = TempDir::new("tls");
    let (cert, key) = identity(&root.0, "identity");
    let (untrusted_cert, _) = identity(&root.0, "untrusted");
    let mut security = SecurityConfig {
        auth_token: Some("client-test-token".into()),
        replication_token: Some("replication-test-token".into()),
        tls: TlsConfig {
            enabled: true,
            cert_path: Some(cert.clone()),
            key_path: Some(key.clone()),
            ca_path: Some(cert.clone()),
            server_name: Some("localhost".into()),
        },
        ..SecurityConfig::default()
    };
    let security_settings = format!("[security]\nauth_token='client-test-token'\nreplication_token='replication-test-token'\n[security.tls]\nenabled=true\ncert_path='{cert}'\nkey_path='{key}'\nca_path='{cert}'\nserver_name='localhost'\n");
    let leader_port = free_port();
    let replication_port = free_port();
    let follower_port = free_port();
    security.trusted_cluster_addresses =
        vec![format!("127.0.0.1:{follower_port}").parse().unwrap()];
    let _leader = Server::start(
        &root.0,
        leader_port,
        &format!(
            "{security_settings}[replication]\nrole='leader'\nbind='127.0.0.1:{replication_port}'"
        ),
        security.clone(),
    );
    let _follower = Server::start(&root.0, follower_port, &format!("{security_settings}[replication]\nrole='follower'\nleader_address='localhost:{replication_port}'"), security.clone());
    let source = client(leader_port, security.clone());
    let request = Request {
        id: 17,
        command: Command::Set {
            key: "binary".into(),
            value: vec![0, 255, 32],
            ttl_secs: Some(0),
        },
    };
    assert_eq!(
        source.send_request(&request).unwrap().result,
        ResponseBody::Ok
    );
    let destination = client(follower_port, security.clone())
        .for_discovered_address(&format!("127.0.0.1:{follower_port}"))
        .unwrap();
    let read = Request {
        id: 18,
        command: Command::Get {
            key: "binary".into(),
        },
    };
    eventually(|| {
        destination.send_request(&read).is_ok_and(|response| {
            response.result
                == ResponseBody::Value {
                    value: vec![0, 255, 32],
                }
        })
    });

    let mut invalid = security.clone();
    invalid.auth_token = Some("wrong-token".into());
    assert!(client(leader_port, invalid).send("INFO").is_err());
    let mut invalid = security.clone();
    invalid.tls.ca_path = Some(untrusted_cert);
    assert!(client(leader_port, invalid).send("INFO").is_err());
    let mut invalid = security.clone();
    invalid.tls.server_name = Some("wrong.example".into());
    assert!(client(leader_port, invalid).send("INFO").is_err());

    // A replication token is distinct from the client token: command credentials
    // cannot subscribe to the replication stream.
    assert!(client(replication_port, security.clone())
        .send("INFO")
        .is_err());
    let mut missing = security;
    missing.auth_token = None;
    let denied = client(leader_port, missing)
        .send("SET unauthorized value")
        .unwrap();
    assert_eq!(denied, "error:unauthorized");
    assert_eq!(source.send("GET unauthorized").unwrap(), "null");
}
