mod support;
use keyz::config::SecurityConfig;
use std::{fs, process::Command};
use support::*;

fn disk_settings(root: &std::path::Path, extra: &str) -> String {
    let path = root.join("data").to_string_lossy().replace('\\', "/");
    format!("[store]\nbackend='disk'\ncompression_threshold=1000000\ncleanup_interval_ms=5\n[store.disk]\ndata_dir='{path}'\npartition_count=4\nflush_interval_ms=10\naof_fsync='every_write'\n{extra}")
}

#[test]
fn acknowledged_writes_survive_crash_during_repeated_checkpoints() {
    let root = TempDir::new("crash");
    let settings = disk_settings(&root.0, "");
    let port = free_port();
    for cycle in 0..3 {
        let mut server = Server::start(&root.0, port, &settings, SecurityConfig::default());
        let connection = client(port, SecurityConfig::default());
        let value = "x".repeat(64 * 1024);
        for key in 0..40 {
            assert_eq!(
                connection
                    .send(&format!("SET key{key} {cycle}-{value}"))
                    .unwrap(),
                "ok"
            );
        }
        assert_eq!(connection.send("SET deleted temporary").unwrap(), "ok");
        assert_eq!(connection.send("DEL deleted").unwrap(), "deleted");
        // kill(), not graceful shutdown: no destructors or final flush can help.
        server.crash();
        drop(connection);
        let _recovered = Server::start(&root.0, port, &settings, SecurityConfig::default());
        let connection = client(port, SecurityConfig::default());
        for key in 0..40 {
            assert_eq!(
                connection.send(&format!("GET key{key}")).unwrap(),
                format!("{cycle}-{value}")
            );
        }
        assert_eq!(connection.send("GET deleted").unwrap(), "null");
    }
}

#[test]
fn failed_startup_exits_nonzero_without_touching_existing_data() {
    let root = TempDir::new("startup-error");
    let config = root.0.join("bad.toml");
    fs::write(&config, "[server]\nmax_connections=0").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_keyz"))
        .current_dir(&root.0)
        .env("KEYZ_CONFIG", &config)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("configuration"));
    assert!(!root.0.join("keyz_data").exists());
}

#[cfg(unix)]
#[test]
fn sigterm_flushes_even_when_aof_and_periodic_checkpoint_have_not_run() {
    let root = TempDir::new("sigterm");
    let settings = disk_settings(&root.0, "aof_enabled=false\n")
        .replace("flush_interval_ms=10", "flush_interval_ms=3600000");
    let port = free_port();
    let mut server = Server::start(&root.0, port, &settings, SecurityConfig::default());
    assert_eq!(
        client(port, SecurityConfig::default())
            .send("SET shutdown durable")
            .unwrap(),
        "ok"
    );
    assert!(Command::new("kill")
        .args(["-TERM", &server.child.id().to_string()])
        .status()
        .unwrap()
        .success());
    eventually(|| server.child.try_wait().unwrap().is_some());
    assert!(server.child.wait().unwrap().success(), "{}", server.logs());
    let _recovered = Server::start(&root.0, port, &settings, SecurityConfig::default());
    assert_eq!(
        client(port, SecurityConfig::default())
            .send("GET shutdown")
            .unwrap(),
        "durable"
    );
}

#[test]
fn follower_recovers_after_leader_restart_and_reports_readiness() {
    let root = TempDir::new("reconnect");
    let leader_port = free_port();
    let replication_port = free_port();
    let follower_port = free_port();
    let settings = disk_settings(
        &root.0,
        &format!("[replication]\nrole='leader'\nbind='127.0.0.1:{replication_port}'\n"),
    );
    let mut leader = Server::start(&root.0, leader_port, &settings, SecurityConfig::default());
    let follower_settings =
        format!("[replication]\nrole='follower'\nleader_address='127.0.0.1:{replication_port}'\n");
    let _follower = Server::start(
        &root.0,
        follower_port,
        &follower_settings,
        SecurityConfig::default(),
    );
    let source = client(leader_port, SecurityConfig::default());
    let destination = client(follower_port, SecurityConfig::default());
    for index in 0..50 {
        assert_eq!(
            source
                .send(&format!("SET load{index} value{index}"))
                .unwrap(),
            "ok"
        );
    }
    eventually(|| {
        destination
            .send("GET load49")
            .is_ok_and(|value| value == "value49")
    });
    leader.crash();
    eventually(|| {
        destination.send("HEALTH").is_ok_and(|json| {
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["ready"] == false
        })
    });
    let _leader = Server::start(&root.0, leader_port, &settings, SecurityConfig::default());
    // A failed write may be uncertain, so construct a new client explicitly.
    let source = client(leader_port, SecurityConfig::default());
    assert_eq!(source.send("SET after restart").unwrap(), "ok");
    eventually(|| {
        destination
            .send("GET after")
            .is_ok_and(|value| value == "restart")
    });
    let health: serde_json::Value =
        serde_json::from_str(&destination.send("HEALTH").unwrap()).unwrap();
    assert_eq!(health["ready"], true);
    assert!(health["reconnects"].as_u64().unwrap() >= 1);
}
