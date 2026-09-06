# Operating Keyz

## Startup, shutdown, and health

Configuration, TLS identity, listeners, and storage are validated before serving requests. Startup errors exit with a nonzero status. `Ctrl+C`, Windows `Ctrl+Break`, and Unix `SIGTERM` stop admission and close idle connections. Requests already admitted to the storage queue finish even when their client disconnects. The worker then stops maintenance and explicitly flushes storage; a flush failure is logged and causes a nonzero exit.

`server.shutdown_timeout_secs` limits **network connection draining**. It does not discard accepted storage jobs or impose a deadline on filesystem synchronization. A stuck storage device can therefore delay shutdown. Force termination uses crash recovery and cannot provide graceful-shutdown guarantees.

`HEALTH` returns JSON; `INFO` includes the same `health` object plus `store.maintenance` diagnostics. `keyz-cli status` exits unsuccessfully when the server is reachable but not ready. Health fields include:

| Field | Meaning |
| --- | --- |
| `live`, `ready` | Serving lifecycle and readiness. Readiness also requires healthy storage and a completed follower snapshot. |
| `replication_state` | `disabled`, `leader`, `connecting`, `syncing`, `ready`, `disconnected`, or `stopping`. |
| `applied_records`, `last_apply_unix_secs` | Live records applied by a follower, and its last application time. |
| `reconnects`, `lagged_records` | Failed follower sessions and broadcast records missed by slow followers. |
| `active_connections`, `accepted_connections`, `rejected_connections` | Client connection counts. Replication has a separate connection limit using the same configured maximum. |
| `work_queue_depth`, `work_queue_capacity`, `rejected_operations`, `completed_operations` | Bounded storage worker activity. |

Maintenance diagnostics retain failure counts and the latest failure for cleanup, checkpoint, log synchronization, and storage operations. A successful retry clears the corresponding current error. Capacity and malformed-command errors do not mark storage unhealthy. Lifecycle, connection failures, maintenance failures, and replication events use JSON log records.

Follower readiness confirms successful snapshot publication and an open replication session. It does **not** prove zero lag or compare leader/follower byte offsets. An idle follower's old `last_apply_unix_secs` is not itself a lag measurement. Disconnected followers continue serving their previous data while reporting `ready=false`.

## Persistence and memory

The disk backend loads its complete dataset into RAM. The former read-through cache duplicated these payloads without avoiding disk reads, so it is no longer used. `cache_max_entries` is accepted for compatibility; cache counters remain zero.

One process may open a disk data directory at a time. An OS lock on `.keyz.lock` is held until storage closes and is released automatically after a crash. Keep this file in place; deleting it while an owner is running can defeat locking on Unix. Use a local filesystem that supports exclusive file locks.

Startup and `analyze` reject inconsistent partition lengths/counts, invalid ranges, overlaps, and unreferenced snapshot bytes. Failed startup preserves the recovery sources. Version 1 stores each partition's encoded length and entry count in 32 bits; checkpoints reject lengths above 4 GiB minus one byte before replacing the snapshot or retiring the log. The encoded length includes encryption overhead when enabled.

The disk backend also rejects duplicate or misplaced snapshot keys. A nonempty AOF with no complete recoverable record fails startup and is preserved: the format cannot reliably distinguish a torn first write from incompatible encryption. Resolve the recovery error before restarting against the original files.

Three independent maintenance schedules apply:

- `store.cleanup_interval_ms`: remove expired entries.
- `store.disk.flush_interval_ms`: checkpoint dirty partitions into a full snapshot.
- `store.disk.aof_fsync = "every_second"`: synchronize the append-only log approximately once per second.

A checkpoint holds the operation lock while capturing and persisting the snapshot, durably replacing the old file, and retiring the log. This can increase request latency; the work stays off Tokio's runtime threads. Shutdown performs a final explicit checkpoint.

`aof_fsync="every_write"` synchronizes the recovery log before acknowledging each mutation. `every_second` permits recent acknowledged writes to be lost after an OS or power failure; the interval is a scheduling target, not a strict upper bound when storage is stalled. Guarantees depend on the filesystem and device honoring synchronization. With AOF disabled, only completed checkpoints survive force termination. Full snapshot transfer temporarily retains both old and new datasets. Log rotation and incremental/background checkpoint architecture remain future work.

A failed append or per-write synchronization rolls back to the last accepted log boundary and synchronizes that rollback. If rollback cannot be made durable, or periodic synchronization fails, new writes are rejected until a successful checkpoint restores a safe log. Check health diagnostics for storage errors.

## Resource limits

```toml
[server]
max_connections = 1024
work_queue_capacity = 1024
write_timeout_secs = 30
shutdown_timeout_secs = 30

[store]
max_bytes = 268435456 # 256 MiB; default 0 is unlimited
```

`max_bytes` counts key UTF-8 bytes plus stored payload bytes **after compression**, including expired entries until removed. Overwrites account for the replaced entry. Over-budget writes and snapshot replacements leave the existing dataset intact. The budget excludes allocator/hash-map overhead, queued request bodies, decompressed responses, and snapshot copies; it is not a process RSS limit. Set connection, queue, and frame limits together for the workload.

