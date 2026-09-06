# Keyz Server Architecture

## Overview
- Provides a TCP key-value server with legacy text commands and typed protocol v1 for exact byte values and distinct missing/error/TTL results.
- Uses Tokio for async I/O, spawn-per-connection handling, and length-prefixed framing.
- Pluggable storage backends: in-memory (`DashMap`) or persistent disk (custom `.keyz` file format with partitions, AES-256-GCM encryption, and an append-only recovery log).
- Optional TTL, gzip compression, and background cleanup across both backends.

## Runtime Flow
1. `src/main.rs` loads configuration, TLS identity, listeners, and storage through the shared library.
2. `server::init::start_until` accepts up to the connection limit; connections complete optional TLS and token authentication.
3. One ordered storage thread executes commands, compression, locking, and disk operations. Its bounded queue rejects excess client work immediately; internal replication jobs wait fairly for admission. Tokio handles socket reads and bounded writes.
4. Signals and tracked tasks coordinate shutdown: stop admission, close idle clients, drain admitted work, stop maintenance, and explicitly flush. Startup/shutdown failures exit nonzero.

`src/lib.rs` exports configuration, protocol, client, transport, and server modules. The CLI uses the shared client rather than importing implementation files through `#[path]`. See [protocol v1](protocol-v1.md), [operations](operations.md), and [benchmarks](benchmarks.md) for contracts and examples.

## Configuration (`src/config.rs`)
- Load order precedence: explicit path argument -> `KEYZ_CONFIG` environment variable -> `keyz.toml` in the working directory -> built-in defaults.
- `ServerConfig`: `host`, `port`, `max_connections`, `work_queue_capacity`, `write_timeout_secs`, and `shutdown_timeout_secs` (network drain deadline).
- `StoreConfig`: backend, compression threshold, cleanup interval, default TTL, and `max_bytes` (key plus stored payload bytes; 0 means unlimited).
- `DiskConfig`: data directory, partition count, encryption, checkpoint interval, and AOF policy. `cache_max_entries` is a compatibility no-op. `KEYZ_ENCRYPTION_KEY` overrides the file encryption key.
- `SecurityConfig`: client/replication tokens (environment overrides), explicit IP-and-port trust for discovery probes, TLS certificate/private key, outgoing CA trust, and optional verified server name. Authentication precedes commands and replication snapshots.
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
  | `HEALTH` | Report lifecycle, storage health, and follower readiness. | JSON payload |
- Invalid commands trigger `ProtocolConfig.invalid_command_response`. Idle clients receive `timeout_response` and the server drops the connection.
- Typed requests reject unknown fields, including extra fields on `info`, `health`, and `close`. The dispatcher derives the response and close decision from the same validated command; an invalid close request returns an error and leaves the connection open.

## Store Facade (`src/server/store.rs`)
- `Store` is a facade that delegates to `Arc<dyn StorageBackend>`, selected by `store.backend` config.
- Compression/decompression and TTL computation live in `Store`, shared across all backends.
- TTL deadlines use checked arithmetic; unrepresentable deadlines return an invalid-command response without changing the key or closing a valid connection.
- A shared mutation mutex orders backend commits, replication publication, and the snapshot/subscription boundary.
- Maintenance: a wakeable thread independently schedules expiry cleanup, checkpoints, and AOF synchronization. Errors update health and counters. Shutdown stops maintenance and explicitly flushes; `Drop` is a fallback.
- API surface exposed to command handlers:
  - `insert_with_ttl`: computes a checked TTL deadline, applies compression, delegates to backend. The legacy `insert` method retains its default-TTL behavior for a zero TTL.
  - `get`: fetches from backend, decompresses if needed.
  - `delete`, `expires_in`: delegates directly to backend.
  - `len`, `stats`: aggregates backend stats with store-level metrics (uptime, compression threshold).

## Storage Backends (`src/server/backend/`)

### StorageBackend Trait (`backend/mod.rs`)
- Defines mutation/read operations, byte accounting and limits, expiry cleanup, checkpoint/log synchronization, statistics, and full snapshot capture/replacement.
- Full snapshot replacement atomically swaps the dataset; disk replacement also retires the previous recovery history.
- Byte limits count UTF-8 keys plus stored payloads, including expired entries until removal. Overwrites subtract the replaced entry; over-budget writes and snapshot replacements preserve the current dataset.
- Backends receive pre-compressed payloads with `compressed` flag; decompression is handled by `Store`.
- `as_any()` method enables type-specific downcasting (used for test helpers).

### InMemoryBackend (`backend/memory.rs`)
- `DashMap<String, ValueEntry>` for concurrent access through shard locks. An outer `RwLock` allows ordinary operations to share access while complete snapshot replacement swaps the map exclusively.
- Reads hide expired entries without removing by key after releasing a read guard, so a concurrent live replacement cannot be deleted by a stale read.
- `purge_expired()` uses `DashMap::retain()` for batch cleanup.

