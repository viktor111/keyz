# Keyz Server Architecture

## Overview
- Provides a TCP key-value server with a minimal text protocol.
- Uses Tokio for async I/O, spawn-per-connection handling, and length-prefixed framing.
- Pluggable storage backends: in-memory (`DashMap`) or persistent disk (custom `.keyz` file format with micro-partitions, AES-256-GCM encryption, and LRU read-through cache).
- Optional TTL, gzip compression, and background cleanup across both backends.

## Runtime Flow
1. `src/main.rs` loads configuration with `Config::load`, resolves the listen socket, and builds shared state (`Store`, `ProtocolConfig`).
2. `server::helpers::create_listener` binds the TCP listener; `server::init::start` enters the accept loop.
3. Each connection is handled on its own Tokio task. Incoming frames are read with `helpers::read_message`, dispatched to command handlers, and responses are written back with `helpers::write_message`.
4. Errors such as disconnects or timeouts are converted into `KeyzError` variants for consistent logging and control flow.

## Configuration (`src/config.rs`)
- Load order precedence: explicit path argument -> `KEYZ_CONFIG` environment variable -> `keyz.toml` in the working directory -> built-in defaults.
- `ServerConfig`: `host`, `port`, and validation that ensures a non-zero port and non-empty host.
- `StoreConfig`: `backend` (`memory` or `disk`), `compression_threshold`, `cleanup_interval_ms`, `default_ttl_secs` (optional default TTL applied when `SET` omits `EX`).
- `DiskConfig` (under `[store.disk]`): `data_dir`, `partition_count` (1–4096, default 16), `encryption_enabled`, `encryption_key` (64 hex chars), `cache_max_entries` (0 disables cache), `flush_interval_ms`. Encryption key may also be provided via the `KEYZ_ENCRYPTION_KEY` environment variable, which takes precedence over the config file value.
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

## Store Facade (`src/server/store.rs`)
- `Store` is a facade that delegates to `Arc<dyn StorageBackend>`, selected by `store.backend` config.
- Compression/decompression and TTL computation live in `Store`, shared across all backends.
- Background cleaner: dedicated thread (`CleanerState`) wakes every `cleanup_interval_ms`, calls `backend.purge_expired()`, and runs once more during shutdown.
- API surface exposed to command handlers:
  - `insert`: computes TTL deadline, applies compression, delegates to backend.
  - `get`: fetches from backend, decompresses if needed.
  - `delete`, `expires_in`: delegates directly to backend.
  - `len`, `stats`: aggregates backend stats with store-level metrics (uptime, compression threshold).

## Storage Backends (`src/server/backend/`)

### StorageBackend Trait (`backend/mod.rs`)
- Defines the interface: `insert`, `get`, `delete`, `expires_in`, `len`, `purge_expired`, `stats_extra`.
- Backends receive pre-compressed payloads with `compressed` flag; decompression is handled by `Store`.
- `as_any()` method enables type-specific downcasting (used for test helpers).

### InMemoryBackend (`backend/memory.rs`)
- `DashMap<String, ValueEntry>` for lock-free concurrent access.
- Lazy expiration on read: expired entries are removed when accessed.
- `purge_expired()` uses `DashMap::retain()` for batch cleanup.

### DiskBackend (`backend/disk.rs`)
- **Micro-partitions**: keys are hash-distributed across N partitions (default 16), each independently locked with `parking_lot::RwLock<PartitionState>`.
- **Partition state**: each partition holds a `HashMap<String, DiskValueEntry>` loaded into memory at startup from the `.keyz` file.
- **LRU read-through cache** (`backend/cache.rs`): optional bounded cache (configurable `cache_max_entries`, 0 to disable). On `get()`, cache is checked first; misses fall through to the partition and populate the cache. Inserts update the cache; deletes and expiry evict from it. Hit/miss counters are reported via `stats_extra()`.
- **Encryption** (`backend/encryption.rs`): when enabled, each partition is encrypted with AES-256-GCM using a unique random 12-byte nonce per flush. The nonce is prepended to the ciphertext. Key is 32 bytes, provided via hex in config or `KEYZ_ENCRYPTION_KEY` env var.
- **Flush strategy**: dirty partitions are flushed to disk during `purge_expired()` (driven by the cleaner thread) and on graceful shutdown via `Drop`. Flush writes to a temp file, then performs a safe 3-step swap (old → backup, temp → data, remove backup) to avoid data loss on crash.
- **Append-Only File (AOF)** (`backend/aof.rs`): crash recovery mechanism. Every `insert` and `delete` appends a CRC32-checksummed record to `data.aof` before acknowledging. On startup, the snapshot (`data.keyz`) is loaded first, then the AOF is replayed to recover post-snapshot writes. After each successful snapshot flush, the AOF is truncated. Lock ordering is AOF mutex → partition RwLock to ensure record order matches mutation order. Config: `aof_enabled` (default true), `aof_fsync` (`every_write` for zero data loss, `every_second` for better throughput — default). When encryption is enabled, AOF records are individually encrypted. Lazy expiry deletions in `get()`/`expires_in()` are not logged to the AOF — expiry is handled canonically by `purge_expired()`.
- **Persistence**: on startup, the existing `data.keyz` file is loaded (decrypted if needed) into partition `HashMap`s, then the AOF is replayed if present.

