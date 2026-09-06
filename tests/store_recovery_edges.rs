mod support;

use keyz::config::SecurityConfig;
use std::{
    fs,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use support::*;

#[test]
fn directory_lock_rejects_another_process_and_releases_after_crash() {
    let root = TempDir::new("exclusive-store");
    let directory = root.0.join("data");
    let path = directory.to_string_lossy().replace('\\', "/");
    let settings = format!(
        "[store]\nbackend='disk'\n[store.disk]\ndata_dir='{path}'\npartition_count=4\nflush_interval_ms=3600000\naof_fsync='every_write'\n"
    );
    let port = free_port();
    let mut owner = Server::start(&root.0, port, &settings, SecurityConfig::default());
    assert_eq!(
        client(port, SecurityConfig::default())
            .send("SET before acknowledged")
            .unwrap(),
        "ok"
    );
    let snapshot = fs::read(directory.join("data.keyz")).unwrap();
    let log = fs::read(directory.join("data.aof")).unwrap();

    // Dot aliases work without symlink privileges on Windows. Unix also checks
    // a real symlink so ownership follows the file object, not a path string.
    #[cfg(not(unix))]
    let alias = directory.join(".");
    #[cfg(unix)]
    let alias = {
        let alias = root.0.join("alias");
        std::os::unix::fs::symlink(&directory, &alias).unwrap();
        alias
    };
    let other_port = free_port();
    let other_config = root.0.join("other.toml");
    let alias_path = alias.to_string_lossy().replace('\\', "/");
    fs::write(
        &other_config,
        format!(
            "[server]\nhost='127.0.0.1'\nport={other_port}\n{}",
            settings.replace(
                &format!("data_dir='{path}'"),
                &format!("data_dir='{alias_path}'")
            )
        ),
    )
    .unwrap();
    let output = fs::File::create(root.0.join("other.log")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_keyz"));
    command
        .current_dir(&root.0)
        .env("KEYZ_CONFIG", &other_config)
        .env_remove("KEYZ_AUTH_TOKEN")
        .env_remove("KEYZ_REPLICATION_TOKEN")
        .env_remove("KEYZ_ENCRYPTION_KEY")
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(output);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut other = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    let status = loop {
        if let Some(status) = other.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = other.kill();
            let _ = other.wait();
            panic!("second process did not reject the locked data directory");
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());
    assert!(fs::read_to_string(root.0.join("other.log"))
        .unwrap()
        .contains("exclusively locked"));
    assert_eq!(fs::read(directory.join("data.keyz")).unwrap(), snapshot);
    assert_eq!(fs::read(directory.join("data.aof")).unwrap(), log);
    assert_eq!(
        client(port, SecurityConfig::default())
            .send("SET after acknowledged")
            .unwrap(),
        "ok"
    );
    owner.crash();
    assert!(directory.join(".keyz.lock").exists());
    let _reopened = Server::start(&root.0, port, &settings, SecurityConfig::default());
    let connection = client(port, SecurityConfig::default());
    for key in ["before", "after"] {
        assert_eq!(
            connection.send(&format!("GET {key}")).unwrap(),
            "acknowledged"
        );
    }
}
