# Architecture

Rushls keeps media behavior in the application and infrastructure in five local crates.
All five crates are direct application dependencies. Cargo resolves their versions from the root manifest.

| Crate | Responsibility |
| --- | --- |
| `cc-config` | TOML discovery, interpolation, environment and CLI overrides, secret files, and source diagnostics |
| `cc-outbound` | HTTP connection pools, credentials, deadlines, and response limits |
| `cc-hooks` | Lifecycle envelopes, signing, ordering, retries, and bounded delivery queues |
| `cc-tls` | PEM loading, certificate rotation, client identities, and TLS listeners |
| `cc-metrics` | Metrics credentials and Prometheus label escaping |

`cc-hooks` depends on `cc-outbound`. Admission and JWKS requests use HTTP transport
without hook delivery. Separate crates keep those dependencies explicit.

The application owns its configuration schema and semantic validation.
It also owns stream identities, authorization responses, lifecycle event contents,
metric registries, media timing, and retention policy.

The media path runs through transport, demuxing, normalization, segmentation,
packaging, storage, and HTTP delivery. See the module documentation in
[`src/lib.rs`](../src/lib.rs) for the layer boundaries.

The RTMP protocol implementation comes from the `rtmpx` registry dependency.
The application uses `rsrt` for SRT and Rust media libraries for demuxing and packaging.

Shared configuration startup assertions live in
[`crates/cc-config/tests/support/fixtures.rs`](../crates/cc-config/tests/support/fixtures.rs).
Application integration tests call those assertions against the built executable.
