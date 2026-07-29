use std::{
    error::Error,
    ffi::OsString,
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    admission::{Principal, StreamPolicy},
    domain::StreamId,
    server::{AllowedOrigins, NodeConfig},
};

use super::{AppConfig, ConfigError};

#[test]
fn the_reference_file_resolves_to_the_runtime_defaults() -> Result<(), Box<dyn Error>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("rushls.toml");
    let config = AppConfig::load_from(
        os([
            "rushls",
            "--config",
            path.to_str().ok_or("workspace path is not UTF-8")?,
        ]),
        std::iter::empty(),
    )?
    .resolve()?;

    assert_eq!(config.node, NodeConfig::default());
    assert_eq!(config.stream_id, StreamId::new("live/camera"));
    assert_eq!(config.principal, Principal("configured-publisher".into()));
    assert_eq!(config.stream_policy, StreamPolicy::permissive());
    assert_eq!(config.publish_key, "replace-with-a-publishing-key");
    Ok(())
}

#[test]
fn cli_overrides_environment_which_overrides_toml() -> Result<(), Box<dyn Error>> {
    let file = TempConfig::new(
        r#"
[server]
maximum_concurrent_publishers = 10

[publishing]
key = "from-file"
"#,
    )?;
    let config = AppConfig::load_from(
        os([
            "rushls",
            "--config",
            file.path.to_str().ok_or("temporary path is not UTF-8")?,
            "--server-maximum-concurrent-publishers",
            "30",
        ]),
        env([
            ("RUSHLS_SERVER_MAXIMUM_CONCURRENT_PUBLISHERS", "20"),
            ("RUSHLS_PUBLISHING_KEY", "from-env"),
        ]),
    )?
    .resolve()?;

    assert_eq!(config.node.maximum_sessions, 30);
    assert_eq!(config.publish_key, "from-env");
    Ok(())
}

#[test]
fn legacy_environment_names_remain_supported() -> Result<(), Box<dyn Error>> {
    let config = AppConfig::load_from(
        os(["rushls"]),
        env([
            ("RUSHLS_PUBLISH_KEY", "legacy-key"),
            ("RUSHLS_STREAM_ID", "legacy/stream"),
            ("RUSHLS_RTMP_LISTEN", "127.0.0.1:1936"),
            ("RUSHLS_HTTP_LISTEN", "127.0.0.1:8081"),
            ("RUSHLS_PUBLIC_BASE", "https://cdn.example/hls"),
            ("RUSHLS_CORS_ORIGINS", "https://player.example"),
            ("RUSHLS_CORS_CREDENTIALS", "true"),
        ]),
    )?
    .resolve()?;

    assert_eq!(config.publish_key, "legacy-key");
    assert_eq!(config.stream_id, StreamId::new("legacy/stream"));
    assert_eq!(config.node.rtmp_address, "127.0.0.1:1936".parse()?);
    assert_eq!(config.node.http_address, "127.0.0.1:8081".parse()?);
    assert!(config.node.http.cors.allow_credentials);
    Ok(())
}

#[test]
fn the_canonical_environment_name_wins_over_its_legacy_alias() -> Result<(), Box<dyn Error>> {
    let config = AppConfig::load_from(
        os(["rushls"]),
        env([
            ("RUSHLS_PUBLISH_KEY", "legacy"),
            ("RUSHLS_PUBLISHING_KEY", "canonical"),
        ]),
    )?
    .resolve()?;

    assert_eq!(config.publish_key, "canonical");
    Ok(())
}

#[test]
fn a_malformed_cors_pattern_stops_startup() -> Result<(), Box<dyn Error>> {
    // Silently matching nothing would be the alternative, and nobody would
    // notice until a player could not reach the origin.
    for rejected in [
        "https://player.example.com/",
        "https://*example.com",
        "https://foo.*.example.com",
        "player.example.com",
    ] {
        let result = AppConfig::load_from(
            os(["rushls"]),
            env([
                ("RUSHLS_PUBLISHING_KEY", "key"),
                ("RUSHLS_HTTP_CORS_ORIGINS", rejected),
            ]),
        )?
        .resolve();

        assert!(
            matches!(result, Err(ConfigError::Invalid(_))),
            "{rejected} should be refused"
        );
    }
    Ok(())
}

