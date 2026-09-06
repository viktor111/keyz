use std::{
    env, fs,
    net::{SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

use crate::server::error::{KeyzError, Result};

const DEFAULT_CONFIG_PATH: &str = "keyz.toml";
const ENV_CONFIG_PATH: &str = "KEYZ_CONFIG";
const ENV_ENCRYPTION_KEY: &str = "KEYZ_ENCRYPTION_KEY";

pub fn parse_hex_key(hex: &str) -> Result<[u8; 32]> {
    if hex.len() != 64 || !hex.is_ascii() {
        return Err(KeyzError::InvalidConfig(format!(
            "encryption key must be 64 hex characters, got {}",
            hex.len()
        )));
    }
    let mut key = [0u8; 32];
    for i in 0..32 {
        key[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| {
            KeyzError::InvalidConfig("encryption key contains invalid hex characters".into())
        })?;
    }
    Ok(key)
}

#[derive(Debug, Clone)]
pub enum ConfigSource {
    ExplicitPath(PathBuf),
    Env(PathBuf),
    DefaultFile(PathBuf),
    Defaults,
}

#[derive(Debug, Clone)]
pub struct ConfigContext {
    pub config: Config,
    pub source: ConfigSource,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub protocol: ProtocolConfig,
    #[serde(default)]
    pub replication: ReplicationConfig,
    #[serde(default)]
    pub security: SecurityConfig,
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: Option<P>) -> Result<Self> {
        let ConfigContext { config, source } = Self::load_with_source(path)?;
        match &source {
            ConfigSource::ExplicitPath(path)
            | ConfigSource::Env(path)
            | ConfigSource::DefaultFile(path) => {
                println!(
                    "[config] Loaded configuration from {}",
                    path.to_string_lossy()
                );
            }
            ConfigSource::Defaults => {
                println!("[config] No configuration file found; using defaults");
            }
        }
        Ok(config)
    }

    pub fn load_with_source<P: AsRef<Path>>(path: Option<P>) -> Result<ConfigContext> {
        let explicit_path = path.map(|p| p.as_ref().to_path_buf());
        let env_path = env::var(ENV_CONFIG_PATH).ok().map(PathBuf::from);

        if let Some(path) = explicit_path {
            let config = Self::load_from_path(&path)?;
            return Ok(ConfigContext {
                config,
                source: ConfigSource::ExplicitPath(path),
            });
        }

        if let Some(path) = env_path {
            let config = Self::load_from_path(&path)?;
            return Ok(ConfigContext {
                config,
                source: ConfigSource::Env(path),
            });
        }

        match fs::read_to_string(DEFAULT_CONFIG_PATH) {
            Ok(content) => {
                let config = Self::from_toml_str(&content)?;
                Ok(ConfigContext {
                    config,
                    source: ConfigSource::DefaultFile(PathBuf::from(DEFAULT_CONFIG_PATH)),
                })
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ConfigContext {
                config: Self::default(),
                source: ConfigSource::Defaults,
            }),
            Err(source) => Err(KeyzError::ConfigIo {
                path: DEFAULT_CONFIG_PATH.to_string(),
                source,
            }),
        }
    }

    fn load_from_path(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path).map_err(|source| KeyzError::ConfigIo {
            path: path.to_string_lossy().to_string(),
            source,
        })?;
        Self::from_toml_str(&content)
    }

    pub fn from_toml_str(input: &str) -> Result<Self> {
        if input.trim().is_empty() {
            let config = Self::default();
            return Ok(config);
        }

        let mut config: Config =
            toml::from_str(input).map_err(|err| KeyzError::ConfigParse(err.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&mut self) -> Result<()> {
        self.server.validate()?;
        self.store.validate()?;
        self.protocol.validate()?;
        self.replication.validate()?;
        if self.replication.role == ReplicationRole::Leader
            && u64::from(self.protocol.max_message_bytes)
                + crate::server::replication::REPLICATION_SET_OVERHEAD as u64
                > crate::server::replication::MAX_REPLICATION_FRAME as u64
        {
            return Err(KeyzError::InvalidConfig(
                "leader protocol.max_message_bytes must leave 22 bytes within the 64 MiB replication record limit".into(),
            ));
        }
        self.security.validate()?;
        if self.security.tls.enabled
            && self.replication.role == ReplicationRole::Follower
            && self
                .security
                .tls
                .ca_path
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(KeyzError::InvalidConfig(
                "TLS followers require security.tls.ca_path".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "ServerConfig::default_host")]
    pub host: String,
    #[serde(default = "ServerConfig::default_port")]
    pub port: u16,
    #[serde(default = "ServerConfig::default_capacity")]
    pub max_connections: usize,
    #[serde(default = "ServerConfig::default_capacity")]
    pub work_queue_capacity: usize,
    #[serde(default = "ServerConfig::default_timeout")]
    pub write_timeout_secs: u64,
    #[serde(default = "ServerConfig::default_timeout")]
    pub shutdown_timeout_secs: u64,
}

impl ServerConfig {
    const fn default_capacity() -> usize {
        1024
    }
    const fn default_timeout() -> u64 {
        30
    }
    fn default_host() -> String {
        "127.0.0.1".into()
    }

    const fn default_port() -> u16 {
        7667
    }

    pub fn socket_addr(&self) -> Result<SocketAddr> {
        let host = if self.host.trim().is_empty() {
            "127.0.0.1"
        } else {
            self.host.trim()
        };
        (host.trim_matches(['[', ']']), self.port)
            .to_socket_addrs()
            .map_err(|_| KeyzError::InvalidSocketAddress)?
            .next()
            .ok_or(KeyzError::InvalidSocketAddress)
    }

    fn validate(&mut self) -> Result<()> {
        if self.max_connections == 0
            || self.work_queue_capacity == 0
            || self.max_connections > tokio::sync::Semaphore::MAX_PERMITS
            || self.work_queue_capacity > tokio::sync::Semaphore::MAX_PERMITS
            || self.write_timeout_secs == 0
            || self.shutdown_timeout_secs == 0
        {
            return Err(KeyzError::InvalidConfig(
                "server connection/queue limits must fit a semaphore and all limits/timeouts must be positive".into(),
            ));
        }
        for timeout in [self.write_timeout_secs, self.shutdown_timeout_secs] {
            if std::time::Instant::now()
                .checked_add(Duration::from_secs(timeout))
                .is_none()
            {
                return Err(KeyzError::InvalidConfig(
                    "server timeout is too large".into(),
                ));
            }
        }
        if self.port == 0 {
            return Err(KeyzError::InvalidConfig(
                "server.port must be greater than zero".into(),
            ));
        }

        if self.host.trim().is_empty() {
            self.host = "127.0.0.1".into();
        }

        Ok(())
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 7667,
            max_connections: Self::default_capacity(),
            work_queue_capacity: Self::default_capacity(),
            write_timeout_secs: Self::default_timeout(),
            shutdown_timeout_secs: Self::default_timeout(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BackendType {
    #[default]
    Memory,
    Disk,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AofFsyncPolicy {
    EveryWrite,
    #[default]
    EverySecond,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiskConfig {
    #[serde(default = "DiskConfig::default_data_dir")]
    pub data_dir: String,
    #[serde(default = "DiskConfig::default_partition_count")]
    pub partition_count: u32,
    #[serde(default)]
    pub encryption_enabled: bool,
    pub encryption_key: Option<String>,
    #[serde(default = "DiskConfig::default_cache_max_entries")]
    pub cache_max_entries: usize,
    #[serde(default = "DiskConfig::default_flush_interval_ms")]
    pub flush_interval_ms: u64,
    #[serde(default = "DiskConfig::default_aof_enabled")]
    pub aof_enabled: bool,
    #[serde(default)]
    pub aof_fsync: AofFsyncPolicy,
}

impl DiskConfig {
    fn default_data_dir() -> String {
        "./keyz_data".into()
    }

    const fn default_partition_count() -> u32 {
        16
    }

    const fn default_cache_max_entries() -> usize {
        10_000
    }

    const fn default_flush_interval_ms() -> u64 {
        5000
    }

    const fn default_aof_enabled() -> bool {
        true
    }

    pub fn resolve_encryption_key(&self) -> Result<Option<[u8; 32]>> {
        if !self.encryption_enabled {
            return Ok(None);
        }

        // Env var takes precedence over config file
        let hex = if let Ok(env_key) = env::var(ENV_ENCRYPTION_KEY) {
            env_key
        } else if let Some(ref config_key) = self.encryption_key {
            config_key.clone()
        } else {
            return Err(KeyzError::InvalidConfig(
                "store.disk.encryption_enabled is true but no encryption key provided \
                 (set KEYZ_ENCRYPTION_KEY env var or store.disk.encryption_key in config)"
                    .into(),
            ));
        };

        parse_hex_key(&hex).map(Some)
    }

    fn validate(&self) -> Result<()> {
        if self.data_dir.trim().is_empty() {
            return Err(KeyzError::InvalidConfig(
                "store.disk.data_dir cannot be empty".into(),
            ));
        }

        if self.partition_count == 0 || self.partition_count > 4096 {
            return Err(KeyzError::InvalidConfig(
                "store.disk.partition_count must be between 1 and 4096".into(),
            ));
        }

        if self.flush_interval_ms == 0 {
            return Err(KeyzError::InvalidConfig(
                "store.disk.flush_interval_ms must be greater than zero".into(),
            ));
        }
        if std::time::Instant::now()
            .checked_add(Duration::from_millis(self.flush_interval_ms))
            .is_none()
        {
            return Err(KeyzError::InvalidConfig(
                "store.disk.flush_interval_ms is too large".into(),
            ));
        }

        if let Some(ref key) = self.encryption_key {
            parse_hex_key(key)?;
        }

        Ok(())
    }
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            data_dir: Self::default_data_dir(),
            partition_count: Self::default_partition_count(),
            encryption_enabled: false,
            encryption_key: None,
            cache_max_entries: Self::default_cache_max_entries(),
            flush_interval_ms: Self::default_flush_interval_ms(),
            aof_enabled: Self::default_aof_enabled(),
            aof_fsync: AofFsyncPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct StoreConfig {
    /// Key UTF-8 bytes plus stored payload bytes; zero means unlimited.
    #[serde(default)]
    pub max_bytes: u64,
    #[serde(default = "StoreConfig::default_compression_threshold")]
    pub compression_threshold: usize,
    #[serde(default = "StoreConfig::default_cleanup_interval_ms")]
    pub cleanup_interval_ms: u64,
    pub default_ttl_secs: Option<u64>,
    #[serde(default)]
    pub backend: BackendType,
    #[serde(default)]
    pub disk: DiskConfig,
}

impl StoreConfig {
    const fn default_compression_threshold() -> usize {
        512
    }

    const fn default_cleanup_interval_ms() -> u64 {
        250
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.compression_threshold == 0 {
            return Err(KeyzError::InvalidConfig(
                "store.compression_threshold must be greater than zero".into(),
            ));
        }

        if self.cleanup_interval_ms == 0 {
            return Err(KeyzError::InvalidConfig(
                "store.cleanup_interval_ms must be greater than zero".into(),
            ));
        }
        if std::time::Instant::now()
            .checked_add(Duration::from_millis(self.cleanup_interval_ms))
            .is_none()
        {
            return Err(KeyzError::InvalidConfig(
                "store.cleanup_interval_ms is too large".into(),
            ));
        }

        if let Some(ttl) = self.default_ttl_secs {
            if ttl == 0 {
                return Err(KeyzError::InvalidConfig(
                    "store.default_ttl_secs cannot be zero (use None instead)".into(),
                ));
            }
        }

        if self.backend == BackendType::Disk {
            self.disk.validate()?;
        }

        Ok(())
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            max_bytes: 0,
            compression_threshold: Self::default_compression_threshold(),
            cleanup_interval_ms: Self::default_cleanup_interval_ms(),
            default_ttl_secs: None,
            backend: BackendType::default(),
            disk: DiskConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProtocolConfig {
    #[serde(default = "ProtocolConfig::default_max_message_bytes")]
    pub max_message_bytes: u32,
    #[serde(default = "ProtocolConfig::default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "ProtocolConfig::default_close_command")]
    pub close_command: String,
    #[serde(default = "ProtocolConfig::default_timeout_response")]
    pub timeout_response: String,
    #[serde(default = "ProtocolConfig::default_invalid_command_response")]
    pub invalid_command_response: String,
}

/// Authentication applies to all client commands, including diagnostics.
/// Environment variables override file values. Replication defaults to the
/// client token unless a distinct replication token is supplied.
#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    pub auth_token: Option<String>,
    pub replication_token: Option<String>,
    /// Explicit IP:port destinations the CLI may probe from cluster discovery.
    pub trusted_cluster_addresses: Vec<std::net::SocketAddr>,
    pub tls: TlsConfig,
}

impl std::fmt::Debug for SecurityConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecurityConfig")
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "replication_token",
                &self.replication_token.as_ref().map(|_| "[redacted]"),
            )
            .field("trusted_cluster_addresses", &self.trusted_cluster_addresses)
            .field("tls", &self.tls)
            .finish()
    }
}

impl SecurityConfig {
    pub fn resolve_auth_token(&self) -> Option<String> {
        env::var("KEYZ_AUTH_TOKEN")
            .ok()
            .or_else(|| self.auth_token.clone())
    }

    pub fn resolve_replication_token(&self) -> Option<String> {
        env::var("KEYZ_REPLICATION_TOKEN")
            .ok()
            .or_else(|| self.replication_token.clone())
            .or_else(|| self.resolve_auth_token())
    }

    fn validate(&self) -> Result<()> {
        for (name, token) in [
            ("client", self.resolve_auth_token()),
            ("replication", self.resolve_replication_token()),
        ] {
            if token
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > 4096)
            {
                return Err(KeyzError::InvalidConfig(format!(
                    "{name} authentication token must contain 1 to 4096 bytes"
                )));
            }
        }
        if self.tls.enabled && (self.tls.cert_path.is_some() != self.tls.key_path.is_some()) {
            return Err(KeyzError::InvalidConfig(
                "TLS certificate and private key must be provided together".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    pub enabled: bool,
    /// PEM certificate chain, required by the server when TLS is enabled.
    pub cert_path: Option<String>,
    /// PEM private key (PKCS#8, PKCS#1, or SEC1).
    pub key_path: Option<String>,
    /// PEM CA certificates for outgoing client and replication connections.
    pub ca_path: Option<String>,
    /// Name verified against the peer certificate. Defaults to address host.
    pub server_name: Option<String>,
}

impl ProtocolConfig {
    const fn default_max_message_bytes() -> u32 {
        4 * 1024 * 1024
    }

    const fn default_idle_timeout_secs() -> u64 {
        30
    }

    fn default_close_command() -> String {
        "CLOSE".into()
    }

    fn default_timeout_response() -> String {
        "error:timeout".into()
    }

    fn default_invalid_command_response() -> String {
        "error:invalid command".into()
    }

    fn validate(&self) -> Result<()> {
        if self.max_message_bytes == 0 {
            return Err(KeyzError::InvalidConfig(
                "protocol.max_message_bytes must be greater than zero".into(),
            ));
        }

        if self.idle_timeout_secs == 0 {
            return Err(KeyzError::InvalidConfig(
                "protocol.idle_timeout_secs must be greater than zero".into(),
            ));
        }
        if std::time::Instant::now()
            .checked_add(Duration::from_secs(self.idle_timeout_secs))
            .is_none()
        {
            return Err(KeyzError::InvalidConfig(
                "protocol.idle_timeout_secs is too large".into(),
            ));
        }

        if self.close_command.trim().is_empty() {
            return Err(KeyzError::InvalidConfig(
                "protocol.close_command cannot be empty".into(),
            ));
        }

        if self.timeout_response.trim().is_empty() {
            return Err(KeyzError::InvalidConfig(
                "protocol.timeout_response cannot be empty".into(),
            ));
        }

        if self.invalid_command_response.trim().is_empty() {
            return Err(KeyzError::InvalidConfig(
                "protocol.invalid_command_response cannot be empty".into(),
            ));
        }

        Ok(())
    }

    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }
}

impl Default for ProtocolConfig {
    fn default() -> Self {
        Self {
            max_message_bytes: Self::default_max_message_bytes(),
            idle_timeout_secs: Self::default_idle_timeout_secs(),
            close_command: Self::default_close_command(),
            timeout_response: Self::default_timeout_response(),
            invalid_command_response: Self::default_invalid_command_response(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ReplicationRole {
    #[default]
    Disabled,
    Leader,
    Follower,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReplicationConfig {
    #[serde(default)]
    pub role: ReplicationRole,
    #[serde(default = "ReplicationConfig::default_bind")]
    pub bind: String,
    pub leader_address: Option<String>,
}

impl ReplicationConfig {
    fn default_bind() -> String {
        "0.0.0.0:7668".into()
    }

    pub fn bind_addr(&self) -> Result<std::net::SocketAddr> {
        self.bind.parse().map_err(|_| {
            KeyzError::InvalidConfig(format!("invalid replication bind address: {}", self.bind))
        })
    }

    pub fn leader_addr(&self) -> Result<std::net::SocketAddr> {
        let addr = self.leader_address.as_ref().ok_or_else(|| {
            KeyzError::InvalidConfig(
                "replication.leader_address is required for follower role".into(),
            )
        })?;
        addr.to_socket_addrs()
            .map_err(|_| {
                KeyzError::InvalidConfig(format!("cannot resolve leader_address: {addr}"))
            })?
            .next()
            .ok_or_else(|| {
                KeyzError::InvalidConfig("leader_address resolved to no addresses".into())
            })
    }

    fn validate(&self) -> Result<()> {
        match self.role {
            ReplicationRole::Leader => {
                self.bind_addr()?;
            }
            ReplicationRole::Follower => {
                let address = self.leader_address.as_deref().ok_or_else(|| {
                    KeyzError::InvalidConfig(
                        "replication.leader_address is required for follower role".into(),
                    )
                })?;
                // DNS is resolved asynchronously on each reconnect, not during validation.
                let valid = if let Ok(address) = address.parse::<SocketAddr>() {
                    address.port() != 0
                } else if let Some((host, port)) = address.rsplit_once(':') {
                    !host.is_empty()
                        && host.len() <= 253
                        && host
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
                        && port.parse::<u16>().is_ok_and(|port| port != 0)
                } else {
                    false
                };
                if !valid {
                    return Err(KeyzError::InvalidConfig(format!(
                        "invalid leader_address: {address}"
                    )));
                }
            }
            ReplicationRole::Disabled => {}
        }
        Ok(())
    }
}

impl Default for ReplicationConfig {
    fn default() -> Self {
        Self {
            role: ReplicationRole::default(),
            bind: Self::default_bind(),
            leader_address: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follower_addresses_support_dns_and_bracketed_ipv6_without_lookup() {
        for address in ["not-resolved.example:7668", "localhost:7668", "[::1]:7668"] {
            let input = format!("[replication]\nrole='follower'\nleader_address='{address}'");
            assert!(Config::from_toml_str(&input).is_ok(), "rejected {address}");
        }
        for address in [
            "localhost",
            "host:0",
            "host:99999",
            "bad host:7668",
            "::1:7668",
            "https://host:7668",
        ] {
            let input = format!("[replication]\nrole='follower'\nleader_address='{address}'");
            assert!(Config::from_toml_str(&input).is_err(), "accepted {address}");
        }
    }

    #[test]
    fn leader_frame_limit_reserves_replication_record_metadata() {
        let maximum = crate::server::replication::MAX_REPLICATION_FRAME
            - crate::server::replication::REPLICATION_SET_OVERHEAD;
        let settings =
            |limit| format!("[replication]\nrole='leader'\n[protocol]\nmax_message_bytes={limit}");
        assert!(Config::from_toml_str(&settings(maximum)).is_ok());
        assert!(Config::from_toml_str(&settings(maximum + 1)).is_err());
    }

    #[test]
    fn listen_address_accepts_ipv6_host_without_manual_brackets() {
        let config = ServerConfig {
            host: "::1".into(),
            ..Default::default()
        };
        assert_eq!(config.socket_addr().unwrap(), "[::1]:7667".parse().unwrap());
    }

    #[test]
    fn defaults_when_input_empty() {
        let cfg = Config::from_toml_str("").expect("config should load");
        assert_eq!(cfg.server.port, 7667);
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.store.compression_threshold, 512);
        assert_eq!(cfg.protocol.max_message_bytes, 4 * 1024 * 1024);
    }

    #[test]
    fn rejects_zero_resource_limits_and_partial_tls_identity() {
        for input in [
            "[server]\nmax_connections=0",
            "[server]\nwork_queue_capacity=0",
            "[server]\nwrite_timeout_secs=0",
            "[server]\nshutdown_timeout_secs=0",
            "[security.tls]\nenabled=true\ncert_path='cert.pem'",
        ] {
            assert!(Config::from_toml_str(input).is_err(), "accepted {input}");
        }
    }

    #[test]
    fn non_ascii_encryption_key_is_an_error_instead_of_panicking() {
        let input = format!("a{}b", "é".repeat(31));
        assert_eq!(input.len(), 64);
        assert!(parse_hex_key(&input).is_err());
    }

    #[test]
    fn authentication_tokens_are_redacted_from_debug_output() {
        let security = SecurityConfig {
            auth_token: Some("private-secret".into()),
            replication_token: Some("replication-secret".into()),
            ..SecurityConfig::default()
        };
        let output = format!("{security:?}");
        assert!(!output.contains("private-secret"));
        assert!(!output.contains("replication-secret"));
        assert!(output.contains("[redacted]"));
    }

    #[test]
    fn parses_partial_overrides() {
        let cfg = Config::from_toml_str(
            r#"
            [server]
            host = "0.0.0.0"
            port = 7777

            [store]
            compression_threshold = 2048

            [protocol]
            idle_timeout_secs = 5
        "#,
        )
        .expect("config should parse");

        assert_eq!(cfg.server.host, "0.0.0.0");
        assert_eq!(cfg.server.port, 7777);
        assert_eq!(cfg.store.compression_threshold, 2048);
        assert_eq!(cfg.store.cleanup_interval_ms, 250);
        assert_eq!(cfg.protocol.idle_timeout_secs, 5);
        assert_eq!(
            cfg.protocol.max_message_bytes,
            ProtocolConfig::default().max_message_bytes
        );
    }

    #[test]
    fn rejects_invalid_protocol_values() {
        let err =
            Config::from_toml_str("[protocol]\nmax_message_bytes = 0").expect_err("should fail");
        assert!(matches!(err, KeyzError::InvalidConfig(_)));
    }

    #[test]
    fn defaults_to_memory_backend() {
        let cfg = Config::from_toml_str("").expect("config should load");
        assert_eq!(cfg.store.backend, BackendType::Memory);
    }

    #[test]
    fn parses_disk_backend_config() {
        let cfg = Config::from_toml_str(
            r#"
            [store]
            backend = "disk"

            [store.disk]
            data_dir = "./my_data"
            partition_count = 8
            cache_max_entries = 5000
            flush_interval_ms = 1000
        "#,
        )
        .expect("config should parse");

        assert_eq!(cfg.store.backend, BackendType::Disk);
        assert_eq!(cfg.store.disk.data_dir, "./my_data");
        assert_eq!(cfg.store.disk.partition_count, 8);
        assert_eq!(cfg.store.disk.cache_max_entries, 5000);
        assert_eq!(cfg.store.disk.flush_interval_ms, 1000);
        assert!(!cfg.store.disk.encryption_enabled);
    }

    #[test]
    fn rejects_zero_partition_count() {
        let err = Config::from_toml_str(
            r#"
            [store]
            backend = "disk"
            [store.disk]
            partition_count = 0
        "#,
        )
        .expect_err("should fail");
        assert!(matches!(err, KeyzError::InvalidConfig(_)));
    }

    #[test]
    fn rejects_invalid_encryption_key() {
        let err = Config::from_toml_str(
            r#"
            [store]
            backend = "disk"
            [store.disk]
            encryption_key = "not_valid_hex"
        "#,
        )
        .expect_err("should fail");
        assert!(matches!(err, KeyzError::InvalidConfig(_)));
    }

    #[test]
    fn disk_defaults_applied() {
        let cfg = Config::from_toml_str(
            r#"
            [store]
            backend = "disk"
        "#,
        )
        .expect("config should parse");

        assert_eq!(cfg.store.disk.data_dir, "./keyz_data");
        assert_eq!(cfg.store.disk.partition_count, 16);
        assert_eq!(cfg.store.disk.cache_max_entries, 10_000);
        assert_eq!(cfg.store.disk.flush_interval_ms, 5000);
    }

    #[test]
    fn defaults_to_disabled_replication() {
        let cfg = Config::from_toml_str("").expect("config should load");
        assert_eq!(cfg.replication.role, ReplicationRole::Disabled);
    }

    #[test]
    fn parses_leader_replication() {
        let cfg = Config::from_toml_str(
            r#"
            [replication]
            role = "leader"
            bind = "0.0.0.0:9000"
        "#,
        )
        .expect("config should parse");
        assert_eq!(cfg.replication.role, ReplicationRole::Leader);
        assert_eq!(cfg.replication.bind, "0.0.0.0:9000");
    }

    #[test]
    fn parses_follower_replication() {
        let cfg = Config::from_toml_str(
            r#"
            [replication]
            role = "follower"
            leader_address = "10.0.0.1:7668"
        "#,
        )
        .expect("config should parse");
        assert_eq!(cfg.replication.role, ReplicationRole::Follower);
        assert_eq!(
            cfg.replication.leader_address,
            Some("10.0.0.1:7668".to_string())
        );
    }

    #[test]
    fn rejects_follower_without_leader_address() {
        let err = Config::from_toml_str(
            r#"
            [replication]
            role = "follower"
        "#,
        )
        .expect_err("should fail");
        assert!(matches!(err, KeyzError::InvalidConfig(_)));
    }
}
