# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is Keyz

Keyz is a lightweight async TCP key-value server in Rust with a companion CLI client. It uses length-prefixed binary framing (4-byte big-endian), pluggable storage backends (in-memory DashMap or disk with custom .keyz file format), optional gzip compression, and per-key TTL with background expiry.

## Build and Run

Requires Rust 1.89 or later (standard-library file locking).

```bash
# Always specify --bin (workspace builds both server and CLI)
cargo run --bin keyz              # run server (binds 127.0.0.1:7667)
cargo run --bin keyz-cli -- --help  # CLI client

# Tests
cargo test --all-targets --locked # full suite, including binary integration tests
cargo test <test_name>            # single test
cargo test -- --nocapture         # with stdout
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings

# Docker
docker build -t keyz . && docker run --rm -p 7667:7667 keyz
```

## Architecture

Two binaries using the shared library in `src/lib.rs` (`config`, `protocol`, `client`, `transport`, `server`):

**Server (`src/main.rs` → `src/server/`):**
- `init.rs` — Bounded TCP accept loop, optional TLS/authentication, idle/write timeouts, shutdown tracking
- `worker.rs` — One bounded ordered storage thread; clients reject overload immediately, replication waits fairly, and accepted jobs drain even after client cancellation
- `health.rs` — Readiness, storage health, replication, connection and queue metrics
- `helpers.rs` — Length-prefixed frame read/write (4-byte BE header)
- `dispatcher.rs` — Validates typed and legacy requests and derives reply/close behavior together; `parse_set_command()` handles legacy `SET key value [EX secs]` syntax
- `commands.rs` — Legacy handler functions and store/health reporting
- `store.rs` — Facade over `Arc<dyn StorageBackend>`, handles compression/decompression, TTL computation, background cleaner thread
- `backend/mod.rs` — `StorageBackend` trait definition + `BackendStats`
- `backend/memory.rs` — `InMemoryBackend` using DashMap (default)
- `backend/disk.rs` — `DiskBackend` with partitioned .keyz format, AES-256-GCM encryption, AOF and independent checkpoints
- `backend/aof.rs` — Append-only file: record format (SET/DEL with CRC32), writer, reader/replay for crash recovery
- `backend/format.rs` — .keyz binary file format: 64-byte header, partition table, length-prefixed entries
- `backend/encryption.rs` — AES-256-GCM encrypt/decrypt per partition and AOF records
- `backend/cache.rs` — Legacy standalone LRU utility; disk no longer duplicates resident payloads here
- `error.rs` — Central `KeyzError` enum with semantic I/O mapping (e.g. `ConnectionReset` → `ClientDisconnected`)

**CLI (`src/bin/keyz-cli.rs`):** Clap app with `exec`, `interactive`, `batch`, `status`, `metrics`, `commands`, `config`, `analyze`, and `cluster`. Uses `keyz::client::KeyzClient` and shared backend modules. Typed requests use `exec --typed --raw '<JSON>'`. Only reads retry; writes with lost replies are uncertain and are never replayed automatically. Request socket I/O shares an absolute deadline across authentication, framing, TLS and the read retry, including buffered TLS operations. Blocking DNS and certificate-file reads cannot be cancelled by that deadline.

**Config (`src/config.rs`):** TOML-based with sections `[server]`, `[protocol]`, `[store]`, `[store.disk]`, `[replication]`, `[security]`, and `[security.tls]`. Load precedence: explicit path → `KEYZ_CONFIG` env var → `./keyz.toml` → built-in defaults. Backend selected via `store.backend = "memory"|"disk"`. Replication via `replication.role = "leader"|"follower"|"disabled"`.

**Replication (`src/server/replication/`):**
- `mod.rs` — Frame helpers for replication protocol (4-byte BE length prefix, same as client protocol)
- `leader.rs` — `ReplicationBroadcaster` (tokio broadcast channel), accepts followers, sends snapshot then live AOF stream
- `follower.rs` — Connects to leader, receives snapshot + live stream, applies to local store. Auto-reconnects on disconnect.

## Protocol

Client frames: `[4-byte BE length][UTF-8 payload]`. Legacy commands: `SET`, `GET`, `DEL`, `EXIN`, `INFO`, `HEALTH`, `CLOSE`. Typed frames use `KEYZ/1 ` plus JSON; values are byte arrays. Every typed command rejects unknown fields, including `info`, `health`, and `close`; invalid close requests do not close the connection. See `docs/protocol-v1.md`. Authenticated connections must complete framed `AUTH <token>` before commands; both client and replication listeners support TLS, and outgoing TLS verifies the server certificate. Replication data frames carry binary AOF records.

