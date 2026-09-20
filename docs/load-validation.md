# Load and failure validation

The local harness exercises a real Rushls process through RTMP ingest and HTTP delivery.
It records measurements and fails on resource, latency, playback-progress, or cleanup violations.
It does not decode media or replace the browser and decoder regression suites.

## Run the harness

Run a sustained workload with an instrumented release build:

```sh
tools/soak.sh --duration 1h --publishers 5 --viewers 5 \
  --slow-viewers 1 --output-dir /tmp/rushls-soak-clean
```

Run concurrent failure scenarios:

```sh
tools/soak.sh --duration 5m --publishers 5 --viewers 5 \
  --faults --output-dir /tmp/rushls-soak-faults
```

Exercise DVR spill with smaller memory budgets:

```sh
tools/soak.sh --duration 5m --publishers 5 --viewers 5 \
  --spill-pressure --output-dir /tmp/rushls-soak-dvr
```

Use a new, empty output directory for each run. Ports are selected locally.
The harness clears inherited `RUSHLS_*` configuration and writes its own configuration.
The wrapper builds with `allocation-counting`. Ordinary production builds have no allocator instrumentation.
`--no-build` reuses the selected binary. `--debug` selects a debug build.
`RUSHLS_BIN` overrides the binary path. Run `--help` for measurement limits.

## Workload and assertions

The default workload has five healthy H.264/AAC publishers, five normal viewers, and one slow viewer.
Each publisher sends 320×180 video at 30 fps, with a requested video bitrate of 300 kbps.
Segments target two seconds. Parts target 500 ms.
Viewers fetch initialization resources and complete audio/video segments through HTTP.
They skip GAP resources and maintain a bounded history of fetched URIs.
The slow viewer reads media at 32 KiB/s and follows the current live edge.
This models slow downloads, not a player that downloads every segment or measures perceptual synchronization.

Each sample checks these limits:

- No healthy publisher exit or unexpected session failure.
- No viewer request error after startup.
- Continued completed-media downloads for every viewer within 20 seconds.
- Normal playlist and media request latency at most two seconds.
- Process RSS at most 512 MiB and sampled process CPU at most 400%.
- Connection and pending-spill counts within their advertised capacities.
- Retained memory at most 4 MiB per possible active stream and disk at most 16 MiB.
- No unexpected DVR write failure.

The final checks bound RSS and requested-heap growth to 32 MiB across the second half of retained samples.
The harness keeps at most 1,024 samples in memory. The CSV retains all samples.
Runs longer than that window use its second half for the growth check.
Each audio and video rendition must advance independently. Audio progress cannot hide stalled video.
After publisher shutdown, all session leases, retained streams, retained bytes, and pending spill jobs must disappear.
Requested heap usage must return within 32 MiB of its warm baseline.
These limits are configurable test acceptance criteria, not promises for arbitrary production workloads.

## Failure scenarios

`--faults` adds two publishers that reconnect concurrently after abrupt process termination.
A separate client sends malformed RTMP handshake input at approximately ten connections per second.
Healthy publishers and their viewers continue throughout these scenarios.

The recorder fault replaces this run's `archive/live` directory with a regular file for eight seconds.
Writes then fail with a filesystem error. The harness requires the loss counter to increase.
It restores the directory and requires new archive files to appear.
Only the harness-owned archive changes. No host filesystem fills, permissions, or network routes change.
The DVR directory stays separate, so failed recording must not interrupt live delivery.

`--spill-pressure` reduces each stream's memory budget to 1 MiB and retains 30 seconds of media.
The run must actually spill bytes to disk. Viewers continue to fetch media during the run.
This scenario exercises storage pressure, not a simulated stalled or full disk.
Existing unit tests separately cover failed spill writes, stalled export queues, and bounded disk reads.

## Measurements and interpretation

Each run writes `samples.csv`, `result.json`, final metrics, and server/publisher logs.
A failed assertion produces a failed result and a nonzero exit status.
Child processes are terminated and reaped on exit. Fault paths restore the archive directory.

CPU comes from `ps %cpu`, whose averaging behavior depends on the operating system.
RSS includes more than the Rust heap. Allocator counters measure successful Rust allocations and reallocations.
Requested allocated bytes include the whole replacement allocation for each successful reallocation.
Freed bytes include the replaced allocation. Counter snapshots are not atomic across all three values.
These counters exclude native-library allocations and allocator metadata.
Instrumentation adds overhead and is intended for comparisons between instrumented runs.

The harness establishes a measured reference workload on the machine that runs it.
It does not discover the maximum safe publisher count automatically.
Production capacity requires longer runs on deployment hardware with representative codecs, resolutions, bitrates, and viewer counts.
CPU starvation, actual ENOSPC/EIO, kernel packet loss, and slow storage devices remain environment-specific validation scenarios.

## Regression commands

```sh
python3 -m unittest discover -s tools -p test_soak.py
shellcheck tools/soak.sh
cargo test -p rushls --lib --all-features
cargo clippy -p rushls --all-targets --all-features -- -D warnings
```

The all-feature library tests include allocation accounting and the existing allocation-free HTTP hot-path checks.
They also cover admission capacity, cancellation, reconnects, malformed input, failed spill writes, and recording queue bounds.

## Local results: 2026-09-20

Both runs used instrumented release binaries on the local macOS host.
Other development work ran concurrently. These are regression results, not isolated capacity benchmarks.

| Measurement | Mixed failures | DVR pressure |
|---|---:|---:|
| Steady workload duration | 180 seconds | 65 seconds |
| Healthy publishers | 5 | 2 |
| Normal / slow viewers | 5 / 1 | 2 / 1 |
| Completed media downloads | 1,015 | 170 |
| Viewer errors | 0 | 0 |
| Peak sampled RSS | 36.94 MiB | 22.25 MiB |
| Second-half RSS growth | 6.41 MiB | −0.02 MiB |
| Second-half requested-heap growth | −0.21 MiB | −0.13 MiB |
| Peak sampled `ps` CPU | 9.7% | 1.3% |
| Maximum playlist request time | 10.97 ms | 7.56 ms |
| Maximum normal media request time | 2.95 ms | 1.39 ms |
| Successful allocation/reallocation calls | 2,389,938 | 265,464 |
| Cumulative requested allocation bytes | 4,261,754,909 | 485,146,460 |
| Maximum retained disk bytes | 0 | 1,571,781 |

The mixed run completed 50 reconnect publications and 1,734 malformed RTMP connections.
The injected recorder failure lost 60 recording segments. Recording resumed after restoration, while live downloads continued without errors.
Both runs ended with zero active sessions, retained memory, retained disk bytes, and pending spill jobs.
A final 40-second run combined both modes and passed the per-rendition progress and zero-retained-byte cleanup assertions.

Artifacts from these runs are local and temporary:

- `/tmp/rushls-soak-release-validation`
- `/tmp/rushls-soak-spill-validation`
- `/tmp/rushls-soak-final-combined`

Future capacity runs must retain their artifacts with the release evidence.
The validated reference workload is limited to the publisher counts and synthetic media described here.
No maximum production capacity is established by these results.
