//! Reusable blocking TCP/TLS client. Each client owns one persistent connection.
use crate::{
    config::{ProtocolConfig, SecurityConfig},
    protocol::{self, Request, Response},
    transport::SyncStream,
};
use anyhow::{anyhow, Context, Result};
use std::{
    cell::RefCell,
    io::{Read, Write},
    net::SocketAddr,
    time::{Duration, Instant},
};

pub struct KeyzClient {
    address: String,
    connect_timeout: Duration,
    response_timeout: Duration,
    max_message_bytes: u32,
    close_command: String,
    security: SecurityConfig,
    conn: RefCell<Option<SyncStream>>,
}

impl KeyzClient {
    pub fn new(
        address: String,
        connect_timeout: Duration,
        response_timeout: Duration,
        protocol: &ProtocolConfig,
    ) -> Self {
        Self {
            address,
            connect_timeout,
            response_timeout,
            max_message_bytes: protocol.max_message_bytes,
            close_command: protocol.close_command.clone(),
            security: SecurityConfig::default(),
            conn: RefCell::new(None),
        }
    }

    /// Apply TLS and authentication settings before the first connection.
    pub fn with_security(mut self, security: SecurityConfig) -> Self {
        self.conn = RefCell::new(None);
        self.security = security;
        self
    }

    /// Create a connection to an explicitly selected, trusted address with the
    /// same credentials. For addresses obtained from INFO use
    /// `for_discovered_address`, which checks the discovery allowlist first.
    pub fn for_address(&self, address: String) -> Self {
        Self {
            address,
            connect_timeout: self.connect_timeout,
            response_timeout: self.response_timeout,
            max_message_bytes: self.max_message_bytes,
            close_command: self.close_command.clone(),
            security: self.security.clone(),
            conn: RefCell::new(None),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.conn.borrow().is_some()
    }

    /// Refuse untrusted discovery targets before DNS, connection, or credential
    /// resolution. Trust is an exact IP and port, never a hostname or wildcard.
    pub fn for_discovered_address(&self, address: &str) -> Result<Self> {
        let endpoint: SocketAddr = address
            .parse()
            .context("discovered address must be an IP and port")?;
        if !self.security.trusted_cluster_addresses.contains(&endpoint) {
            return Err(anyhow!(
                "discovered address is not in security.trusted_cluster_addresses"
            ));
        }
        Ok(self.for_address(endpoint.to_string()))
    }

    /// Send a typed request. Mutations are never automatically retried, even
    /// when their response is lost; IDs provide correlation, not deduplication.
    pub fn send_request(&self, request: &Request) -> Result<Response> {
        let raw = protocol::encode_request(request);
        let response = self.send(&raw)?;
        Ok(protocol::decode_response(&response)?)
    }

    /// Send a legacy command or an already encoded versioned request.
    pub fn send(&self, command: &str) -> Result<String> {
        if command.trim().is_empty() {
            return Err(anyhow!("command cannot be empty"));
        }

        if command.len() > self.max_message_bytes as usize {
            return Err(anyhow!(
                "command length {} exceeds configured max {} bytes",
                command.len(),
                self.max_message_bytes
            ));
        }
        // Reject malformed typed requests before anything is sent or mutated.
        if protocol::is_typed(command) {
            protocol::decode_request(command)?;
        }

        // A failed exchange may already have committed a mutation. Only reads
        // are safe to repeat without server-side request deduplication.
        let deadline = Instant::now()
            .checked_add(self.response_timeout)
            .ok_or_else(|| anyhow!("response timeout is too large"))?;
        match self.try_send(command, deadline) {
            Ok(response) => Ok(response),
            Err(_)
                if Instant::now() < deadline
                    && command != self.close_command
                    && is_read_command(command) =>
            {
                self.try_send(command, deadline)
            }
            Err(err) => Err(err
                .context("request failed; the command may have been applied and was not retried")),
        }
    }

    fn try_send(&self, command: &str, deadline: Instant) -> Result<String> {
        self.ensure_connected(deadline)?;
        // Keep the stream only after a complete exchange. In particular, the
        // last failed retry must not leave a late response for the next command.
        let mut stream = self.conn.borrow_mut().take().unwrap();
        stream.set_deadline(deadline);
        self.write_frame(&mut stream, command.as_bytes())?;
        let response = self.read_frame(&mut stream)?;
        if protocol::is_typed(command) {
            let request = protocol::decode_request(command)?;
            let envelope = protocol::decode_response(&response)?;
            if envelope.id != Some(request.id) {
                return Err(anyhow!("response ID does not match request ID"));
            }
        }
        let close = protocol::is_close(command, &self.close_command);
        stream
            .check_deadline()
            .context("request deadline expired")?;
        if !close {
            *self.conn.borrow_mut() = Some(stream);
        }
        Ok(response)
    }

    fn ensure_connected(&self, deadline: Instant) -> Result<()> {
        let mut conn = self.conn.borrow_mut();
        if conn.is_none() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| anyhow!("request deadline expired"))?;
            let mut stream = SyncStream::connect(
                &self.address,
                self.connect_timeout.min(remaining),
                &self.security.tls,
            )
            .with_context(|| format!("unable to connect to {}", self.address))?;
            stream.set_deadline(deadline);
            stream
                .set_nodelay(true)
                .context("unable to configure TCP_NODELAY")?;
            if let Some(token) = self.security.resolve_auth_token() {
                let command = format!("AUTH {token}");
                if command.len() > self.max_message_bytes as usize {
                    return Err(anyhow!("authentication frame exceeds max_message_bytes"));
                }
                self.write_frame(&mut stream, command.as_bytes())?;
                if self.read_frame(&mut stream)? != "ok" {
                    return Err(anyhow!("server authentication failed"));
                }
            }
            *conn = Some(stream);
        }
        Ok(())
    }

    fn write_frame(&self, stream: &mut SyncStream, payload: &[u8]) -> Result<()> {
        let len = payload.len();
        if len > u32::MAX as usize {
            return Err(anyhow!("payload too large to encode ({len} bytes)"));
        }

        let len_bytes = (len as u32).to_be_bytes();
        stream
            .write_all(&len_bytes)
            .context("failed to write frame length")?;
        stream
            .write_all(payload)
            .context("failed to write frame payload")?;
        stream.flush().context("failed to flush request frame")?;
        Ok(())
    }

    fn read_frame(&self, stream: &mut SyncStream) -> Result<String> {
        let mut len_bytes = [0u8; 4];
        stream
            .read_exact(&mut len_bytes)
            .context("failed to read response length")?;

        let len = u32::from_be_bytes(len_bytes);
        if len == 0 {
            return Err(anyhow!("server returned empty frame"));
        }
        if len > self.max_message_bytes {
            return Err(anyhow!(
                "response length {} exceeds max_message_bytes {}",
                len,
                self.max_message_bytes
            ));
        }

        let mut buffer = vec![0u8; len as usize];
        stream
            .read_exact(&mut buffer)
            .context("failed to read response payload")?;
        let response = String::from_utf8(buffer)?;
        Ok(response)
    }
}

fn is_read_command(command: &str) -> bool {
    if protocol::is_typed(command) {
        return protocol::decode_request(command)
            .is_ok_and(|request| request.command.is_read_only());
    }
    matches!(
        command.trim().split(' ').next(),
        Some("GET" | "EXIN" | "INFO" | "HEALTH")
    )
}
