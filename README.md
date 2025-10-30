# Keyz

`keyz` is a lightweight TCP key-value server written in Rust. It uses a minimal text protocol with length-prefixed frames, supports optional per-key or default TTLs, and keeps data in memory for fast access.

## Features
- Async TCP handling driven by Tokio with per-connection tasks.
- Length-prefixed protocol that guards against partial reads and enforces message size limits.
- `DashMap`-based in-memory store with optional gzip compression and automatic cleanup of expired keys.
- Configurable responses, timeouts, and command vocabulary through a single TOML file.
- Unit-tested coverage for configuration parsing, protocol helpers, store behaviors, and command dispatching.

## Getting Started

### Prerequisites
- Rust toolchain (1.70+ recommended) with `cargo`.

### Run Locally
```bash
cargo run
# or for optimized builds
cargo run --release
```
By default the server binds to `127.0.0.1:7667`. Update `keyz.toml` or point the `KEYZ_CONFIG` environment variable at another TOML file to override defaults.

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
- `[store]`: compression threshold, cleanup interval, optional default TTL (applied when `SET` omits `EX`).

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
- `src/server/commands.rs`: implementations of `SET`, `GET`, `DEL`, and `EXIN`.
- `src/server/store.rs`: in-memory store with TTL, compression, and background cleanup.
- `docs/`: supplemental documentation, including the architecture deep dive.

## Additional Documentation
For a more detailed tour of the runtime flow and components, see `docs/keyz-architecture.md`.