### DiskBackend (`backend/disk.rs`)
- **Directory ownership**: an exclusive OS lock on `.keyz.lock` is acquired before recovery files are opened and held for the backend's lifetime. A second opener fails, including through directory aliases. The OS releases the lock after a crash; do not delete or replace the lock file while a process owns it.
- **Micro-partitions**: keys are hash-distributed across N partitions (default 16), each independently locked with `parking_lot::RwLock<PartitionState>`.
- **Partition state**: each partition holds a `HashMap<String, DiskValueEntry>` loaded into memory at startup from the `.keyz` file.
- **Resident payloads**: partitions already hold the dataset in RAM. The duplicate LRU is unused; `cache_max_entries` remains accepted and cache counters are zero. The standalone cache utility remains available in the library.
- **Encryption** (`backend/encryption.rs`): when enabled, each partition is encrypted with AES-256-GCM using a unique random 12-byte nonce per flush. The nonce is prepended to the ciphertext. Key is 32 bytes, provided via hex in config or `KEYZ_ENCRYPTION_KEY` env var.
- **Flush strategy**: dirty data is checkpointed at `flush_interval_ms`, independently of expiry cleanup, and explicitly at shutdown. An exclusive operation lock blocks reads and writes from snapshot capture through log retirement. The temporary snapshot is synced and durably replaces `data.keyz` (Unix directory sync; Windows write-through replacement). Only then is the AOF truncated and synced and dirty state cleared. `Drop` is a fallback after successful initialization.
- **Append-Only File (AOF)**: each mutation appends a CRC32 record before publishing state. The operation guard serializes writes and byte accounting; append/fsync precedes partition publication. `aof_enabled` defaults to true and `aof_fsync` selects `every_write` or `every_second`. Encryption also covers AOF records. Expiry removal rechecks the current entry and needs no log record because its saved deadline is authoritative.
- **Failed writes**: append or per-write synchronization failure truncates to the last accepted log boundary and synchronizes the rollback. Failed rollback or periodic synchronization blocks subsequent appends until a durable checkpoint and log retirement recover the writer. Already accepted records remain part of the checkpoint; failed writes are not published.
- **Persistence**: startup validates the complete partition table, ranges, and decoded counts before accepting a snapshot, then replays the AOF before enabling drop-time persistence. Wrong keys and corrupt complete records fail without truncating the recovery log. Interrupted legacy swaps recover from `.bak` or `.tmp` when the primary snapshot is absent.
- **Full replacement**: a synced `data.keyz.replace` snapshot is a recovery marker for replacing the whole dataset. Its publication makes the old AOF obsolete; finalization retires that log before installing the replacement at `data.keyz`. Startup finishes an interrupted replacement before replaying any later writes. A failed finalization blocks further mutations until recovery succeeds.

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
- Lengths and counts use checked conversions. Each encoded partition must fit in `u32` bytes (4 GiB minus one byte), including encryption overhead. An oversized checkpoint fails before snapshot replacement or log retirement.
- Shared validation used by startup and CLI `analyze` rejects inconsistent empty partitions, invalid or overflowing ranges, overlaps, gaps, trailing bytes, and decoded entry-count mismatches. Nonempty partition ranges must exactly cover the data region; empty partitions have zero offset, length, and count. Analysis of an encrypted file without its key validates metadata only; payload integrity and decoded counts require decryption.

### AOF Record Format (`backend/aof.rs`)
Two record types, each ending with a CRC32 checksum:
- **SET (0x01)**: `[op:1][key_len:u32 LE][key][value_len:u32 LE][value][expires_at:u64 LE][compressed:1][crc32:u32 LE]`
- **DEL (0x02)**: `[op:1][key_len:u32 LE][key][crc32:u32 LE]`

When encryption is enabled, each record is wrapped: `[encrypted_len:u32 LE][nonce:12][AES-GCM ciphertext + tag]`. Replay tolerates an incomplete trailing record after a valid prefix. Authentication failures, CRC failures, and malformed complete records return an error and preserve the log. A nonempty log with no verifiable record also stops startup: an incomplete first write cannot be distinguished reliably from an incompatible encryption configuration. The file is retained for recovery instead of being discarded.

## Connection Lifecycle (`src/server/init.rs`)
- Accept errors propagate to the supervised server task and trigger shutdown. Excess connections close before dispatch.
- Per-connection loop:
  - Reads a frame with a timeout (`tokio::time::timeout` using `ProtocolConfig.idle_timeout`).
  - Rejects invalid frame lengths and closes that connection, preventing unread payload bytes from becoming new commands.
  - Submits complete frames to the bounded storage worker after optional TLS/authentication. Full queues return `overloaded`; an admitted job still runs if its client disconnects.
  - Writes responses with a timeout and size limit. `CLOSE` closes one connection; process signals stop listeners and drain storage.