#[test]
fn a_wildcard_cors_pattern_is_accepted() -> Result<(), Box<dyn Error>> {
    let config = AppConfig::load_from(
        os(["rushls"]),
        env([
            ("RUSHLS_PUBLISHING_KEY", "key"),
            (
                "RUSHLS_HTTP_CORS_ORIGINS",
                "https://*.example.com,https://**.video.example.com",
            ),
        ]),
    )?
    .resolve()?;

    let AllowedOrigins::Only(patterns) = &config.node.http.cors.allowed_origins else {
        panic!("an allowlist was configured");
    };
    assert_eq!(patterns.len(), 2);
    Ok(())
}

#[test]
fn credentialed_wildcard_cors_is_rejected() -> Result<(), Box<dyn Error>> {
    let result = AppConfig::load_from(
        os(["rushls"]),
        env([
            ("RUSHLS_PUBLISHING_KEY", "key"),
            ("RUSHLS_HTTP_CORS_ALLOW_CREDENTIALS", "true"),
        ]),
    )?
    .resolve();

    assert!(matches!(result, Err(ConfigError::Invalid(_))));
    Ok(())
}

#[test]
fn metrics_endpoint_and_authentication_resolve_from_configuration() -> Result<(), Box<dyn Error>> {
    let config = AppConfig::load_from(
        os(["rushls"]),
        env([
            ("RUSHLS_PUBLISHING_KEY", "key"),
            ("RUSHLS_METRICS_ENABLED", "true"),
            ("RUSHLS_METRICS_TOKEN", "scrape-secret"),
            ("RUSHLS_METRICS_PER_STREAM", "true"),
        ]),
    )?
    .resolve()?;

    assert!(config.node.metrics.enabled);
    assert!(config.node.metrics.token.is_some());
    assert!(config.node.metrics.export.per_stream);
    Ok(())
}

#[test]
fn an_empty_metrics_token_is_rejected() -> Result<(), Box<dyn Error>> {
    let result = AppConfig::load_from(
        os(["rushls"]),
        env([
            ("RUSHLS_PUBLISHING_KEY", "key"),
            ("RUSHLS_METRICS_TOKEN", ""),
        ]),
    )?
    .resolve();

    assert!(matches!(result, Err(ConfigError::Invalid(_))));
    Ok(())
}

#[test]
fn tls_requires_both_the_certificate_and_key() {
    let result = AppConfig::load_from(
        os(["rushls"]),
        env([
            ("RUSHLS_PUBLISHING_KEY", "key"),
            ("RUSHLS_HTTP_TLS_CERTIFICATE", "/tmp/certificate.pem"),
        ]),
    );

    assert!(result.is_err());
}

#[test]
fn unknown_toml_keys_are_rejected() -> Result<(), Box<dyn Error>> {
    let file = TempConfig::new(
        r#"
[publishing]
key = "key"

[server]
maximum_concurrent_publishers = 10
maximum_concurrent_publisherz = 11
"#,
    )?;

    assert!(
        AppConfig::load_from(
            os([
                "rushls",
                "--config",
                file.path.to_str().ok_or("temporary path is not UTF-8")?,
            ]),
            std::iter::empty(),
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn low_level_pipeline_settings_are_not_part_of_the_public_schema() -> Result<(), Box<dyn Error>> {
    let file = TempConfig::new(
        r#"
[publishing]
key = "key"

[ingest.rtmp.avformat]
io_buffer_size = "64KiB"
"#,
    )?;

    assert!(
        AppConfig::load_from(
            os([
                "rushls",
                "--config",
                file.path.to_str().ok_or("temporary path is not UTF-8")?,
            ]),
            std::iter::empty(),
        )
        .is_err()
    );
    Ok(())
}

fn os<const N: usize>(values: [&str; N]) -> impl Iterator<Item = OsString> {
    values.into_iter().map(OsString::from)
}

fn env<const N: usize>(values: [(&str, &str); N]) -> impl Iterator<Item = (OsString, OsString)> {
    values
        .into_iter()
        .map(|(key, value)| (key.into(), value.into()))
}

struct TempConfig {
    path: PathBuf,
}

impl TempConfig {
    fn new(contents: &str) -> std::io::Result<Self> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rushls-config-test-{}-{unique}.toml",
            std::process::id()
        ));
        fs::write(&path, contents)?;
        Ok(Self { path })
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
