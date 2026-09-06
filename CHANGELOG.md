# Changelog

## Unreleased

### Added

- Shared Rust library and persistent TCP/TLS client used by the server and CLI.
- Versioned `KEYZ/1` requests with exact binary values, numeric request IDs,
  distinct missing/value/error/TTL results, and strict command schemas. Legacy
  text clients continue to work.
- Token authentication and verified TLS for client and replication connections,
  plus explicit trusted IP-and-port destinations for CLI cluster discovery.
- Bounded connection and storage admission, storage byte budgets, graceful
  shutdown, readiness and maintenance diagnostics, and structured lifecycle events.
- Leader/follower replication with atomic full snapshot replacement, ordered live
  updates, reconnect handling and cluster diagnostics.
- Windows/Linux CI, process recovery and TLS integration tests, deterministic
  parser mutations, coverage-guided fuzz targets and isolated benchmark workloads.
- A container image containing both release binaries, a non-root runtime user,
  readiness healthcheck and SIGTERM shutdown handling.

### Changed

- Rust 1.89 or newer is required for standard-library file locking.
- Disk partitions remain fully resident in memory. The duplicate read-through
  cache is unused; `cache_max_entries` remains a compatibility setting with no effect.
- Expiration cleanup, checkpoints and periodic log synchronization run on
  independent schedules. Explicit shutdown drains accepted work and flushes storage.
- Internal replication jobs wait fairly for bounded storage capacity; client
  overload still returns an immediate error. Accepted work survives client cancellation.
- The client applies one socket-I/O deadline across authentication, requests,
  responses, TLS buffering and the optional safe read retry. Failed exchanges
  discard their connection; mutations are never retried automatically.
- Cluster status and monitor list advertised followers but probe only destinations
  in `security.trusted_cluster_addresses`. Skipped entries report `untrusted` or
  `probe_skipped=true`. Explicit host/port selection remains available.

### Fixed

- Concurrent disk owners could overwrite or retire another process's acknowledged
  writes. A lifetime-held OS directory lock now rejects competing owners and aliases.
- Partial AOF writes could corrupt the recovery boundary and hide later writes.
  Appends and optional per-write synchronization now roll back durably on failure;
  failed rollback, periodic sync or log retirement blocks writes until checkpoint recovery.
- Oversized snapshot partitions could wrap 32-bit lengths. Checked serialization
  rejects unrepresentable entries, counts and encrypted partition sizes before
  replacing the snapshot or retiring recovery data.
- Invalid snapshot offsets, counts, overlaps and missing ranges could panic or
  silently omit keys. Shared validators reject inconsistent metadata; startup
  preserves recovery files and the offline analyzer reports errors safely.
- Snapshot/log publication and replacement now preserve crash-recovery ordering,
  including pending full follower replacements and failed encryption validation.
- Stalled partial replication frames now clear readiness and reconnect after the
  configured deadline; idle complete streams remain connected.
- Malformed typed CLOSE commands now return an error and keep the connection
  usable. Valid CLOSE uses the same validated command for its reply and shutdown.
- Slow frame progress and already-buffered TLS plaintext can no longer extend a
  client request beyond its deadline.
- Server-advertised addresses can no longer cause automatic client credential
  forwarding without independently configured destination trust.
- TTL overflow, expired-entry races, storage byte accounting, response limits,
  typed reply correlation and batch failure handling have regression coverage.

### Compatibility and verification

Typed TTL omission/null uses the configured default, while typed `ttl_secs=0`
means no expiration. Legacy `SET ... EX 0` retains its default-TTL behavior.
Recovery guarantees still depend on the selected AOF policy and filesystem;
blocking DNS and certificate-file reads are outside socket-I/O cancellation.
See [operations](docs/operations.md) and [protocol v1](docs/protocol-v1.md).

The completed implementation passed 209 tests on Windows, formatting, strict
all-target Clippy, compilation of both fuzz targets and both release binaries
on 2026-09-06. Linux/container CI is configured but was not run locally for this
verification. Physical disk-full and power-loss events were not induced; storage
failure paths use deterministic fault injection. A Redis comparison and the
proposed performance redesign remain future work.
