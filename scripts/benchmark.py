#!/usr/bin/env python3
"""Repeatable local Keyz load/checkpoint/resync benchmark, using only the stdlib."""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field


def free_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def receive_exact(stream: socket.socket, length: int) -> bytes:
    result = bytearray()
    while len(result) < length:
        chunk = stream.recv(length - len(result))
        if not chunk:
            raise ConnectionError("server closed the connection")
        result.extend(chunk)
    return bytes(result)


def exchange(stream: socket.socket, command: bytes, max_frame: int) -> bytes:
    stream.sendall(struct.pack("!I", len(command)) + command)
    length = struct.unpack("!I", receive_exact(stream, 4))[0]
    if length == 0 or length > max_frame:
        raise ValueError(f"invalid response length: {length}")
    return receive_exact(stream, length)


def connect(port: int, timeout: float = 5) -> socket.socket:
    stream = socket.create_connection(("127.0.0.1", port), timeout)
    stream.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return stream


def query(port: int, command: bytes, max_frame: int) -> dict:
    with connect(port) as stream:
        return json.loads(exchange(stream, command, max_frame))


def wait_ready(server: "Server", max_frame: int, timeout: float = 30) -> dict:
    deadline = time.monotonic() + timeout
    last = "starting"
    while time.monotonic() < deadline:
        if server.process.poll() is not None:
            raise RuntimeError(f"{server.name} exited: {server.log_text()}")
        try:
            health = query(server.port, b"HEALTH", max_frame)
            if health.get("ready") is True:
                return health
            last = str(health)
        except (OSError, ValueError, ConnectionError) as error:
            last = str(error)
        time.sleep(0.025)
    raise TimeoutError(f"{server.name} did not become ready: {last}")


def rss_bytes(pid: int) -> tuple[int | None, int | None]:
    """Return current and process-lifetime peak resident bytes, when supported."""
    try:
        if sys.platform == "win32":
            from ctypes import wintypes

            class Counters(ctypes.Structure):
                _fields_ = [("cb", wintypes.DWORD), ("faults", wintypes.DWORD)] + [
                    (name, ctypes.c_size_t) for name in (
                        "peak_working", "working", "peak_paged", "paged",
                        "peak_nonpaged", "nonpaged", "pagefile", "peak_pagefile")
                ]

            kernel = ctypes.WinDLL("kernel32", use_last_error=True)
            psapi = ctypes.WinDLL("psapi", use_last_error=True)
            kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
            kernel.OpenProcess.restype = wintypes.HANDLE
            kernel.CloseHandle.argtypes = [wintypes.HANDLE]
            psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
            handle = kernel.OpenProcess(0x1000 | 0x0010, False, pid)
            if not handle:
                return None, None
            try:
                counters = Counters()
                counters.cb = ctypes.sizeof(counters)
                if psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
                    return counters.working, counters.peak_working
            finally:
                kernel.CloseHandle(handle)
        elif sys.platform.startswith("linux"):
            values = {}
            for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                if line.startswith(("VmRSS:", "VmHWM:")):
                    key, amount, _unit = line.split()
                    values[key] = int(amount) * 1024
            return values.get("VmRSS:"), values.get("VmHWM:")
    except (OSError, ValueError, ProcessLookupError):
        pass
    return None, None


@dataclass
class Server:
    name: str
    port: int
    process: subprocess.Popen
    log_path: Path
    log_file: object

    def log_text(self) -> str:
        self.log_file.flush()
        return self.log_path.read_text(errors="replace")[-4000:]

    def stop(self) -> None:
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)
        self.log_file.close()


