# Keyz

`keyz` is a TCP key-value server written in Rust. It supports legacy text commands and a versioned protocol for exact binary values, optional TTLs, disk persistence, and asynchronous replication.

## Features
- Async TCP handling driven by Tokio, with bounded connections and an ordered storage worker that keeps compression and disk operations off runtime threads.
- Length-prefixed protocol that guards against partial reads and enforces message size limits.
- **Pluggable storage backends**: in-memory (`DashMap`) or persistent disk with a custom `.keyz` file format.
- **Disk backend**: exclusively owned data directories, validated partitioned snapshots, AES-256-GCM encryption at rest, and append-only file (AOF) recovery. Checkpoints, expiry cleanup, and log synchronization have independent schedules.
- Optional gzip compression and automatic cleanup of expired keys.
- Configurable responses, timeouts, and command vocabulary through a single TOML file.
- **Async replication**: leader-follower with automatic snapshot transfer and live AOF streaming. Followers serve read-only queries.
- **Shared Rust client and CLI**: persistent TCP/TLS connections, read-only retry, interactive REPL, and offline `.keyz` file analyzer.
- Optional token authentication and verified TLS for clients and replication.
- Graceful shutdown, readiness reporting, storage byte budgets, and structured operational events.
- Windows/Linux CI with regression, process recovery, TLS, and parser tests; coverage-guided fuzz targets and repeatable benchmarks.

## Getting Started

### Prerequisites
- Rust 1.89 or newer with `cargo` (directory locking uses the standard library file-lock API).

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
- `cluster status`: list nodes advertised by the leader's INFO endpoint and probe explicitly trusted follower addresses for connectivity, latency, and key count.
- `cluster monitor`: refresh the same cluster view continuously (Ctrl+C to stop).

The CLI reuses successful connections and discards a connection after any incomplete exchange. It retries `GET`, `EXIN`, `INFO`, and `HEALTH` at most once when time remains in the same request deadline. `--response-timeout` bounds socket I/O across authentication, framing, TLS and the retry; progress does not restart it. Writes are not automatically retried because the server may already have applied them. Batch `--stop-on-error` stops on server command failures as well as transport failures. `status` distinguishes reachable from ready and exits unsuccessfully when the server is not ready. See [operations](docs/operations.md) for deadline scope and DNS limitations.

### Docker
```bash
docker build -t keyz .
docker run --rm -p 7667:7667 keyz
```
The container ships both release binaries, listens on `0.0.0.0:7667`, runs as UID 10001, and checks readiness with the CLI. It defaults to memory storage. For persistence, mount a config and data directory writable by UID 10001; configure authentication/TLS for network use. `docker stop` sends SIGTERM and drains storage.

## Configuration
Configuration is defined in `keyz.toml`. Load precedence is:
1. Explicit path passed to `Config::load` (used internally by the binary).
2. `KEYZ_CONFIG` environment variable.
3. `keyz.toml` in the working directory.
4. Built-in defaults.

Sections include:
- `[server]`: listening host/port, maximum connections, storage queue capacity, write and shutdown timeouts.
- `[protocol]`: frame size limit, idle timeout, close command, and canned responses.
- `[store]`: backend selection (`memory` or `disk`), compression threshold, cleanup interval, optional default TTL, and `max_bytes` (0 = unlimited).
- `[store.disk]`: data directory, partition count, encryption settings, checkpoint interval, AOF settings (`aof_enabled`, `aof_fsync`). `cache_max_entries` remains accepted but has no effect: partitions already retain all payloads in RAM.
- `[replication]`: role (`disabled`, `leader`, or `follower`), leader bind address, follower's leader address.
- `[security]` and `[security.tls]`: token authentication, explicit trusted addresses for cluster discovery, and TLS certificate, key, and CA trust paths.

The encryption key can be provided via the `KEYZ_ENCRYPTION_KEY` environment variable (takes precedence) or `store.disk.encryption_key` in the config file.

See `keyz.toml` for an annotated example.

See [operations](docs/operations.md) for shutdown, readiness, durability, limits, and TLS setup. The byte budget counts key UTF-8 bytes plus stored payload bytes after compression; it is not a process memory limit.

## Replication

Keyz supports async leader-follower replication. The leader streams mutations (using the AOF record format) to connected followers over TCP or TLS. Followers serve read-only queries (`GET`, `EXIN`, `INFO`, `HEALTH`) and reject writes with `error:readonly`.

### Leader setup
```toml
[replication]
role = "leader"
bind = "0.0.0.0:7668"
```

### Follower setup
Create a separate config (e.g., `follower.toml`) with a different port and data directory:
```toml
[server]
port = 7669

[store]
backend = "disk"
[store.disk]
data_dir = "./follower_data"

[replication]
role = "follower"
leader_address = "127.0.0.1:7668"
```
```bash
KEYZ_CONFIG=follower.toml cargo run --bin keyz
```

