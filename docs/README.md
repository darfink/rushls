# Rushls documentation

Start with the [project README](../README.md) for installation and a first stream.

## Guides

| Page | Covers |
| --- | --- |
| [Installation](installation.md) | Release archives, the container image, checksums, and Windows notes |
| [Publishing](publishing.md) | FFmpeg, GStreamer, and OBS recipes; multitrack; captions |
| [MoQ](moq.md) | Media over QUIC ingest, catalog requirements, captions, and browser publishing |
| [Configuration](configuration.md) | How settings load and behave: listeners, publishers, HLS, storage, recording, authorization |
| [Configuration reference](configuration-reference.md) | Every setting with its default, environment variable, and flag |
| [Deployment](deployment.md) | Ports, storage, certificates, reverse proxies and CDNs, shutdown, and systemd |
| [Admission and hooks](admission-and-hooks.md) | The admission request and response, lifecycle events, and signatures |
| [Input handling](input-handling.md) | Strict and permissive input, gaps, and timestamp failures |
| [Metrics](metrics.md) | Prometheus series, dashboards, alerts, and diagnosing stalls |
| [Players](players.md) | Tested players, codec support, and known player issues |

## Contributing

[Development documentation](development/README.md) covers the architecture, CI, releases, and validation records.
