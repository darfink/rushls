use std::{
    error::Error,
    ffi::OsString,
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    admission::{
        ClientInfo, IngestProtocol, PresentedCredential, Principal, PublishRequest,
        PublishResource, StreamPolicy, TakeoverPolicy,
    },
    domain::StreamId,
    server::{AllowedOrigins, NodeConfig, ResolvedAppConfig},
};

use super::{AppConfig, ConfigError};

const STATIC_AUTH: &str = r#"
[auth]
provider = "static"

[auth.static.publishers.configured-publisher]
stream = "live/camera"
key = "key"
"#;

#[tokio::test]
async fn the_reference_file_resolves_to_the_runtime_defaults() -> Result<(), Box<dyn Error>> {
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
    let grant = config
        .authenticator
        .authenticate(&request("replace-with-a-publishing-key"))
        .await?;
    assert_eq!(grant.stream_id, StreamId::new("live/camera"));
    assert_eq!(grant.principal, Principal("configured-publisher".into()));
    assert_eq!(grant.policy, StreamPolicy::permissive());
    Ok(())
}

#[test]
fn cli_overrides_environment_which_overrides_toml() -> Result<(), Box<dyn Error>> {
    let file = TempConfig::new(
        r#"
[server]
maximum_concurrent_publishers = 10

[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
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
        env([("RUSHLS_SERVER_MAXIMUM_CONCURRENT_PUBLISHERS", "20")]),
    )?
    .resolve()?;

    assert_eq!(config.node.maximum_sessions, 30);
    Ok(())
}

#[test]
fn the_removed_publishing_table_is_rejected() -> Result<(), Box<dyn Error>> {
    let file = TempConfig::new(
        r#"
[publishing]
key = "legacy"
stream_id = "live/camera"
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
fn the_replaced_array_of_publishers_is_rejected() -> Result<(), Box<dyn Error>> {
    let file = TempConfig::new(
        r#"
[auth]
provider = "static"

[[auth.static.publishers]]
name = "camera"
stream = "live/camera"
key = "key"
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

#[tokio::test]
async fn static_publishers_select_named_or_builtin_policies() -> Result<(), Box<dyn Error>> {
    let file = TempConfig::new(
        r#"
[auth]
provider = "static"

[auth.policies.protected]
takeovers = "deny"
maximum_video_tracks = 1

[auth.static.publishers.camera]
stream = "live/camera"
key = "camera-key"

[auth.static.publishers.stage]
stream = "live/stage"
key = "stage-key"
policy = "protected"
"#,
    )?;
    let config = AppConfig::load_from(
        os([
            "rushls",
            "--config",
            file.path.to_str().ok_or("temporary path is not UTF-8")?,
        ]),
        std::iter::empty(),
    )?
    .resolve()?;

    let camera = config
        .authenticator
        .authenticate(&request("camera-key"))
        .await?;
    assert_eq!(camera.stream_id, StreamId::new("live/camera"));
    assert_eq!(camera.policy, StreamPolicy::permissive());

    let stage = config
        .authenticator
        .authenticate(&request("stage-key"))
        .await?;
    assert_eq!(stage.stream_id, StreamId::new("live/stage"));
    assert_eq!(stage.principal, Principal("stage".into()));
    assert_eq!(stage.policy.takeovers, TakeoverPolicy::Deny);
    assert_eq!(stage.policy.maximum_video_tracks, 1);
    Ok(())
}

#[tokio::test]
async fn a_static_publishing_key_can_be_read_from_a_file() -> Result<(), Box<dyn Error>> {
    let secret = TempConfig::new("mounted-secret")?;
    let file = TempConfig::new(&format!(
        r#"
[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
key_file = "{}"
"#,
        secret.path.display()
    ))?;
    let config = AppConfig::load_from(
        os([
            "rushls",
            "--config",
            file.path.to_str().ok_or("temporary path is not UTF-8")?,
        ]),
        std::iter::empty(),
    )?
    .resolve()?;

    config
        .authenticator
        .authenticate(&request("mounted-secret"))
        .await?;
    Ok(())
}

#[test]
fn static_authentication_rejects_ambiguous_or_duplicate_keys() -> Result<(), Box<dyn Error>> {
    for publishers in [
        r#"
[auth.static.publishers.camera]
stream = "live/camera"
key = "same"
key_file = "/run/secrets/camera"
"#,
        r#"
[auth.static.publishers.camera]
stream = "live/camera"
key = "same"

[auth.static.publishers.stage]
stream = "live/stage"
key = "same"
"#,
    ] {
        let file = TempConfig::new(&format!(
            r#"
[auth]
provider = "static"
{publishers}
"#
        ))?;
        let result = AppConfig::load_from(
            os([
                "rushls",
                "--config",
                file.path.to_str().ok_or("temporary path is not UTF-8")?,
            ]),
            std::iter::empty(),
        )?
        .resolve();

        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }
    Ok(())
}

#[test]
fn an_unknown_static_policy_is_rejected() -> Result<(), Box<dyn Error>> {
    let file = TempConfig::new(
        r#"
[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
key = "key"
policy = "missing"
"#,
    )?;
    let result = AppConfig::load_from(
        os([
            "rushls",
            "--config",
            file.path.to_str().ok_or("temporary path is not UTF-8")?,
        ]),
        std::iter::empty(),
    )?
    .resolve();

    assert!(matches!(result, Err(ConfigError::Invalid(_))));
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
        let result = resolve_with_env([("RUSHLS_HTTP_CORS_ORIGINS", rejected)]);

        assert!(
            matches!(result, Err(ConfigError::Invalid(_))),
            "{rejected} should be refused"
        );
    }
    Ok(())
}

#[test]
fn a_wildcard_cors_pattern_is_accepted() -> Result<(), Box<dyn Error>> {
    let config = resolve_with_env([(
        "RUSHLS_HTTP_CORS_ORIGINS",
        "https://*.example.com,https://**.video.example.com",
    )])?;

    let AllowedOrigins::Only(patterns) = &config.node.http.cors.allowed_origins else {
        panic!("an allowlist was configured");
    };
    assert_eq!(patterns.len(), 2);
    Ok(())
}

#[test]
fn credentialed_wildcard_cors_is_rejected() -> Result<(), Box<dyn Error>> {
    let result = resolve_with_env([("RUSHLS_HTTP_CORS_ALLOW_CREDENTIALS", "true")]);

    assert!(matches!(result, Err(ConfigError::Invalid(_))));
    Ok(())
}

#[test]
fn metrics_endpoint_and_authentication_resolve_from_configuration() -> Result<(), Box<dyn Error>> {
    let config = resolve_with_env([
        ("RUSHLS_METRICS_ENABLED", "true"),
        ("RUSHLS_METRICS_TOKEN", "scrape-secret"),
        ("RUSHLS_METRICS_PER_STREAM", "true"),
    ])?;

    assert!(config.node.metrics.enabled);
    assert!(config.node.metrics.token.is_some());
    assert!(config.node.metrics.export.per_stream);
    Ok(())
}

#[test]
fn an_empty_metrics_token_is_rejected() -> Result<(), Box<dyn Error>> {
    let result = resolve_with_env([("RUSHLS_METRICS_TOKEN", "")]);

    assert!(matches!(result, Err(ConfigError::Invalid(_))));
    Ok(())
}

#[test]
fn existing_optional_secrets_accept_mounted_files() -> Result<(), Box<dyn Error>> {
    let secret = TempConfig::new("mounted-secret")?;
    let file = TempConfig::new(&format!(
        r#"
{STATIC_AUTH}

[ingest.srt]
passphrase_file = "{}"

[metrics]
token_file = "{}"
"#,
        secret.path.display(),
        secret.path.display(),
    ))?;
    let config = AppConfig::load_from(
        os([
            "rushls",
            "--config",
            file.path.to_str().ok_or("temporary path is not UTF-8")?,
        ]),
        std::iter::empty(),
    )?
    .resolve()?;

    assert!(config.node.srt.encryption.is_some());
    assert!(config.node.metrics.token.is_some());
    Ok(())
}

#[test]
fn an_inline_secret_and_its_file_are_mutually_exclusive() -> Result<(), Box<dyn Error>> {
    let secret = TempConfig::new("mounted-secret")?;
    let file = TempConfig::new(&format!(
        r#"
{STATIC_AUTH}

[metrics]
token = "inline"
token_file = "{}"
"#,
        secret.path.display(),
    ))?;
    let result = AppConfig::load_from(
        os([
            "rushls",
            "--config",
            file.path.to_str().ok_or("temporary path is not UTF-8")?,
        ]),
        std::iter::empty(),
    )?
    .resolve();

    assert!(matches!(result, Err(ConfigError::Invalid(_))));
    Ok(())
}

#[test]
fn tls_requires_both_the_certificate_and_key() {
    let file = TempConfig::new(STATIC_AUTH).expect("temporary config is written");
    let result = AppConfig::load_from(
        os([
            "rushls",
            "--config",
            file.path.to_str().expect("temporary path is UTF-8"),
        ]),
        env([("RUSHLS_HTTP_TLS_CERTIFICATE", "/tmp/certificate.pem")]),
    );

    assert!(result.is_err());
}

#[test]
fn unknown_toml_keys_are_rejected() -> Result<(), Box<dyn Error>> {
    let file = TempConfig::new(
        r#"
[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
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
[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
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

fn resolve_with_env<const N: usize>(
    values: [(&str, &str); N],
) -> Result<ResolvedAppConfig, ConfigError> {
    let file = TempConfig::new(STATIC_AUTH).map_err(|source| ConfigError::Read {
        path: PathBuf::from("<temporary auth config>"),
        source,
    })?;
    AppConfig::load_from(
        os([
            "rushls",
            "--config",
            file.path
                .to_str()
                .ok_or_else(|| ConfigError::Invalid("temporary config path is not UTF-8".into()))?,
        ]),
        env(values),
    )?
    .resolve()
}

fn request(credential: &str) -> PublishRequest {
    PublishRequest {
        protocol: IngestProtocol::Rtmp,
        resource: PublishResource {
            namespace: Some("live".into()),
            name: "presented-key".into(),
        },
        credential: PresentedCredential::new(credential),
        client: ClientInfo {
            remote_address: "127.0.0.1:1935".parse().expect("constant address is valid"),
            encoder: None,
            protocol_version: None,
        },
    }
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