- Converts I/O edge cases to domain errors (`KeyzError::ClientDisconnected`, `ClientTimeout`) to avoid noisy logs.
- Accepts a `read_only` flag (true for follower nodes), passed through connection handling to dispatch. When read-only, `SET` and `DEL` return `error:readonly` for legacy requests or the typed `readonly` error.

## Replication (`src/server/replication/`)

### Protocol
Replication uses the same 4-byte big-endian length-prefixed framing as the client protocol. After optional TLS and token authentication, the follower sends a handshake frame containing its server address (e.g., `127.0.0.1:7669`) for cluster discovery. Data frames contain a serialized `AofRecord` (with CRC32), limited to 64 MiB. A special single-byte `0xFF` frame marks the end of the snapshot phase. Advertised addresses are metadata, not trusted client-credential destinations.

Snapshot capture, serialization, decode, and apply work use fair waiting admission to the storage queue. Cancelling before admission submits no job; accepted jobs finish during shutdown. Temporary queue saturation alone does not trigger a new snapshot, although sustained broadcast lag still requires resynchronization.

### Leader (`replication/leader.rs`)
- `ReplicationBroadcaster` wraps `tokio::sync::broadcast::Sender<Vec<u8>>` (capacity 4096).
- `Store::insert()` and `Store::delete()` publish after each successful mutation while holding the same ordering lock used for the commit.
- `FollowerRegistry` tracks connection identities, server addresses, and connection times. A registration guard removes its own connection on exit; an older connection cannot remove its successor. The live loop observes peer closure even when no mutations arrive. Exposed via `INFO` for cluster discovery.
- `run_leader()` accepts follower TCP connections on a dedicated replication port.
- Per follower: reads handshake (server address), registers in `FollowerRegistry`, sends a snapshot (all current entries as `AofRecord::Set` frames), sends `SYNC_COMPLETE` sentinel, then streams live records from the broadcast channel.
- If a follower falls behind (channel overflow), it is disconnected and must reconnect for a fresh snapshot.

### Follower (`replication/follower.rs`)
- `run_follower()` runs an infinite reconnect loop with 2-second backoff.
- On connect: sends its server address, stages all snapshot frames until `SYNC_COMPLETE`, then atomically replaces the dataset via `Store::replace_replicated_snapshot()`. Absent keys are removed. Incomplete or corrupt transfers do not publish a partial snapshot. Then processes live frames indefinitely; corrupt live records force reconnection.
- Idle live streams may wait indefinitely before the first frame byte. Once a frame begins, its remaining header and payload share one `server.write_timeout_secs` deadline. Partial-frame stalls disconnect the session, clear readiness, and trigger reconnect. Authentication and snapshot reads also have deadlines.
- Replicated data is also written to the follower's local AOF (via `StorageBackend::insert`), providing crash recovery on the follower.
- Deletes are applied via `Store::delete()`, which bypasses the replication broadcaster (followers have no broadcaster set).

### Snapshot Consistency
The leader captures the snapshot and subscribes while holding the mutation-order lock. Later writes enter the channel after the captured state, including writes during network transfer. The follower serves its previous dataset while receiving the snapshot, then publishes the replacement in one operation. Snapshot staging temporarily requires memory for both datasets.

## Error Handling (`src/server/error.rs`)
- Central `KeyzError` enum unifies I/O, parsing, UTF-8, time, and protocol violations.
- Helpers translate I/O error kinds into semantic errors so callers can branch on disconnect vs. fatal failure.
- `Result<T>` type alias keeps signatures compact across modules.