@dataclass
class Harness:
    executable: Path
    directory: Path
    args: argparse.Namespace
    max_frame: int
    servers: list[Server] = field(default_factory=list)
    lock: threading.Lock = field(default_factory=threading.Lock)
    peaks: dict = field(default_factory=dict)

    def start(self, name: str, port: int, role: str = "disabled", replication_port: int = 0) -> Server:
        disk = self.args.mode != "memory"
        data = self.directory / f"{name}-data"
        config = self.directory / f"{name}.toml"
        config.write_text(
            f'[server]\nhost="127.0.0.1"\nport={port}\n'
            f'max_connections={max(1024, self.args.clients + 8)}\n'
            f'work_queue_capacity={self.args.queue_capacity}\n'
            f'[protocol]\nmax_message_bytes={self.max_frame}\nidle_timeout_secs=120\n'
            f'[store]\nbackend="{"disk" if disk else "memory"}"\n'
            f'compression_threshold={self.args.compression_threshold}\ncleanup_interval_ms=250\n'
            f'[store.disk]\ndata_dir={json.dumps(str(data))}\n'
            f'cache_max_entries={self.args.cache_entries}\nflush_interval_ms={self.args.flush_interval_ms}\n'
            f'aof_enabled=true\naof_fsync="{self.args.fsync}"\n'
            f'[replication]\nrole="{role}"\n'
            + (f'bind="127.0.0.1:{replication_port}"\n' if role == "leader" else "")
            + (f'leader_address="127.0.0.1:{replication_port}"\n' if role == "follower" else ""),
            encoding="utf-8",
        )
        environment = os.environ.copy()
        # An isolated benchmark must not accidentally inherit credentials or keys.
        for key in ("KEYZ_AUTH_TOKEN", "KEYZ_REPLICATION_TOKEN", "KEYZ_ENCRYPTION_KEY"):
            environment.pop(key, None)
        environment["KEYZ_CONFIG"] = str(config)
        log_path = self.directory / f"{name}-{len(self.servers)}.log"
        log_file = log_path.open("wb")
        try:
            process = subprocess.Popen(
                [str(self.executable)], cwd=self.directory, env=environment,
                stdin=subprocess.DEVNULL, stdout=log_file, stderr=subprocess.STDOUT,
                creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
            )
        except BaseException:
            log_file.close()
            raise
        server = Server(name, port, process, log_path, log_file)
        with self.lock:
            self.servers.append(server)
        return server

    def sample(self) -> None:
        with self.lock:
            servers = self.servers.copy()
        for server in servers:
            if server.process.poll() is not None:
                continue
            current, peak = rss_bytes(server.process.pid)
            with self.lock:
                stats = self.peaks.setdefault(server.name, {"sampled_peak_rss_bytes": None, "process_peak_rss_bytes": None})
                for key, value in (("sampled_peak_rss_bytes", current), ("process_peak_rss_bytes", peak)):
                    if value is not None:
                        stats[key] = max(stats[key] or 0, value)

    def cleanup(self) -> None:
        with self.lock:
            servers = self.servers.copy()
        for server in reversed(servers):
            server.stop()


def percentile(values: list[float], percent: float) -> float | None:
    if not values:
        return None
    return values[max(0, math.ceil(percent / 100 * len(values)) - 1)] * 1000


