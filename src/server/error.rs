use std::string::FromUtf8Error;
use std::time::SystemTimeError;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, KeyzError>;

#[derive(Debug, Error)]
pub enum KeyzError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid socket address")]
    InvalidSocketAddress,
    #[error("Config IO error at {path}: {source}")]
    ConfigIo {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Config parse error: {0}")]
    ConfigParse(String),
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("Invalid command: {0}")]
    InvalidCommand(String),
    #[error("Invalid UTF-8 data: {0}")]
    InvalidUtf8(#[from] FromUtf8Error),
    #[error("System time error: {0}")]
    Time(#[from] SystemTimeError),
    #[error("Client timed out")]
    ClientTimeout,
    #[error("Client disconnected")]
    ClientDisconnected,
    #[error("Encryption error: {0}")]
    Encryption(String),
    #[error("Corrupt data file: {0}")]
    CorruptDataFile(String),
    #[error("Backend error: {0}")]
    Backend(String),
    #[error("Storage work queue is full")]
    Overloaded,
    #[error("Server is shutting down")]
    ShuttingDown,
    #[error("Storage capacity exceeded (used {used_bytes}, requested {requested_bytes}, limit {max_bytes} bytes)")]
    CapacityExceeded {
        used_bytes: u64,
        requested_bytes: u64,
        max_bytes: u64,
    },
    #[error("TLS error: {0}")]
    Tls(String),
    #[error("Authentication failed")]
    Unauthorized,
}