## Adding a New Server Command

1. Add the handler function in `commands.rs`
2. Update legacy dispatch in `dispatcher.rs` and, for typed support, the strict command schema in `protocol.rs` and `dispatch_typed()`
3. Add client retry/response classification only when safe, document the command in the CLI and `docs/protocol-v1.md`, and cover both success and malformed input

## Interactive REPL

The REPL (`keyz-cli interactive`) supports colon-commands: `:help`, `:commands`, `:config`, `:config init`, `:metrics`/`:info`, `:status`, `:cluster`, `:clear`, `:quit`/`:exit`. Everything else is sent as a server command. New REPL commands are added as match arms in `handle_interactive()`.

## Adding a New Storage Backend

1. Implement `StorageBackend` trait in `backend/` (see `memory.rs` or `disk.rs` as examples)
2. Add a variant to `BackendType` enum in `config.rs`
3. Add a match arm in `Store::with_config()` in `store.rs`

## Key Design Choices

- **StorageBackend trait** abstracts storage — `Store` is a facade that delegates to `Arc<dyn StorageBackend>`
- **InMemoryBackend**: DashMap shard locks plus an outer lock for atomic replacement/accounting.
- **DiskBackend**: All partitions are RAM-resident; the duplicate LRU is unused (`cache_max_entries` remains accepted). Independent checkpoint schedule and explicit shutdown flush; Drop is a fallback.
- **Directory ownership**: Disk storage holds an exclusive OS lock on `.keyz.lock` for its lifetime. Do not delete or replace the lock file; a second process must fail before reading recovery files. Crash termination releases the lock.
- **AOF**: Mutations append before publication. Operation guards serialize byte accounting and disk mutations; snapshot sync and durable replacement precede log retirement. `every_write` syncs before acknowledgment; `every_second` is a periodic target. Failed append/per-write sync rolls back durably. Failed rollback or periodic sync blocks writes until checkpoint recovery. Do not weaken recovery ordering or preservation on failed validation.
- **Snapshot format**: Startup and offline analysis share checked table/range/count validation. Empty rows are entirely zero; data ranges must cover the complete data region without gaps or overlaps. Encoded partition sizes, including encryption overhead, must fit in `u32`; checkpoint failure must preserve the old snapshot and recovery log.
- Compression/decompression stays in `Store` (shared across backends)
- Wakeable maintenance independently schedules expiry cleanup, checkpointing, and log synchronization; errors remain visible in health until the corresponding operation recovers.
- Store is `Clone` (cheap Arc increment) and shared across all connection tasks
- Encryption key: `KEYZ_ENCRYPTION_KEY` env var takes precedence over `store.disk.encryption_key` in config
- Tokens: `KEYZ_AUTH_TOKEN` and `KEYZ_REPLICATION_TOKEN` override file values; replication falls back to the client token. See `docs/operations.md` for TLS and readiness semantics.
- Discovery: server-advertised endpoints are untrusted. Use `KeyzClient::for_discovered_address`, which accepts only exact numeric IP-and-port entries in `security.trusted_cluster_addresses`, before opening connections or resolving credentials. `for_address` retains credentials and is for explicitly selected trusted destinations. CLI cluster views skip untrusted probes by default.
- `max_bytes` counts keys and stored compressed payloads, not RSS. Connection/queue/frame limits bound other allocations. Every network storage operation goes through `StorageWorker`.
- Shutdown stops admission and drains accepted work before explicit flush. `shutdown_timeout_secs` bounds network draining, not storage synchronization.
- **Replication**: async leader→follower using AOF records over TCP. Leader broadcasts via `tokio::sync::broadcast`. Follower sends handshake (server address) on connect, then receives snapshot + live stream. Followers enforce read-only mode (SET/DEL rejected by dispatcher). Followers auto-reconnect on disconnect. `FollowerRegistry` tracks connected followers for cluster discovery via INFO.
- Live replication may idle before a frame starts; after its first byte, one `write_timeout_secs` deadline bounds the remaining header and payload. A partial-frame timeout clears readiness and reconnects. Internal storage admission waits fairly instead of treating transient queue pressure as a failed replication session.
- Performance work such as worker sharding, command batching, a binary value protocol, and checkpoints concurrent with reads/writes is not implemented. Preserve ordering and durability contracts when changing these paths; use `docs/benchmarks.md` for current measurements.
