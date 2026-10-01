# Architecture

Rushls keeps media behavior in the application and infrastructure in one local crate,
`rushls-common`, which Routmp shares. Each module sits behind a feature of the same name,
so a dependent compiles only what it uses.

| Feature and module | Responsibility |
| --- | --- |
| `config` | TOML discovery, interpolation, environment and CLI overrides, secret files, and source diagnostics |
| `outbound` | HTTP connection pools, credentials, client TLS, deadlines, and response limits |
| `hooks` | Lifecycle envelopes, signing, ordering, retries, and bounded delivery queues |
| `tls` | PEM loading, certificate rotation, client identities, and TLS listeners |
| `metrics` | Metrics credentials and Prometheus label escaping |
| `proxy-protocol` | PROXY protocol v1 and v2 headers |
| `accept` | Which `accept` errors are fatal, and the backoff for the rest |

`hooks` enables `outbound`, which enables `tls`. Admission and JWKS requests use HTTP
transport without hook delivery. The modules report through return values and observer
traits rather than logging, so each application keeps control of its own events.

The application owns its configuration schema and semantic validation.
It also owns stream identities, authorization responses, lifecycle event contents,
metric registries, media timing, and retention policy.

The media path runs through transport, demuxing, normalization, segmentation,
packaging, storage, and HTTP delivery. See the module documentation in
[`src/lib.rs`](../../src/lib.rs) for the layer boundaries.

The RTMP protocol implementation comes from the `rtmpx` registry dependency.
The application uses `rsrt` for SRT and Rust media libraries for demuxing and packaging.

Shared configuration startup assertions live in
[`crates/rushls-common/tests/support/fixtures.rs`](../../crates/rushls-common/tests/support/fixtures.rs).
Application integration tests call those assertions against the built executable.
