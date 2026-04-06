# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is Keyz

Keyz is a lightweight async TCP key-value server in Rust with a companion CLI client. It uses length-prefixed binary framing (4-byte big-endian), pluggable storage backends (in-memory DashMap or disk with custom .keyz file format), optional gzip compression, and per-key TTL with background expiry.

## Build and Run

```bash
# Always specify --bin (workspace builds both server and CLI)
cargo run --bin keyz              # run server (binds 127.0.0.1:7667)
cargo run --bin keyz-cli -- --help  # CLI client

# Tests
cargo test                        # full suite
cargo test <test_name>            # single test
cargo test -- --nocapture         # with stdout

# Docker
docker build -t keyz . && docker run --rm -p 7667:7667 keyz
```

## Architecture

Two binaries in a single crate:

**Server (`src/main.rs` → `src/server/`):**
- `init.rs` — TCP accept loop, spawns a Tokio task per connection with idle timeout
- `helpers.rs` — Length-prefixed frame read/write (4-byte BE header)
- `dispatcher.rs` — Parses raw text into commands, routes to handlers; `parse_set_command()` handles `SET key value [EX secs]` syntax
- `commands.rs` — Handler functions: `set`, `get`, `delete`, `expires_in`, `info`
- `store.rs` — Facade over `Arc<dyn StorageBackend>`, handles compression/decompression, TTL computation, background cleaner thread
- `backend/mod.rs` — `StorageBackend` trait definition + `BackendStats`
- `backend/memory.rs` — `InMemoryBackend` using DashMap (default)
- `backend/disk.rs` — `DiskBackend` with micro-partitioned .keyz file format, LRU read-through cache, AES-256-GCM encryption
- `backend/format.rs` — .keyz binary file format: 64-byte header, partition table, length-prefixed entries
- `backend/encryption.rs` — AES-256-GCM encrypt/decrypt per partition
- `backend/cache.rs` — LRU cache wrapper with hit/miss counters
- `error.rs` — Central `KeyzError` enum with semantic I/O mapping (e.g. `ConnectionReset` → `ClientDisconnected`)

**CLI (`src/bin/keyz-cli.rs`):** Single-file Clap app with subcommands: `exec`, `interactive`, `batch`, `status`, `metrics`, `commands`, `config`.

**Config (`src/config.rs`):** TOML-based with sections `[server]`, `[protocol]`, `[store]`, `[store.disk]`. Load precedence: explicit path → `KEYZ_CONFIG` env var → `./keyz.toml` → built-in defaults. Backend selected via `store.backend = "memory"|"disk"`.

## Protocol

All frames: `[4-byte BE length][UTF-8 payload]`. Commands: `SET`, `GET`, `DEL`, `EXIN`, `INFO`, `CLOSE`. Responses are plain text except `INFO` which returns JSON.

## Adding a New Command

1. Add the handler function in `commands.rs`
2. Add a match arm in `dispatcher.rs` → `dispatcher()`
3. Add the command description to the `commands` list in `keyz-cli.rs`

## Adding a New Storage Backend

1. Implement `StorageBackend` trait in `backend/` (see `memory.rs` or `disk.rs` as examples)
2. Add a variant to `BackendType` enum in `config.rs`
3. Add a match arm in `Store::with_config()` in `store.rs`

## Key Design Choices

- **StorageBackend trait** abstracts storage — `Store` is a facade that delegates to `Arc<dyn StorageBackend>`
- **InMemoryBackend**: DashMap for lock-free concurrent reads/writes
- **DiskBackend**: Custom .keyz file format with micro-partitions (hash-distributed, `RwLock` per partition), AES-256-GCM encryption per partition, LRU read-through cache. Flush on purge_expired and on Drop.
- Compression/decompression stays in `Store` (shared across backends)
- Background cleaner thread calls `backend.purge_expired()` periodically
- Store is `Clone` (cheap Arc increment) and shared across all connection tasks
- Encryption key: `KEYZ_ENCRYPTION_KEY` env var takes precedence over `store.disk.encryption_key` in config