The work queue permits `work_queue_capacity` waiting operations plus one running operation. Client queue saturation returns an `overloaded` error immediately. Internal replication work waits fairly for capacity and can cancel admission during shutdown; transient queue pressure does not force a full resync. A full connection limit closes newly accepted sockets before processing requests; client and replication listeners enforce independent limits. Each connection has one outstanding exchange at a time. Both requests and responses are bounded by `protocol.max_message_bytes`; an oversized result returns `response_too_large` when that error fits in the configured frame limit.

Replication records are limited to 64 MiB, including 22 bytes of SET metadata. Leader configuration reserves that overhead within the client frame limit. Oversized leader writes are rejected before commit, and enabling leader mode validates existing entries at startup (temporarily taking one additional snapshot copy).

An idle live replication connection may wait for updates indefinitely. Once any frame byte arrives, `server.write_timeout_secs` bounds receipt of its remaining header and payload together. A stalled partial frame disconnects the session and clears follower readiness before reconnecting.

The CLI's `--response-timeout` bounds request socket I/O with one deadline across authentication, framing, TLS records, and the optional safe read retry. Progress does not reset it. Connection establishment also respects `--connect-timeout`. Blocking system DNS lookup and certificate-file reads are outside socket I/O cancellation; use literal addresses if DNS latency must be avoided. A timed-out connection is discarded so late replies cannot contaminate the next request. Mutations are never retried automatically.

## Authentication and TLS

Existing local installations retain plaintext/no-auth defaults. For network deployments, configure TLS and tokens together. Authentication alone does not encrypt token or payload traffic.

```toml
[security]
# Prefer environment variables instead of placing secrets in this file:
# KEYZ_AUTH_TOKEN and KEYZ_REPLICATION_TOKEN

[security.tls]
enabled = true
cert_path = "/etc/keyz/server.pem"
key_path = "/etc/keyz/server-key.pem"
ca_path = "/etc/keyz/ca.pem"
server_name = "keyz.example.com"
```

Use a PEM certificate chain and corresponding private key from your certificate authority. The server certificate must cover the DNS name or IP used by clients. TLS clients require `ca_path`; TLS servers require `cert_path` and `key_path`. Followers act as both server and client and need all three. The CLI can use a separate config with just TLS enabled, CA path, and optionally the verified server name.

`server_name` overrides the certificate name checked on **outgoing** connections. Omit it to verify the address hostname; use it when connecting by IP to a certificate issued for a DNS name. Cluster discovery returns addresses, so configure appropriate IP SANs or an override for those probes. Certificate verification cannot be disabled, and failed TLS connections never fall back to plaintext. Certificate changes require restarting the process.

Set `KEYZ_AUTH_TOKEN` to require authentication on every client connection, including health requests. Set `KEYZ_REPLICATION_TOKEN` for a separate replication credential; if absent, replication uses the client token. These environment values override file settings. Tokens are 1–4096 bytes and are redacted from debug configuration output. The shared client and CLI authenticate each new connection automatically. Manual clients send a framed `AUTH <token>` and require `ok` before sending commands or the replication address handshake. Do not send `AUTH` when the server has authentication disabled.

TLS authenticates server certificates; client/follower identity is provided by the shared token, not mutual TLS or per-user authorization. Encryption at rest remains independent: configure `store.disk.encryption_enabled` and `KEYZ_ENCRYPTION_KEY` when needed.

Cluster discovery treats follower-advertised addresses as untrusted. `cluster status` and `cluster monitor` list them but skip connections by default, even when TLS or authentication is disabled. To authorize probes, configure exact numeric IP-and-port destinations in the **CLI's** configuration:

```toml
[security]
trusted_cluster_addresses = ["10.0.0.11:7667", "[2001:db8::12]:7667"]
```

This authorizes use of the CLI's client credentials at those endpoints. Hostnames and wildcards are not accepted, and trust is not inferred from the replication token or leader INFO response. Untrusted entries appear as `untrusted` in the table and `probe_skipped=true` in JSON. Explicit `--host`/`--port` connections continue to work normally. Library consumers should use `for_discovered_address` for server-provided endpoints; `for_address` is for explicitly selected trusted destinations.

## Verification

Windows and Linux CI run formatting, strict Clippy, unit tests, process recovery, TLS integration, and release builds. Process tests use isolated temporary directories and kill/restart real binaries. The Linux-only SIGTERM test verifies final persistence with AOF and periodic checkpoints disabled; cross-platform tests exercise the same shutdown worker and drain paths directly.

Storage regressions inject partial appends and synchronization/rollback failures for plaintext and encrypted logs, verify rejection of concurrent directory owners, and exercise format limits without allocating a 4 GiB partition. Client tests cover trickled frames, buffered TLS data after a deadline, safe connection reuse, strict CLOSE handling, and discovery trust. Replication tests cover partial-frame timeouts, queue saturation, cancellation and draining.

Normal tests run 12,000 deterministic parser mutations. Coverage-guided fuzzing runs in a separate Linux workflow when parser files change or on manual dispatch:

```sh
cargo +nightly install cargo-fuzz --locked
cargo +nightly fuzz run protocol -- -max_total_time=60 -max_len=65536
cargo +nightly fuzz run storage -- -max_total_time=60 -max_len=65536
```

See [benchmarks](benchmarks.md) for repeatable measurements. Performance results are workload and hardware dependent; no throughput or latency threshold is asserted by CI.
