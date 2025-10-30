use std::{
    env,
    fs,
    net::{SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

use crate::server::error::{KeyzError, Result};

const DEFAULT_CONFIG_PATH: &str = "keyz.toml";
const ENV_CONFIG_PATH: &str = "KEYZ_CONFIG";

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
        let explicit_path = path.map(|p| p.as_ref().to_path_buf());
        let env_path = env::var(ENV_CONFIG_PATH).ok().map(PathBuf::from);

        if let Some(path) = explicit_path.or(env_path) {
            let content = fs::read_to_string(&path).map_err(|source| KeyzError::ConfigIo {
                path: path.to_string_lossy().to_string(),
                source,
            })?;
            let config = Self::from_toml_str(&content)?;
            println!(
                "[config] Loaded configuration from {}",
                path.to_string_lossy()
            );
            return Ok(config);
        }

        match fs::read_to_string(DEFAULT_CONFIG_PATH) {
            Ok(content) => {
                let config = Self::from_toml_str(&content)?;
                println!("[config] Loaded configuration from {DEFAULT_CONFIG_PATH}");
                Ok(config)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                println!("[config] No configuration file found; using defaults");
                Ok(Self::default())
            }
            Err(source) => Err(KeyzError::ConfigIo {
                path: DEFAULT_CONFIG_PATH.to_string(),
                source,
            }),
        }
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

#[derive(Debug, Clone, Deserialize)]
pub struct StoreConfig {
    #[serde(default = "StoreConfig::default_compression_threshold")]
    pub compression_threshold: usize,
    #[serde(default = "StoreConfig::default_cleanup_interval_ms")]
    pub cleanup_interval_ms: u64,
    pub default_ttl_secs: Option<u64>,
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

        Ok(())
    }
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            compression_threshold: Self::default_compression_threshold(),
            cleanup_interval_ms: Self::default_cleanup_interval_ms(),
            default_ttl_secs: None,
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
}