def benchmark(args: argparse.Namespace) -> dict:
    executable = Path(args.executable).resolve()
    if not executable.is_file():
        raise FileNotFoundError(f"server executable missing: {executable}; run cargo build --release first")
    max_frame = max(4 * 1024 * 1024, args.value_size + 4096)
    randomizer = random.Random(args.seed)
    alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-"
    payload = bytes(randomizer.choice(alphabet) for _ in range(args.value_size))
    keys = [f"bench:{number}".encode() for number in range(args.keys)]
    payload_digest = hashlib.sha256(payload).hexdigest()
    with tempfile.TemporaryDirectory(prefix="keyz-benchmark-") as temporary:
        harness = Harness(executable, Path(temporary), args, max_frame)
        sampler_stop = threading.Event()
        workload_stop = threading.Event()
        threads: list[threading.Thread] = []
        controller = None
        try:
            ports = set()
            while len(ports) < 3:
                ports.add(free_port())
            leader_port, follower_port, replication_port = sorted(ports)
            resync_mode = args.mode == "follower-resync"
            leader = harness.start("leader", leader_port, "leader" if resync_mode else "disabled", replication_port)
            wait_ready(leader, max_frame)
            with connect(leader_port) as stream:
                for key in keys:
                    if exchange(stream, b"SET " + key + b" " + payload, max_frame) != b"ok":
                        raise RuntimeError("preload write failed")
            follower = None
            if resync_mode:
                follower = harness.start("follower", follower_port, "follower", replication_port)
                wait_ready(follower, max_frame)
            baseline_info = query(leader_port, b"INFO", max_frame)
            harness.sample()

            def sample_memory() -> None:
                while not sampler_stop.wait(0.05):
                    harness.sample()

            sampler = threading.Thread(target=sample_memory, name="rss-sampler", daemon=True)
            sampler.start()
            samples: list[list[float]] = [[] for _ in range(args.clients)]
            error_counts = [0] * args.clients
            error_examples: list[str | None] = [None] * args.clients
            starts = threading.Barrier(args.clients + 1)
            begin_measurement = threading.Event()
            deadline = [0.0]

            def load(client_id: int) -> None:
                stream = None
                sequence = client_id
                try:
                    stream = connect(leader_port)
                except OSError as error:
                    error_counts[client_id] += 1
                    error_examples[client_id] = str(error)
                try:
                    starts.wait()
                    begin_measurement.wait()
                except threading.BrokenBarrierError:
                    if stream is not None:
                        stream.close()
                    return
                try:
                    while not workload_stop.is_set() and time.perf_counter() < deadline[0]:
                        key = keys[(sequence * args.clients + client_id) % len(keys)]
                        # Three reads per write; every write has a deterministic value.
                        write = sequence % 4 == 0
                        command = b"SET " + key + b" " + payload if write else b"GET " + key
                        started = time.perf_counter()
                        try:
                            if stream is None:
                                stream = connect(leader_port)
                            result = exchange(stream, command, max_frame)
                            expected = b"ok" if write else payload
                            if result != expected:
                                raise ValueError(f"unexpected {'SET' if write else 'GET'} response: {result[:120]!r}")
                            samples[client_id].append(time.perf_counter() - started)
                        except (OSError, ValueError, ConnectionError) as error:
                            error_counts[client_id] += 1
                            if error_examples[client_id] is None:
                                error_examples[client_id] = str(error)
                            if stream is not None:
                                stream.close()
                                stream = None
                            # Never replay a write whose response might have been lost.
                        sequence += 1
                finally:
                    if stream is not None:
                        stream.close()

            for client_id in range(args.clients):
                thread = threading.Thread(target=load, args=(client_id,), name=f"load-{client_id}")
                threads.append(thread)
                thread.start()
            starts.wait()
            started = time.perf_counter()
            deadline[0] = started + args.duration
            resync = {"requested": resync_mode}

            def restart_follower() -> None:
                nonlocal follower
                if workload_stop.wait(args.duration / 3):
                    return
                resync_started = time.perf_counter()
                resync["started_after_seconds"] = resync_started - started
                try:
                    assert follower is not None
                    harness.sample()
                    follower.stop()
                    follower = harness.start("follower", follower_port, "follower", replication_port)
                    health = wait_ready(follower, max_frame)
                    resync["seconds_to_ready"] = time.perf_counter() - resync_started
                    resync["completed_during_measurement"] = time.perf_counter() <= deadline[0]
                    resync["health"] = health
                except Exception as error:
                    resync["error"] = str(error)

            if resync_mode:
                controller = threading.Thread(target=restart_follower, name="follower-restart")
                controller.start()
            begin_measurement.set()
            for thread in threads:
                thread.join()
            elapsed = time.perf_counter() - started
            if controller is not None:
                controller.join()
            sampler_stop.set()
            sampler.join()
            harness.sample()
            latencies = sorted(latency for client_samples in samples for latency in client_samples)
            final_info = query(leader_port, b"INFO", max_frame)
            return {
                "mode": args.mode,
                "executable": str(executable),
                "platform": platform.platform(),
                "python": platform.python_version(),
                "settings": {key: getattr(args, key) for key in (
                    "clients", "keys", "value_size", "duration", "seed", "compression_threshold",
                    "flush_interval_ms", "fsync", "queue_capacity", "cache_entries")},
                "payload_sha256": payload_digest,
                "workload": "75% GET / 25% SET; deterministic shared key set; preload excluded",
                "elapsed_seconds": elapsed,
                "successful_operations": len(latencies),
                "failed_operations": sum(error_counts),
                "error_examples": [error for error in error_examples if error is not None],
                "operations_per_second": len(latencies) / elapsed,
                "latency_ms": {f"p{percent}": percentile(latencies, percent) for percent in (50, 95, 99)},
                "latency_scope": "successful operations, including reconnect when required",
                "memory": harness.peaks,
                "baseline_info": baseline_info,
                "final_info": final_info,
                "resync": resync,
            }
        finally:
            workload_stop.set()
            if "starts" in locals():
                starts.abort()
            if "begin_measurement" in locals():
                begin_measurement.set()
            for thread in threads:
                thread.join(timeout=10)
            if controller is not None:
                controller.join(timeout=45)
            sampler_stop.set()
            if "sampler" in locals():
                sampler.join(timeout=5)
            harness.cleanup()


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    default_binary = Path(__file__).resolve().parents[1] / "target" / "release" / ("keyz.exe" if os.name == "nt" else "keyz")
    parser.add_argument("--executable", default=str(default_binary), help="built server executable")
    parser.add_argument("--mode", choices=("memory", "disk-checkpoint", "follower-resync"), default="memory")
    parser.add_argument("--clients", type=int, default=4)
    parser.add_argument("--value-size", type=int, default=1024)
    parser.add_argument("--keys", type=int, default=1000)
    parser.add_argument("--duration", type=float, default=5)
    parser.add_argument("--seed", type=int, default=1953)
    parser.add_argument("--compression-threshold", type=int, default=512)
    parser.add_argument("--flush-interval-ms", type=int, default=250)
    parser.add_argument("--fsync", choices=("every_second", "every_write"), default="every_second")
    parser.add_argument("--queue-capacity", type=int, default=1024)
    parser.add_argument("--cache-entries", type=int, default=0)
    parser.add_argument("--output", type=Path, help="also write the JSON report to this path")
    args = parser.parse_args()
    for name in ("clients", "value_size", "keys", "duration", "compression_threshold", "flush_interval_ms", "queue_capacity"):
        value = getattr(args, name)
        if value <= 0 or not math.isfinite(value):
            parser.error(f"--{name.replace('_', '-')} must be finite and positive")
    if args.cache_entries < 0:
        parser.error("--cache-entries cannot be negative")
    if args.value_size + 4096 > 0xFFFFFFFF:
        parser.error("value size exceeds protocol framing capacity")
    return args


def main() -> int:
    args = arguments()
    try:
        result = benchmark(args)
        report = json.dumps(result, indent=2)
        if args.output:
            args.output.write_text(report + "\n", encoding="utf-8")
        print(report)
        return 1 if result["failed_operations"] or result["resync"].get("error") else 0
    except KeyboardInterrupt:
        print("benchmark interrupted; temporary servers stopped", file=sys.stderr)
        return 130
    except Exception as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
