#![allow(dead_code)]
use keyz::{
    client::KeyzClient,
    config::{ProtocolConfig, SecurityConfig},
};
use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct TempDir(pub PathBuf);
impl TempDir {
    pub fn new(label: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "keyz-{label}-{}-{suffix}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn client(port: u16, security: SecurityConfig) -> KeyzClient {
    KeyzClient::new(
        format!("127.0.0.1:{port}"),
        Duration::from_secs(2),
        Duration::from_secs(5),
        &ProtocolConfig::default(),
    )
    .with_security(security)
}

pub struct Server {
    pub child: Child,
    pub port: u16,
    log: PathBuf,
}
impl Server {
    pub fn start(root: &Path, port: u16, settings: &str, security: SecurityConfig) -> Self {
        let config = root.join(format!("server-{port}.toml"));
        fs::write(
            &config,
            format!("[server]\nhost='127.0.0.1'\nport={port}\n{settings}"),
        )
        .unwrap();
        let log = root.join(format!("server-{port}.log"));
        let output = fs::File::create(&log).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_keyz"));
        command
            .current_dir(root)
            .env("KEYZ_CONFIG", config)
            .env_remove("KEYZ_AUTH_TOKEN")
            .env_remove("KEYZ_REPLICATION_TOKEN")
            .env_remove("KEYZ_ENCRYPTION_KEY")
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .stdin(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let child = command.spawn().unwrap();
        let mut server = Self { child, port, log };
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(status) = server.child.try_wait().unwrap() {
                panic!("server exited {status}: {}", server.logs());
            }
            if client(port, security.clone()).send("INFO").is_ok() {
                return server;
            }
            assert!(
                Instant::now() < deadline,
                "server failed to start: {}",
                server.logs()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    pub fn crash(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
    pub fn logs(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn eventually(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while !check() {
        assert!(Instant::now() < deadline, "condition did not converge");
        thread::sleep(Duration::from_millis(10));
    }
}
