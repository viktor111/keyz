# Repeatable local benchmarks

Build an optimized server, then run the standard-library Python 3.10+ harness:

```powershell
cargo build --release
python scripts/benchmark.py --mode memory --clients 4 --keys 10000 --value-size 1024 --duration 15
python scripts/benchmark.py --mode disk-checkpoint --clients 4 --keys 10000 --value-size 1024 --duration 15 --flush-interval-ms 250
python scripts/benchmark.py --mode follower-resync --clients 4 --keys 10000 --value-size 1024 --duration 15
```

The harness creates isolated temporary configurations, ports and data directories.
It starts its own server processes, without visible windows on Windows, and stops
them in `finally` cleanup. It does not connect to your configured server. On Unix,
cleanup first sends SIGTERM and waits; on Windows, cleanup terminates the temporary
process. This benchmark does not verify crash recovery or durable acknowledgments.

Each run preloads the dataset, then runs one persistent socket per client with
75% GET and 25% SET requests. The key set and payload are deterministic for the
chosen settings and seed. SET writes the same payload on every update; the server
still processes each write and marks disk state dirty. Reads validate their full
response. The workload uses the legacy protocol, allowing comparison with older
server builds that provide HEALTH/INFO. Preload and initial replication are
excluded from the operation latency and throughput measurements.

The modes exercise different paths:

| Mode | Servers | Activity during load |
| --- | --- | --- |
| `memory` | One memory server | Mixed reads and writes |
| `disk-checkpoint` | One disk server | Mixed traffic, AOF writes and periodic checkpoints |
| `follower-resync` | Disk leader and disk follower | Same traffic, with follower restart one-third into the run, forcing a full resync |

The resync report records the time from follower termination until HEALTH reports
ready. It separately states whether this completed within the measurement window.
If resync runs longer than the workload, the harness continues waiting for readiness
for up to 30 seconds after the follower restarts. This includes follower startup
and recovery time, not just network snapshot transfer time.

The JSON output includes:

- Successful throughput and successful-operation p50, p95 and p99 latencies.
- Failure counts and examples. Failed mutations are never automatically replayed.
- Server resident-memory peaks and baseline/final INFO metrics.
- Payload hash, platform, Python version and all workload settings.

`sampled_peak_rss_bytes` is the largest resident-memory sample observed from the
end of preload through the end of measurement/resync, sampled roughly every
50 ms. `process_peak_rss_bytes` is the OS peak
over the process lifetime, including startup and preload. On Windows these use
`GetProcessMemoryInfo` working-set counters; on Linux they use `/proc/<pid>/status`
VmRSS/VmHWM. Memory fields are null on unsupported platforms or when access fails.
Follower peaks take the maximum across the original and restarted follower, rather
than adding the processes together. Server RSS excludes the Python load generator.

Useful controls:

```powershell
# Smaller development smoke run (not a performance baseline).
python scripts/benchmark.py --executable target/debug/keyz.exe --clients 2 --keys 50 --duration 1

# Disable compression for a storage-only comparison by setting a threshold above value size.
python scripts/benchmark.py --mode disk-checkpoint --compression-threshold 1048576 --value-size 1024

# Compare per-write fsync with periodic fsync under the same workload.
python scripts/benchmark.py --mode disk-checkpoint --fsync every_write --duration 15

# Keep a report for comparison with a later commit.
python scripts/benchmark.py --output benchmark-before.json
```

`--cache-entries` retains the existing configuration control for comparison;
inspect INFO and the implementation when interpreting cache results. The current
disk backend keeps all entries in memory, so cache settings do not represent a
separate disk-read working set. `--queue-capacity` can expose admission failures
under load. A run exits unsuccessfully if requests fail or the resync fails.

Use the same executable profile, hardware, seed, key count, value size,
compression threshold, fsync policy and duration when comparing changes. Run each
case several times on an otherwise idle machine. Python thread scheduling, socket
overhead and the GIL can limit the load generator, especially for tiny memory-only
operations; these results describe this end-to-end workload, not the storage
engine's maximum capacity. Keep reports as evidence and avoid fixed CI performance
thresholds on shared runners. Ordinary CI verifies correctness separately.

## Redis comparison and planned performance work

This harness currently benchmarks Keyz only. There is no measured claim that
Keyz outperforms Redis, and multicore storage sharding, batched networking and
checkpoints concurrent with client reads/writes are not implemented. Client command execution
currently passes through one ordered storage worker.

Before comparing implementations, use a compiled load generator with equivalent
Keyz and Redis workloads. Match CPU allocation, payloads, key distributions,
connection counts, pipeline depths, TLS, persistence and acknowledgment policies.
Report successful operations, errors/timeouts, p50/p95/p99 latency, CPU and memory;
include scheduled-load measurements so pauses and queueing remain visible. Measure
single-core efficiency separately from total throughput under the same CPU budget.
See the [Redis benchmarking guidance](https://redis.io/docs/latest/operate/oss_and_stack/management/optimization/benchmarks/).

The proposed sequence is a comparison baseline, removal of redundant payload
copies, bounded batching, then measured changes to execution ownership and
persistence. Each step must preserve request ordering, memory limits, replication
consistency and durable acknowledgments.