## Testing Strategy
- Unit tests cover configuration validation, socket helpers, store behaviors (compression, TTL, cleanup), dispatcher parsing, command flow, and all backend modules.
- Backend-specific tests cover format roundtrips and limits, malformed metadata, encryption and tampering, directory ownership and crash release, partial append rollback, poisoned-log recovery, disk reload, and partition distribution. The standalone LRU utility has separate tests.
- Asynchronous tests (annotated with `#[tokio::test]`) validate integration between dispatcher and store, and end-to-end message handling.
- Regression tests cover failed-key recovery, interrupted snapshot commits, concurrent flush/write ordering, expiry races, follower replacement and queue pressure, partial replication frames, strict typed close behavior, client deadlines including buffered TLS, discovery trust, read retries, and batch command errors. Process tests kill and restart real binaries in isolated data directories.
- Run with `cargo test --all-targets --locked`. CI also runs formatting, strict Clippy, release builds, and separate parser fuzzing; see [operations](operations.md#verification).

## Running the Server
- **Local dev**: `cargo run --bin keyz` (optionally with `--release`) starts the server. Select another config through `KEYZ_CONFIG`.
- **Toolchain**: Rust 1.89 or later, required for standard-library file locking.
- **Configuration sample**: see `keyz.toml` for documented defaults. Override fields per section (`[server]`, `[protocol]`, `[store]`).
- **Docker**: build and run via the provided `Dockerfile`. The release binary is staged into a slim Debian image exposing port `7667`.

## CLI Tooling
- `keyz-cli` is compiled alongside the server (`cargo run --bin keyz-cli -- --help`).
- Reuses a TCP connection after complete exchanges. Every failed exchange discards its socket. Only known read commands retry once; writes and custom close commands never replay after an ambiguous failure.
- `--response-timeout` gives socket I/O one absolute request deadline across authentication, framing, TLS, and a safe read retry; partial progress cannot reset it. Connection establishment is also limited by `--connect-timeout`. Blocking DNS lookup and certificate-file reads cannot be cancelled by the socket deadline.
- Uses library modules for analysis and `KeyzClient` for TCP/TLS; `exec --typed` accepts versioned JSON requests.
- Global flags mirror configuration selection (`--config`, `--host`, `--port`, `--json`).
- Subcommands:
  - `exec`: send ad-hoc commands, with `--raw` for literal frames.
  - `commands`: list protocol verbs and usage notes.
  - `config show|init`: inspect effective configuration (including disk backend settings) or scaffold a template.
  - `status`: run one-off or continuous health checks (`--watch`), reporting latency.
  - `interactive`: readline REPL with colon-commands: `:help`, `:commands`, `:config`, `:config init`, `:metrics`/`:info`, `:status`, `:cluster`, `:clear`, `:quit`/`:exit`. History persistence via `--history`.
  - `batch`: execute commands from files/stdin with optional `--stop-on-error`, covering server command failures and transport failures. Response classification preserves arbitrary GET values and deleted keys beginning with `error:`.
  - `metrics`: probes the `INFO` command and pretty-prints JSON payloads when available.
  - `analyze`: offline inspection of `.keyz` data files. Reads the file header, partition table, and (if unencrypted or `--encryption-key` provided) deserializes all entries to produce per-partition and aggregate statistics: key counts, active/expired breakdown, compression ratio, partition balance (min/max/stddev). Supports `--verbose` to list all keys and `--json` for machine-readable output.
  - `cluster status`: queries the selected server's INFO and lists discovered nodes. It probes only numeric IP-and-port endpoints in the CLI's `security.trusted_cluster_addresses`; other entries are marked untrusted without opening a connection. Approved probes retain TLS and authentication settings.
  - `cluster monitor`: live-refreshing cluster dashboard with configurable interval (`--interval`, default 2s). Clears screen and re-renders on each cycle.

## Operational Considerations
- **In-memory backend**: restart loses state. The byte budget excludes map overhead, requests, decompressed replies, and temporary snapshot copies.
- **Disk backend**: all entries remain RAM-resident. `every_write` synchronizes before acknowledgment; `every_second` permits recent acknowledged loss after OS/power failure and is a scheduling target rather than a strict loss bound. Durability depends on filesystem/device synchronization. Without AOF, only completed checkpoints survive forced termination.
- Typed v1 values are byte arrays. Legacy commands retain their whitespace, TTL-suffix, and missing-value ambiguities.
- Adjust `ProtocolConfig.max_message_bytes` to control resource usage per message.
- Encryption key management: storing the key in the config file is convenient for development but not recommended for production. Prefer the `KEYZ_ENCRYPTION_KEY` environment variable.
- **Replication**: writes do not wait for follower acknowledgment. Lag is workload-dependent and has no fixed bound. Reconnects transfer a full snapshot. The broadcast channel holds 4096 records; lagging subscribers reconnect. Health exposes readiness, apply timestamps, reconnects, and missed broadcast records rather than exact byte offsets. Followers keep local recovery history.

## Extending Keyz
- **New commands**: update `dispatcher.rs` and provide corresponding handlers in `commands.rs`; typed commands also require the strict schema in `protocol.rs` and typed dispatch. Review client retry classification and document both protocols.
- **New storage backends**: implement `StorageBackend` trait in `backend/`, add a `BackendType` variant in `config.rs`, and add a match arm in `Store::with_config()`.
- **New replication modes**: the replication protocol is extensible. Synchronous replication could be added by having the leader wait for follower ACKs before acknowledging the client. Multi-leader replication would require conflict resolution.
- Enhance observability by instrumenting `init.rs` and the store with metrics/logging as needed; `KeyzError` can be extended for richer context.