When a follower connects, it sends a handshake with its server address, then the leader sends a full snapshot of all entries followed by live mutations in commit order. A follower stages the snapshot and replaces its entire dataset only after the transfer completes, removing keys that disappeared while it was disconnected. An interrupted or corrupt transfer leaves the previous dataset available and triggers reconnection. Followers auto-reconnect on disconnect.

Replication waits fairly for storage queue capacity. An idle live stream is allowed, but a started frame must finish within `server.write_timeout_secs`; a stalled partial frame clears readiness and triggers reconnection. TLS and token authentication, when enabled, precede the address handshake.

Disk snapshots are synced and replaced before their recovery log is retired. Snapshot flushes pause disk operations during this commit. Failed loading, including a wrong encryption key, preserves the database and log. Full follower replacements use a temporary recovery marker so a restart cannot replay the old dataset over the replacement.

Only one backend may own a disk data directory at a time. The OS releases its lock after a crash; keep `.keyz.lock` in place while an owner is running. Failed log appends roll back durably, and an unrecoverable log error blocks further writes until checkpoint recovery. Snapshot metadata and 32-bit format limits are validated before publication or log retirement. See [persistence guarantees](docs/operations.md#persistence-and-memory).

### Cluster Monitoring
Point the CLI at a leader to list advertised instances. Authorize automatic follower probes with exact numeric IP-and-port entries in the CLI configuration:

```toml
[security]
trusted_cluster_addresses = ["127.0.0.1:7669", "[::1]:7670"]
```

This permits use of the CLI's configured credentials at those destinations. Other advertised nodes are listed as `untrusted`, with `probe_skipped=true` in JSON, and receive no connection. Direct `--host`/`--port` selection remains available.

```bash
# One-shot status
cargo run --bin keyz-cli -- cluster status

# Live dashboard (refreshes every 2s)
cargo run --bin keyz-cli -- cluster monitor

# Also available in the interactive REPL
keyz> :cluster
```

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
| `HEALTH` | Liveness, readiness, storage, replication, connection, and queue state. | JSON payload |

Invalid commands receive the configured `invalid_command_response`, and idle connections receive `timeout_response` and are dropped.

For exact values and unambiguous results, use [protocol v1](docs/protocol-v1.md). Typed frames contain `KEYZ/1 ` followed by JSON; values are byte arrays and missing keys, errors, values, and TTLs have distinct response types. Existing text clients continue to work. The reusable client is `keyz::client::KeyzClient`.

## Testing
Run the same checks used by Windows/Linux CI:
```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

Process tests force termination during checkpoints, reject concurrent directory owners, and exercise replication reconnects. TLS tests verify trust, names, authentication and absolute deadlines; protocol tests cover malformed CLOSE and continued connection use. Storage tests inject partial-write, synchronization and rollback failures and check that invalid snapshots preserve recovery sources. Linux additionally tests SIGTERM persistence. `tests/parser_robustness.rs` runs deterministic mutations on every test run; `fuzz/` contains libFuzzer targets. See [benchmarks](docs/benchmarks.md) for latency and memory measurements during checkpoints and full resync.

## Project Layout
- `src/main.rs`: entry point, configuration loading, listener bootstrap.
- `src/lib.rs`: shared library exports for server and clients.
- `src/protocol.rs`, `src/client.rs`, `src/transport.rs`: typed protocol, reusable client, and TLS transport.
- `src/config.rs`: typed configuration loader and validation logic.
- `src/server/init.rs`: accept loop and per-connection protocol handling.
- `src/server/dispatcher.rs`: parses commands and routes to handlers.
- `src/server/commands.rs`: implementations of `SET`, `GET`, `DEL`, `EXIN`, and `INFO`.
- `src/server/store.rs`: storage facade with TTL, compression, and background cleanup.
- `src/server/worker.rs`, `src/server/health.rs`: bounded storage execution and operational health.
- `src/server/backend/`: pluggable storage backends.
  - `mod.rs`: `StorageBackend` trait definition.
  - `memory.rs`: in-memory backend using `DashMap`.
  - `disk.rs`: disk backend with partitions, encryption, and AOF integration.
  - `aof.rs`: append-only file for crash recovery (record format, writer, reader/replay).
  - `format.rs`: custom `.keyz` binary file format (header, partition table, entries).
  - `encryption.rs`: AES-256-GCM encrypt/decrypt per partition and AOF records.
  - `cache.rs`: standalone legacy LRU utility, no longer used by the disk backend.
- `src/server/replication/`: async leader-follower replication.
  - `mod.rs`: replication frame protocol helpers.
  - `leader.rs`: broadcast channel, follower accept loop, snapshot + live stream.
  - `follower.rs`: connect to leader, receive snapshot + live stream, apply records.
- `docs/`: supplemental documentation, including the architecture deep dive.

## Additional Documentation

- [Architecture](docs/keyz-architecture.md): runtime flow and components.
- [Protocol v1](docs/protocol-v1.md): typed requests, responses and shared client.
- [Operations](docs/operations.md): security, discovery trust, durability, limits and health.
- [Benchmarks](docs/benchmarks.md): repeatable workloads and measurement limits.
- [Changelog](CHANGELOG.md): implemented changes and compatibility notes.
