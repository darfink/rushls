# rushls-config

Configuration loading independent of application policy. Application schemas use
`conf::Conf` with Serde support. The loader owns file discovery, interpolation,
source precedence, environment warnings, and source metadata.

## Application integration

```rust
use std::path::PathBuf;
use conf::Conf;
use rushls_config::{Loader, SecretString};

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

The application declares `conf` as a direct dependency because its derive macro
uses the `conf` crate path. The loader prefix matches the schema prefix.
A field marked `env` derives its environment name from the field name.
Nested structs use `#[conf(flatten, prefix)]` to derive nested names.
The example accepts `EXAMPLE_LISTEN`, `--listen`, and the TOML key `listen`.

Application validation runs after loading. The shared crate does not construct
services, choose media policies, or start background tasks.

## Source contract

Values resolve in this order, with later sources overriding earlier sources:

1. Compiled defaults
2. TOML document
3. Environment variables
4. CLI arguments.

File selection uses `--config`, then `<PREFIX>CONFIG`. An explicit path must
exist. Without an explicit path, discovery selects the first file named
`<app>.toml` in the working directory or platform configuration directories.
The local platform directory precedes the roaming directory.
Paths inside the document remain relative to the process working directory.

`explicit_only().load_from(args, env)` disables discovery and accepts source
snapshots. Tests do not mutate the process environment or read the live shell.
`load()` reads the live process arguments and environment.

Unknown CLI arguments and TOML fields fail startup. Unknown application-prefixed
environment variables produce warnings and are ignored as configuration overrides.
Environment names come from the derived schema, including declared aliases.
`Loaded::warnings` contains sorted, deduplicated diagnostics with variable names
but no values. Each application reports these diagnostics at startup.
Variables outside the application prefix produce no warning.

All environment variables remain available for explicit TOML interpolation.
Custom interpolation inputs can use names such as `INGEST_HOST` to avoid warnings
about undeclared application-prefixed names.

Schemas can explicitly contain maps with operator-defined keys. Those maps
retain their own validation of entry fields.

## Interpolation

Interpolation applies to TOML string values, including strings inside arrays
and tables. It never changes keys or table names.

| Syntax | Meaning |
| --- | --- |
| `${NAME}` | Required environment variable |
| `${NAME:-fallback}` | Literal fallback when the variable is absent |
| `$$` | Literal dollar sign |
| `$NAME` | Literal text |

Names use ASCII letters, digits, and underscores, starting with a letter or
underscore. A defined empty value remains empty, even with a fallback.
Inserted values are literal strings. They cannot introduce TOML fields or
trigger another interpolation pass. Fallback values are not recursively expanded.
Interpolation runs before overrides, so unresolved file references still fail
when another source overrides the field.

## Credentials and diagnostics

Secret fields use `#[conf(env, secret)]` and `SecretString` for redacted Debug
output and Serde type errors. Existing string fields can use
`serde(deserialize_with = "rushls_config::deserialize_secret")` for error redaction. `conf` disallows inline CLI arguments on fields marked secret.

Credentials use one `TextSource` field each: a literal string (including
`"${VAR}"` interpolation) or `{ file = "/path" }`. The value's shape selects
the source, so there is no separate `_file` field to conflict with. Environment
values accept the same two spellings. `TextSource::read` returns the text.
Mounted secret files lose trailing CR/LF characters only. Spaces remain part of
the credential. Inline values remain unchanged.

`Loaded::sources` maps field identifiers to defaults, CLI arguments, environment
names, or document names. It contains no configuration values.
TOML syntax diagnostics omit source lines. Interpolation diagnostics omit the
value being expanded. Help and version remain available with broken configuration.
Applications own diagnostic output and exit handling.

## Dependencies and tests

This crate depends on parsing and filesystem libraries. It has no dependency
on HTTP, hooks, TLS, or the application.

```sh
cargo test -p rushls-config --locked
```
