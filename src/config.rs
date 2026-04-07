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
    if hex.len() != 64 {
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

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub protocol: ProtocolConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            store: StoreConfig::default(),
            protocol: ProtocolConfig::default(),
        }
    }
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
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "ServerConfig::default_host")]
    pub host: String,
    #[serde(default = "ServerConfig::default_port")]
    pub port: u16,
}

impl ServerConfig {
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
        let addr = format!("{host}:{}", self.port);
        addr.to_socket_addrs()
            .map_err(|_| KeyzError::InvalidSocketAddress)?
            .next()
            .ok_or(KeyzError::InvalidSocketAddress)
    }

    fn validate(&mut self) -> Result<()> {
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
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BackendType {
    Memory,
    Disk,
}

impl Default for BackendType {
    fn default() -> Self {
        BackendType::Memory
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AofFsyncPolicy {
    EveryWrite,
    EverySecond,
}

impl Default for AofFsyncPolicy {
    fn default() -> Self {
        AofFsyncPolicy::EverySecond
    }
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

    fn validate(&self) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_input_empty() {
        let cfg = Config::from_toml_str("").expect("config should load");
        assert_eq!(cfg.server.port, 7667);
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.store.compression_threshold, 512);
        assert_eq!(cfg.protocol.max_message_bytes, 4 * 1024 * 1024);
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
}