### Custom `.keyz` File Format (`backend/format.rs`)
```
[ FILE HEADER (64 bytes) ]
[ PARTITION TABLE (16 bytes × N) ]
[ PARTITION 0 DATA ]
[ PARTITION 1 DATA ]
  ...
[ PARTITION N-1 DATA ]
```
- **Header** (64 bytes): magic `KEZ\0`, version (1.0), partition count, encryption flag, compression algorithm, encryption salt, CRC32 checksum.
- **Partition table entry** (16 bytes): offset (u64 LE), length (u32 LE), entry count (u32 LE).
- **Entry format**: key length (u32 LE) + key (UTF-8) + value length (u32 LE) + value (bytes) + expires_at (u64 LE, 0 = no expiry) + flags (1 byte, bit 0 = compressed).
- When encryption is enabled, each partition block is: `[12-byte nonce][AES-256-GCM ciphertext + auth tag]`.

### AOF Record Format (`backend/aof.rs`)
Two record types, each ending with a CRC32 checksum:
- **SET (0x01)**: `[op:1][key_len:u32 LE][key][value_len:u32 LE][value][expires_at:u64 LE][compressed:1][crc32:u32 LE]`
- **DEL (0x02)**: `[op:1][key_len:u32 LE][key][crc32:u32 LE]`

When encryption is enabled, each record is wrapped: `[encrypted_len:u32 LE][nonce:12][AES-GCM ciphertext + tag]`. Replay stops at the first truncated or corrupt record (expected on crash — partial write).

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
- Unit tests cover configuration validation, socket helpers, store behaviors (compression, TTL, cleanup), dispatcher parsing, command flow, and all backend modules.
- Backend-specific tests: format serialization roundtrips, encryption roundtrips and tamper detection, LRU cache eviction and counters, disk persistence across reload, encrypted persistence, partition distribution, and cache hit paths.
- Asynchronous tests (annotated with `#[tokio::test]`) validate integration between dispatcher and store, and end-to-end message handling.
- Run with `cargo test` (77 tests across all modules).

## Running the Server
- **Local dev**: `cargo run` (or `cargo run --release`) starts the server with defaults. Provide alternate settings by editing `keyz.toml` or setting the `KEYZ_CONFIG` environment variable to another TOML file path before launch.
- **Configuration sample**: see `keyz.toml` for documented defaults. Override fields per section (`[server]`, `[protocol]`, `[store]`).
- **Docker**: build and run via the provided `Dockerfile`. The release binary is staged into a slim Debian image exposing port `7667`.

## CLI Tooling
- `keyz-cli` is compiled alongside the server (`cargo run --bin keyz-cli -- --help`).
- Imports `format.rs` and `encryption.rs` read-only via `#[path]` for offline file analysis.
- Global flags mirror configuration selection (`--config`, `--host`, `--port`, `--json`).
- Subcommands:
  - `exec`: send ad-hoc commands, with `--raw` for literal frames.
  - `commands`: list protocol verbs and usage notes.
  - `config show|init`: inspect effective configuration (including disk backend settings) or scaffold a template.
  - `status`: run one-off or continuous health checks (`--watch`), reporting latency.
  - `interactive`: readline REPL with colon-commands: `:help`, `:commands`, `:config`, `:config init`, `:metrics`/`:info`, `:status`, `:clear`, `:quit`/`:exit`. History persistence via `--history`.
  - `batch`: replay commands from files/stdin with optional `--stop-on-error`.
  - `metrics`: probes the `INFO` command and pretty-prints JSON payloads when available.
  - `analyze`: offline inspection of `.keyz` data files. Reads the file header, partition table, and (if unencrypted or `--encryption-key` provided) deserializes all entries to produce per-partition and aggregate statistics: key counts, active/expired breakdown, compression ratio, partition balance (min/max/stddev). Supports `--verbose` to list all keys and `--json` for machine-readable output.

## Operational Considerations
- **In-memory backend**: single-process and in-memory; restart loses state. Memory growth is bound only by stored values; configure `compression_threshold` and TTL defaults to mitigate.
- **Disk backend**: data survives restarts. All partition entries are loaded into memory at startup, so memory usage scales with dataset size. With AOF enabled (default), acknowledged writes survive crashes — the AOF is replayed on startup. With `aof_fsync = "every_write"`, no acknowledged write is lost even on power failure. With `"every_second"` (default), up to 1 second of writes may be lost on crash. With AOF disabled, a crash between snapshot flushes loses recent writes.
- Length-prefixed protocol is binary-safe (values may contain spaces but not null bytes when interpreted as UTF-8).
- Adjust `ProtocolConfig.max_message_bytes` to control resource usage per message.
- Encryption key management: storing the key in the config file is convenient for development but not recommended for production. Prefer the `KEYZ_ENCRYPTION_KEY` environment variable.

## Extending Keyz
- **New commands**: update `dispatcher.rs` and provide corresponding functions in `commands.rs`.
- **New storage backends**: implement `StorageBackend` trait in `backend/`, add a `BackendType` variant in `config.rs`, and add a match arm in `Store::with_config()`.
- Enhance observability by instrumenting `init.rs` and the store with metrics/logging as needed; `KeyzError` can be extended for richer context.
