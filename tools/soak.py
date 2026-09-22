#!/usr/bin/env python3
"""Bounded local load and fault checks. No third-party Python dependencies."""
import argparse
import collections
import csv
import json
import hashlib
import platform
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
import urllib.error
from urllib.parse import urljoin

ROOT = Path(__file__).resolve().parents[1]


def duration(value):
    match = re.fullmatch(r"([1-9][0-9]*)([smh]?)", value)
    if not match:
        raise argparse.ArgumentTypeError("use positive seconds, minutes, or hours")
    return int(match[1]) * {"": 1, "s": 1, "m": 60, "h": 3600}[match[2]]


def positive(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def free_port(kind=socket.SOCK_STREAM):
    with socket.socket(type=kind) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def open_url(url, timeout):
    try:
        return urllib.request.urlopen(url, timeout=timeout)
    except urllib.error.HTTPError as error:
        error.close()
        raise


def metrics(text):
    return {line.split()[0]: float(line.split()[1]) for line in text.splitlines()
            if line and not line.startswith("#") and "{" not in line.split()[0]}


def playlist(text, base):
    """Keep GAP resources unavailable; return children and available segments."""
    children, media = [], []
    master = "#EXT-X-STREAM-INF:" in text
    gap = False
    for line in text.splitlines():
        if line.startswith("#EXT-X-MEDIA:") or line.startswith("#EXT-X-MAP:"):
            match = re.search(r'URI="([^"]+)"', line)
            if match:
                (children if line.startswith("#EXT-X-MEDIA:") else media).append(urljoin(base, match[1]))
        elif line == "#EXT-X-GAP":
            gap = True
        elif line and not line.startswith("#"):
            if not gap:
                (children if master else media).append(urljoin(base, line))
            gap = False
    return children, media


class Viewer:
    def __init__(self, url, stop, slow=False):
        self.url, self.stop, self.slow = url, stop, slow
        self.lock = threading.Lock()
        self.state = dict(media=0, bytes=0, errors=0, last_media=time.monotonic(),
                          playlist_max=0.0, media_max=0.0, renditions={})
        self.seen = collections.deque(maxlen=256)
        self.thread = threading.Thread(target=self.run, daemon=True)

    def get(self, url, media=False):
        started = time.monotonic()
        data = bytearray()
        with open_url(url, timeout=5) as response:
            while not self.stop.is_set():
                block = response.read(8192)
                if not block:
                    break
                data.extend(block)
                if len(data) > 8 * 1024 * 1024:
                    raise ValueError("unexpectedly large response")
                if media and self.slow:
                    self.stop.wait(len(block) / 32768)
        if self.stop.is_set():
            return b""
        with self.lock:
            key = "media_max" if media else "playlist_max"
            self.state[key] = max(self.state[key], time.monotonic() - started)
            if media and "/segment/" in url:
                if not data:
                    raise ValueError("empty media response")
                self.state["media"] += 1
                self.state["bytes"] += len(data)
                self.state["last_media"] = time.monotonic()
                self.state["renditions"][url.split("/segment/", 1)[0]] = time.monotonic()
        return bytes(data)

    def run(self):
        while not self.stop.is_set():
            try:
                children, _ = playlist(self.get(self.url).decode(), self.url)
                for child in dict.fromkeys(children):
                    _, resources = playlist(self.get(child).decode(), child)
                    # Fetch the current complete segment and its initialization.
                    # A slow viewer follows the live edge rather than growing a backlog.
                    for resource in resources[:1] + resources[-1:]:
                        if resource not in self.seen:
                            self.get(resource, media=True)
                            self.seen.append(resource)
            except Exception as error:
                if not self.stop.is_set():
                    with self.lock:
                        self.state["errors"] += 1
                        self.state["last_error"] = str(error)
            self.stop.wait(0.5)

    def snapshot(self):
        with self.lock:
            return {**self.state, "renditions": dict(self.state["renditions"])}


class Run:
    def __init__(self, args):
        self.args = args
        self.out = Path(args.output_dir or tempfile.mkdtemp(prefix="rushls-soak-")).resolve()
        self.out.mkdir(parents=True, exist_ok=True)
        if any(self.out.iterdir()):
            raise ValueError("output directory must be empty")
        self.stop = threading.Event()
        self.children, self.viewers, self.workers = [], [], []
        self.fault_errors = []
        self.fault_counts = collections.Counter()
        self.ports = dict(rtmp=free_port(), srt=free_port(socket.SOCK_DGRAM),
                          http=free_port(), metrics=free_port())
        self.base = f'http://127.0.0.1:{self.ports["http"]}'
        self.archive = self.out / "archive"
        self.blocked = self.archive / "live"
        self.saved = self.out / "archive-live.saved"
        self.samples = collections.deque(maxlen=1024)
        self.rss_peak = self.cpu_peak = 0.0
        self.environment = {k: v for k, v in os.environ.items()
                            if not k.startswith("RUSHLS_")}

    def spawn(self, command, name):
        with (self.out / f"{name}.log").open("ab") as log:
            process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT,
                                       env=self.environment, start_new_session=True)
        self.children.append(process)
        return process

    def publisher(self, name):
        return self.spawn(["ffmpeg", "-hide_banner", "-loglevel", "error", "-re",
            "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30", "-re", "-f", "lavfi",
            "-i", "sine=frequency=1000:sample_rate=48000", "-c:v", "libx264",
            "-preset", "ultrafast", "-tune", "zerolatency", "-pix_fmt", "yuv420p",
            "-g", "60", "-keyint_min", "60", "-sc_threshold", "0", "-bf", "0",
            "-b:v", "300k", "-c:a", "aac", "-b:a", "64k", "-f", "flv",
            f'rtmp://127.0.0.1:{self.ports["rtmp"]}/live/{name}'], name)

    def halt(self, process, abrupt=False):
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL if abrupt else signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        if process in self.children:
            self.children.remove(process)

    def get(self, path, metric=False):
        base = f'http://127.0.0.1:{self.ports["metrics"]}' if metric else self.base
        with open_url(base + path, timeout=3) as response:
            return response.read().decode()

    def measured(self):
        return metrics(self.get("/metrics", metric=True))

    def await_condition(self, check, timeout, description):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self.server.poll() is not None:
                raise RuntimeError("server exited; inspect rushls.log")
            try:
                if check():
                    return
            except (OSError, ValueError):
                pass
            time.sleep(0.2)
        raise AssertionError(description)

    def faults(self):
        try:
            # Fail only this run's recorder path. No host disk or network changes.
            self.stop.wait(5)
            if self.stop.is_set():
                return
            self.blocked.rename(self.saved)
            self.blocked.write_text("injected ENOTDIR\n")
            before = self.measured()["rushls_recording_segments_lost_total"]
            self.stop.wait(8)
            after = self.measured()["rushls_recording_segments_lost_total"]
            if after <= before:
                raise AssertionError("recorder fault did not cause a rejected write")
            self.fault_counts["recording_losses"] = int(after - before)
            self.restore_archive()
            before_files = sum(1 for _ in self.archive.rglob("*.mp4"))
            self.stop.wait(8)
            if sum(1 for _ in self.archive.rglob("*.mp4")) <= before_files:
                raise AssertionError("recording did not resume after restoration")
            self.fault_counts["recording_recoveries"] += 1
        except Exception as error:
            self.fault_errors.append(str(error))
        finally:
            self.restore_archive()

    def restore_archive(self):
        if self.saved.exists():
            if self.blocked.is_file():
                self.blocked.unlink()
            self.saved.rename(self.blocked)

    def hostile(self):
        try:
            while not self.stop.is_set():
                with socket.create_connection(("127.0.0.1", self.ports["rtmp"]), timeout=2) as peer:
                    peer.sendall(b"\xff" * 4096)
                self.fault_counts["malformed_connections"] += 1
                self.stop.wait(0.1)
        except OSError as error:
            if not self.stop.is_set():
                self.fault_errors.append(f"hostile client: {error}")

    def churn(self):
        try:
            while not self.stop.is_set():
                admitted_before = self.measured()["rushls_sessions_started_total"]
                peers = [self.publisher(f"churn-{index}") for index in range(2)]
                self.stop.wait(7)
                if not self.stop.is_set():
                    assert all(peer.poll() is None for peer in peers), "reconnect publisher exited"
                    assert self.measured()["rushls_sessions_started_total"] >= admitted_before + 2, "reconnect was not admitted"
                    for index in range(2):
                        self.get(f"/live/churn-{index}/index.m3u8")
                        self.fault_counts["reconnect_publications"] += 1
                for peer in peers:
                    self.halt(peer, abrupt=True)
        except Exception as error:
            if not self.stop.is_set():
                self.fault_errors.append(f"reconnect: {error}")

    def execute(self):
        args = self.args
        binary = Path(args.binary).resolve()
        with binary.open("rb") as executable:
            binary_hash = hashlib.file_digest(executable, "sha256").hexdigest()
        config = self.out / "rushls.toml"
        config.write_text(f'''[rtmp]
listen = "127.0.0.1:{self.ports['rtmp']}"
[srt]
listen = "127.0.0.1:{self.ports['srt']}"
[http]
listen = "127.0.0.1:{self.ports['http']}"
[metrics]
listen = "127.0.0.1:{self.ports['metrics']}"
[hls]
segment = {{ target = "2s", max = "2x" }}
part = {{ target = "500ms", max = "2x" }}
retain = "{30 if args.spill_pressure else 12}s"
[accept]
takeover = true
[capacity]
publishers = {args.publishers + 4}
streams = {args.publishers + 4}
memory_per_stream = "{1 if args.spill_pressure else 4}MiB"
disk_per_stream = "16MiB"
dir = {json.dumps(str(self.out / 'dvr'))}
[record]
dir = {json.dumps(str(self.archive))}
''')
        self.environment["RUSHLS_CONFIG"] = str(config)
        self.environment["RUST_LOG"] = "info"
        self.server = self.spawn([str(binary)], "rushls")
        self.await_condition(lambda: self.get("/health/ready"), 30, "server readiness timeout")
        healthy = [self.publisher(f"soak-{i}") for i in range(args.publishers)]
        for i in range(args.publishers):
            self.await_condition(lambda i=i: self.get(f"/live/soak-{i}/index.m3u8"),
                                 30, f"publisher {i} startup timeout")
        for i in range(args.viewers + args.slow_viewers):
            viewer = Viewer(f"{self.base}/live/soak-{i % args.publishers}/index.m3u8",
                            self.stop, i >= args.viewers)
            self.viewers.append(viewer)
            viewer.thread.start()
        # Wait for actual segment downloads, not merely successful manifest requests.
        self.await_condition(lambda: all(len(v.snapshot()["renditions"]) == 2 for v in self.viewers),
                             30, "viewers did not fetch media")
        baseline = self.measured()
        for key in ("rushls_allocator_calls_total", "rushls_allocator_allocated_bytes_total",
                    "rushls_allocator_freed_bytes_total"):
            if baseline.get(key, 0) <= 0:
                raise AssertionError("build with --features allocation-counting")
        threads = self.workers
        if args.faults:
            for task in (self.faults, self.hostile, self.churn):
                thread = threading.Thread(target=task, daemon=True)
                threads.append(thread)
                thread.start()
        start = time.monotonic()
        with (self.out / "samples.csv").open("w") as output:
            writer = None
            while time.monotonic() - start < args.duration:
                measured = self.measured()
                rss, cpu = subprocess.check_output(
                    ["ps", "-o", "rss=,%cpu=", "-p", str(self.server.pid)], text=True).split()
                now = time.monotonic()
                states = [v.snapshot() for v in self.viewers]
                row = dict(elapsed=now-start, rss_mib=int(rss)/1024, cpu_percent=float(cpu),
                           media=sum(s["media"] for s in states),
                           errors=sum(s["errors"] for s in states),
                           playlist_max=max(s["playlist_max"] for s in states),
                           media_max=max(s["media_max"] for v, s in zip(self.viewers, states) if not v.slow))
                row.update(measured)
                self.samples.append(row)
                self.rss_peak = max(self.rss_peak, row["rss_mib"])
                self.cpu_peak = max(self.cpu_peak, row["cpu_percent"])
                if writer is None:
                    writer = csv.DictWriter(output, fieldnames=list(row))
                    writer.writeheader()
                writer.writerow(row)
                output.flush()
                assert all(p.poll() is None for p in healthy), "healthy publisher exited"
                assert not self.fault_errors, self.fault_errors
                assert row["errors"] == 0, states
                assert measured["rushls_http_connections"] <= measured["rushls_http_connection_capacity"], "connection capacity exceeded"
                assert measured["rushls_disk_spill_pending"] <= measured["rushls_disk_spill_capacity"], "spill queue exceeded"
                assert measured["rushls_retained_memory_bytes"] <= (args.publishers + 2) * 4 * 1024 * 1024, "retained memory ceiling exceeded"
                assert measured["rushls_retained_disk_bytes"] <= (args.publishers + 2) * 16 * 1024 * 1024, "retained disk ceiling exceeded"
                assert measured["rushls_disk_spills_failed_total"] == 0, "unexpected DVR failure"
                assert row["rss_mib"] <= args.max_rss_mib, "RSS ceiling exceeded"
                assert row["cpu_percent"] <= args.max_cpu_percent, "CPU ceiling exceeded"
                assert row["playlist_max"] <= args.max_latency, "playlist latency ceiling exceeded"
                assert row["media_max"] <= args.max_latency, "normal media latency ceiling exceeded"
                assert all(now-last <= 20 for s in states for last in s["renditions"].values()), "audio or video stopped advancing"
                assert measured["rushls_sessions_failed_total"] == baseline["rushls_sessions_failed_total"], "unexpected session failure"
                time.sleep(args.sample_interval)
        if args.spill_pressure:
            assert any(s["rushls_retained_disk_bytes"] > 0 for s in self.samples), "DVR pressure did not exercise spilling"
        self.stop.set()
        for thread in threads:
            thread.join(timeout=12)
            assert not thread.is_alive(), "fault worker did not stop"
        assert not self.fault_errors, self.fault_errors
        for publisher in healthy:
            self.halt(publisher)
        self.await_condition(lambda: self.measured()["rushls_active_sessions"] == 0,
                             20, "publisher leases did not release")
        self.await_condition(lambda: self.measured()["rushls_idle_streams"] == 0,
                             45, "retained streams did not expire")
        final = self.measured()
        (self.out / "final-metrics.json").write_text(json.dumps(final, indent=2))
        assert final["rushls_published_streams"] == 0, "publication leak"
        assert final["rushls_retained_memory_bytes"] == 0, "retained memory did not release"
        assert final["rushls_retained_disk_bytes"] == 0, "retained disk did not release"
        assert final["rushls_disk_spill_pending"] == 0, "spill jobs did not drain"
        if args.faults:
            assert self.fault_counts["recording_recoveries"] == 1
            assert self.fault_counts["reconnect_publications"] >= 4
            assert self.fault_counts["malformed_connections"] >= 10
        stable = list(self.samples)[len(self.samples)//2:]
        growth = stable[-1]["rss_mib"] - stable[0]["rss_mib"]
        heap = lambda sample: sample["rushls_allocator_allocated_bytes_total"] - sample["rushls_allocator_freed_bytes_total"]
        heap_growth = (heap(stable[-1]) - heap(stable[0])) / (1024 * 1024)
        assert heap_growth <= args.max_growth_mib, "requested heap growth exceeded"
        assert heap(final) <= heap(baseline) + args.max_growth_mib * 1024 * 1024, "heap did not settle after cleanup"
        assert growth <= args.max_growth_mib, "steady-state RSS growth exceeded"
        return dict(status="passed", elapsed=time.monotonic()-start,
                    host=dict(platform=platform.platform(), logical_cpus=os.cpu_count()),
                    binary_sha256=binary_hash,
                    configuration=vars(args), faults=dict(self.fault_counts),
                    rss_peak_mib=self.rss_peak,
                    rss_growth_mib=growth, requested_heap_growth_mib=heap_growth,
                    cpu_peak_percent=self.cpu_peak,
                    viewers=[v.snapshot() for v in self.viewers],
                    allocation_calls=self.samples[-1]["rushls_allocator_calls_total"]-baseline["rushls_allocator_calls_total"],
                    allocated_bytes=self.samples[-1]["rushls_allocator_allocated_bytes_total"]-baseline["rushls_allocator_allocated_bytes_total"])

    def close(self):
        self.stop.set()
        for viewer in self.viewers:
            viewer.thread.join(timeout=6)
        for worker in self.workers:
            worker.join(timeout=12)
        for child in list(reversed(self.children)):
            self.halt(child)
        self.restore_archive()


def main():
    if not __debug__:
        raise RuntimeError("do not disable assertions for a validation harness")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--duration", type=duration, default=3600)
    parser.add_argument("--publishers", type=positive, default=5)
    parser.add_argument("--viewers", type=positive, default=5)
    parser.add_argument("--slow-viewers", type=positive, default=1)
    parser.add_argument("--sample-interval", type=positive, default=5)
    parser.add_argument("--max-cpu-percent", type=positive, default=400)
    parser.add_argument("--max-rss-mib", type=positive, default=512)
    parser.add_argument("--max-growth-mib", type=positive, default=32)
    parser.add_argument("--max-latency", type=positive, default=2)
    parser.add_argument("--faults", action="store_true")
    parser.add_argument("--spill-pressure", action="store_true")
    parser.add_argument("--output-dir")
    parser.add_argument("--binary", default=str(ROOT / "target/debug/rushls"))
    args = parser.parse_args()
    if args.duration < 30 or args.duration < 2 * args.sample_interval:
        parser.error("use at least 30s and two sample intervals")
    if args.viewers < args.publishers:
        parser.error("use at least one normal viewer per publisher")
    run = Run(args)
    result = dict(status="failed")
    print(f"artifacts: {run.out}", flush=True)
    try:
        result = run.execute()
    except Exception as error:
        result.update(error=str(error), faults=dict(run.fault_counts))
        raise
    finally:
        run.close()
        (run.out / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
