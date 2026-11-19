# rushls-common

Shared infrastructure for [Rushls](https://github.com/darfink/rushls) and its sibling services.
Every module sits behind a Cargo feature of the same name so dependents compile only what they use.
Nothing here picks application policy or emits logs directly—reporting stays in each application's own observer.

| Feature | Module | Provides |
| --- | --- | --- |
| `config` | `config` | Layered configuration loading over `conf` schemas |
| `outbound` | `outbound` | Pooled HTTP client for operator-configured endpoints, with optional client TLS |
| `hooks` | `hooks` | Ordered, best-effort lifecycle event delivery (CloudEvents JSON + Standard Webhooks HMAC) |
| `tls` | `tls` | TLS termination with live certificate rotation |
| `axum` | `tls` | `axum::serve::Listener` integration for the rotating TLS listener |
| `metrics` | `metrics` | Prometheus helpers: bearer token checks and label escaping |
| `proxy-protocol` | `proxy_protocol` | PROXY protocol v1 and v2 header parsing ahead of a TCP stream |
| `accept` | `accept` | Accept-loop error classification and backoff |

`outbound` enables `tls`, `hooks` enables `outbound`, `tls` enables `accept`, and `axum` enables `tls`.

## Configuration (`config`)

Application-agnostic configuration loading over `conf::Conf` + Serde schemas.
`rushls-common` handles file discovery, environment interpolation, precedence, unknown-variable warnings, and value-source tracking, while each application owns its schema, validation, and runtime wiring.

### Application integration

```rust
use std::path::PathBuf;
use conf::Conf;
use rushls_common::config::{Loader, SecretString};

#[derive(Conf)]
#[conf(serde, name = "example", env_prefix = "EXAMPLE_")]
pub struct Config {
    #[conf(long, env, serde(skip))]
    config: Option<PathBuf>,

    #[conf(long, env, default_value = "127.0.0.1:8080")]
    listen: std::net::SocketAddr,

    #[conf(env, secret)]
    token: Option<SecretString>,
}

let loaded = Loader::new("example", "EXAMPLE_").load::<Config>()?;
```

Declare `conf` as a direct dependency in the application (its derive macro expands against the `conf` path) and match the loader's prefix to `env_prefix`.
Fields marked `env` derive their environment names automatically, and nested structs use `#[conf(flatten, prefix)]`. In the example above, `listen` accepts TOML `listen`, `EXAMPLE_LISTEN`, and `--listen`.

### Precedence and discovery

Values resolve in order, later wins:

1. Compiled defaults
2. TOML file
3. Environment variables
4. CLI arguments

File selection checks `--config`, then `<PREFIX>CONFIG` (an explicit path must exist). Without an explicit path, discovery picks the first `<app>.toml` found in the working directory, platform local config directory, platform roaming config directory, or `/etc/<app>/<app>.toml` on Unix. Paths inside the file stay relative to the process working directory.

For deterministic tests, call `Loader::explicit_only().load_from(args, env)` to disable discovery and pass explicit argument/environment snapshots without mutating process state.

Unknown CLI flags and unknown TOML keys fail startup. Unknown `<PREFIX>*` environment variables are ignored as overrides and returned in `Loaded::warnings` (sorted, deduplicated, names only—never values). Variables outside the prefix do not warn, so custom interpolation inputs such as `INGEST_HOST` can be used cleanly.

### Interpolation

Interpolation expands inside TOML string values (including strings in arrays and tables) before overrides apply. Keys and table names are never modified.

| Syntax | Meaning |
| --- | --- |
| `${NAME}` | Required environment variable (fails if unset; stays empty if explicitly set to `""`) |
| `${NAME:-fallback}` | Literal fallback when the variable is unset or empty, as in a POSIX shell |
| `$$` | Literal `$` |
| `$NAME` | Literal text |

Variable names use ASCII letters, digits, and underscores, starting with a letter or underscore.
Expanded values and fallbacks are literal strings—they never inject TOML syntax or trigger a second interpolation pass.
Because interpolation runs before overrides, an unresolved reference in the file still fails even if an environment variable or CLI flag overrides that field.

### Credentials and diagnostics

- Mark secret fields with `#[conf(env, secret)]` and `SecretString` (or `serde(deserialize_with = "rushls_common::config::deserialize_secret")`) so `Debug` output and Serde errors never leak values. `conf` also prevents passing inline CLI arguments to secret fields.
- Use `TextSource` for credentials that accept either an inline string (including `"${VAR}"`) or `{ file = "/path" }` in TOML and environment variables. Mounted secret files strip trailing CR/LF characters while preserving spaces; inline values stay verbatim.
- `Loaded::sources` records where each field came from (`Default`, `File`, `Environment`, `Cli`) without storing values.
- TOML syntax errors omit source lines, interpolation errors omit expanded values, and `--help` / `--version` keep working even with a broken config file.

### Shared schema vocabulary

Shared value types keep spelling consistent across Rushls and its sibling services:

| Item | Purpose |
| --- | --- |
| `OptionalDuration` | A duration, or `"off"` / `"none"` where disabling a deadline is valid |
| `OptionalAddress` | A socket address, or `"off"` |
| `OptionalBytes` | A byte limit, or `"unlimited"` |
| `TomlTable<T>` | An open table keyed by operator-chosen names, such as `[hook.<name>]` |
| `TomlValue<T>` | A structured value parsed as TOML text, overridable via environment or CLI |
| `read_credential` | Resolves a `TextSource` credential paired with a file-path CLI flag |

Two helpers keep documentation locked to the schema:

- `reference::reference_markdown` generates a Markdown configuration reference directly from a `conf` schema so tests can verify it stays current.
- `example::AnnotatedExample` parses an annotated example file (commented settings included) so tests can verify every option is documented and every example value parses.

## Testing

The `config` feature itself depends only on parsing and filesystem crates; HTTP, hooks, and TLS dependencies are pulled in only when their features are enabled. Run the full crate test suite with:

```sh
cargo test -p rushls-common --all-features --locked
```
