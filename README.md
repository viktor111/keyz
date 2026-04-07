# Keyz

`keyz` is a lightweight TCP key-value server written in Rust. It uses a minimal text protocol with length-prefixed frames, supports optional per-key or default TTLs, and keeps data in memory for fast access.

## Features
- Async TCP handling driven by Tokio with per-connection tasks.
- Length-prefixed protocol that guards against partial reads and enforces message size limits.
- **Pluggable storage backends**: in-memory (`DashMap`) or persistent disk with a custom `.keyz` file format.
- **Disk backend**: micro-partitioned storage with automatic key clustering, AES-256-GCM encryption at rest, LRU read-through cache, and append-only file (AOF) for crash recovery.
- Optional gzip compression and automatic cleanup of expired keys.
- Configurable responses, timeouts, and command vocabulary through a single TOML file.
- Unit-tested coverage for configuration parsing, protocol helpers, store behaviors, backends, and command dispatching.

## Getting Started

### Prerequisites
- Rust toolchain (1.70+ recommended) with `cargo`.

### Run Locally
```bash
# run the TCP server
cargo run --bin keyz
# or for optimized builds
cargo run --release --bin keyz
```
By default the server binds to `127.0.0.1:7667`. Update `keyz.toml` or point the `KEYZ_CONFIG` environment variable at another TOML file to override defaults.
> Note: the workspace builds both the server and CLI binaries, so always pass `--bin` to choose which target to launch.

### Client CLI
The companion CLI `keyz-cli` ships with the crate and provides a rich interface for interacting with the server.

```bash
cargo run --bin keyz-cli -- --help
# run a single command (after the second `--` args go to the CLI)
cargo run --bin keyz-cli -- exec GET foo
```

Highlights:
- `exec`: send a single command (`keyz-cli exec SET foo bar EX 10` or `--raw "GET foo"`).
- `commands`: list supported verbs and optional notes (`--verbose`).
- `config show`: inspect the effective configuration (including disk backend settings) and whether it came from disk; `config init` scaffolds a `keyz.toml`.
- `status`: probe server health once or continuously with `--watch`.
- `interactive`: launch a readline-powered REPL with `:help`, `:commands`, `:config`, `:metrics`, `:status`, `:clear`, and `:quit`.
- `batch`: stream commands from stdin or a file, with `--stop-on-error` and `--json` for scripting.
- `metrics`: fetch diagnostics via the `INFO` command and pretty-print JSON, falling back gracefully when unsupported.
- `analyze`: inspect `.keyz` data files offline — shows header, partition table, key distribution, and summary statistics. Supports `--encryption-key` for encrypted files, `--verbose` to list all keys, and `--json` for machine-readable output.

### Docker
```bash
docker build -t keyz .
docker run --rm -p 7667:7667 keyz
```
The container ships the release binary and exposes port `7667`.

## Configuration
Configuration is defined in `keyz.toml`. Load precedence is:
1. Explicit path passed to `Config::load` (used internally by the binary).
2. `KEYZ_CONFIG` environment variable.
3. `keyz.toml` in the working directory.
4. Built-in defaults.

Sections include:
- `[server]`: listening host and port.
- `[protocol]`: frame size limit, idle timeout, close command, and canned responses.
- `[store]`: backend selection (`memory` or `disk`), compression threshold, cleanup interval, optional default TTL.
- `[store.disk]`: data directory, partition count, encryption settings, cache size, flush interval, AOF settings (`aof_enabled`, `aof_fsync`).

The encryption key can be provided via the `KEYZ_ENCRYPTION_KEY` environment variable (takes precedence) or `store.disk.encryption_key` in the config file.

See `keyz.toml` for an annotated example.

## Protocol Cheatsheet
All requests and responses are framed with a 4-byte big-endian length header. Commands supported out of the box:

| Command | Description | Success Response |
|---------|-------------|------------------|
| `SET <key> <value> [EX <seconds>]` | Insert or update a value. Optional TTL in seconds. | `ok` |
| `GET <key>` | Retrieve a value. | Stored value or `null` |
| `DEL <key>` | Delete a key. | Deleted key or `null` |
| `EXIN <key>` | Remaining TTL for a key. | Seconds or `null` |
| `CLOSE` | Graceful connection termination. | `Closing connection` |
| `INFO` | Emit JSON describing server/store metrics. | JSON payload |

Invalid commands receive the configured `invalid_command_response`, and idle connections receive `timeout_response` and are dropped.

## Testing
Run the full unit test suite with:
```bash
cargo test
```

## Project Layout
- `src/main.rs`: entry point, configuration loading, listener bootstrap.
- `src/config.rs`: typed configuration loader and validation logic.
- `src/server/init.rs`: accept loop and per-connection protocol handling.
- `src/server/dispatcher.rs`: parses commands and routes to handlers.
- `src/server/commands.rs`: implementations of `SET`, `GET`, `DEL`, `EXIN`, and `INFO`.
- `src/server/store.rs`: storage facade with TTL, compression, and background cleanup.
- `src/server/backend/`: pluggable storage backends.
  - `mod.rs`: `StorageBackend` trait definition.
  - `memory.rs`: in-memory backend using `DashMap`.
  - `disk.rs`: disk backend with micro-partitions, LRU cache, encryption, and AOF integration.
  - `aof.rs`: append-only file for crash recovery (record format, writer, reader/replay).
  - `format.rs`: custom `.keyz` binary file format (header, partition table, entries).
  - `encryption.rs`: AES-256-GCM encrypt/decrypt per partition and AOF records.
  - `cache.rs`: LRU read-through cache with hit/miss counters.
- `docs/`: supplemental documentation, including the architecture deep dive.

## Additional Documentation
For a more detailed tour of the runtime flow and components, see `docs/keyz-architecture.md`.
