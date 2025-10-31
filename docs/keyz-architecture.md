# Keyz Server Architecture

## Overview
- Provides a TCP key-value server with a minimal text protocol.
- Uses Tokio for async I/O, spawn-per-connection handling, and length-prefixed framing.
- Persists data in an in-memory store backed by `DashMap` with optional TTL, compression, and background cleanup.

## Runtime Flow
1. `src/main.rs` loads configuration with `Config::load`, resolves the listen socket, and builds shared state (`Store`, `ProtocolConfig`).
2. `server::helpers::create_listener` binds the TCP listener; `server::init::start` enters the accept loop.
3. Each connection is handled on its own Tokio task. Incoming frames are read with `helpers::read_message`, dispatched to command handlers, and responses are written back with `helpers::write_message`.
4. Errors such as disconnects or timeouts are converted into `KeyzError` variants for consistent logging and control flow.

## Configuration (`src/config.rs`)
- Load order precedence: explicit path argument -> `KEYZ_CONFIG` environment variable -> `keyz.toml` in the working directory -> built-in defaults.
- `ServerConfig`: `host`, `port`, and validation that ensures a non-zero port and non-empty host.
- `StoreConfig`: `compression_threshold`, `cleanup_interval_ms`, `default_ttl_secs` (optional default TTL applied when `SET` omits `EX`).
- `ProtocolConfig`: frame limits (`max_message_bytes`), idle timeout, `CLOSE` command string, and stock responses for timeout/invalid input.
- Helper methods return strongly typed values (e.g., `socket_addr`, `idle_timeout`) and perform domain validation before the server boots.

## Command Protocol
- **Framing**: every request/response is prefixed with a 4-byte big-endian length (`helpers.rs`), preventing partial reads and enforcing size limits (`ProtocolConfig.max_message_bytes`).
- **Command Set** (see `server/dispatcher.rs`, `server/commands.rs`):
  | Command | Description | Response |
  |---------|-------------|----------|
  | `SET <key> <value> [EX <seconds>]` | Upsert value; optional TTL applied when `EX` present or via default TTL. | `"ok"` on success, `"error:set command invalid"` on parse issues. |
  | `GET <key>` | Fetch value. | Raw value or `"null"` if absent/expired. |
  | `DEL <key>` | Delete entry. | Deleted key or `"null"` when missing/expired. |
  | `EXIN <key>` | Query remaining TTL. | Seconds remaining or `"null"` if infinite/missing. |
  | `CLOSE` | Graceful connection close command (configurable). | `"Closing connection"` then socket shutdown. |
  | `INFO` | Emit server/store metrics. | JSON payload |
- Invalid commands trigger `ProtocolConfig.invalid_command_response`. Idle clients receive `timeout_response` and the server drops the connection.

## Store Implementation (`src/server/store.rs`)
- Data structure: `DashMap<String, ValueEntry>` for concurrent access without explicit locks.
- Payload handling:
  - Compression: values at or above `compression_threshold` are passed through gzip; decompressed transparently on read.
  - TTL: explicit `SET ... EX <seconds>` or default TTL from config. Expiry times stored as epoch seconds.
- Background cleaner: dedicated thread (`CleanerState`) wakes every `cleanup_interval_ms` to evict expired entries and runs once more during shutdown.
- API surface exposed to command handlers:
  - `insert`: applies TTL and compression decisions.
  - `get`: lazy-expiration check on read.
  - `delete`: removes key, skipping expired entries.
  - `expires_in`: reports remaining TTL.
  - `len`, `is_compressed`, and `stats` assist diagnostics/tests.

## Connection Lifecycle (`src/server/init.rs`)
- Accept loop retries with a short backoff on errors (`ACCEPT_BACKOFF`).
- Per-connection loop:
  - Reads a frame with a timeout (`tokio::time::timeout` using `ProtocolConfig.idle_timeout`).
  - Validates non-empty input, compares against configured `close_command`, then routes to the dispatcher.
  - Sends responses immediately; shutdown is graceful on `CLOSE`.
- Converts I/O edge cases to domain errors (`KeyzError::ClientDisconnected`, `ClientTimeout`) to avoid noisy logs.

## Error Handling (`src/server/error.rs`)
- Central `KeyzError` enum unifies I/O, parsing, UTF-8, time, and protocol violations.
- Helpers translate I/O error kinds into semantic errors so callers can branch on disconnect vs. fatal failure.
- `Result<T>` type alias keeps signatures compact across modules.

## Testing Strategy
- Unit tests cover configuration validation, socket helpers, store behaviors (compression, TTL, cleanup), dispatcher parsing, and command flow.
- Asynchronous tests (annotated with `#[tokio::test]`) validate integration between dispatcher and store, and end-to-end message handling.
- Run with `cargo test`; integration tests are not present, but unit coverage spans all core modules.

## Running the Server
- **Local dev**: `cargo run` (or `cargo run --release`) starts the server with defaults. Provide alternate settings by editing `keyz.toml` or setting the `KEYZ_CONFIG` environment variable to another TOML file path before launch.
- **Configuration sample**: see `keyz.toml` for documented defaults. Override fields per section (`[server]`, `[protocol]`, `[store]`).
- **Docker**: build and run via the provided `Dockerfile`. The release binary is staged into a slim Debian image exposing port `7667`.

## CLI Tooling
- `keyz-cli` is compiled alongside the server (`cargo run --bin keyz-cli -- --help`).
- Global flags mirror configuration selection (`--config`, `--host`, `--port`, `--json`).
- Subcommands:
  - `exec`: send ad-hoc commands, with `--raw` for literal frames.
  - `commands`: list protocol verbs and usage notes.
  - `config show|init`: inspect effective configuration or scaffold a template.
  - `status`: run one-off or continuous health checks (`--watch`), reporting latency.
  - `interactive`: provides a readline shell with `:help`, `:commands`, and history persistence.
  - `batch`: replay commands from files/stdin with optional `--stop-on-error`.
  - `metrics`: probes the `INFO` command and pretty-prints JSON payloads when available.
- Intended to stay forward-compatible as new protocol verbs or telemetry land on the server.

## Operational Considerations
- The server is single-process and in-memory; restart loses state.
- Memory growth is bound only by stored values; configure `compression_threshold` and TTL defaults to mitigate.
- Length-prefixed protocol is binary-safe (values may contain spaces but not null bytes when interpreted as UTF-8).
- Adjust `ProtocolConfig.max_message_bytes` to control resource usage per message.

## Extending Keyz
- Add new commands by updating `dispatcher.rs` and providing corresponding functions in `commands.rs`.
- To persist data, swap `DashMap` for a pluggable backend implementing the same API used by command handlers.
- Enhance observability by instrumenting `init.rs` and the store with metrics/logging as needed; `KeyzError` can be extended for richer context.
