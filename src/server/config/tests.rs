use std::{
    error::Error,
    ffi::OsString,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{
    admission::{
        ClientInfo, IngestProtocol, PresentedCredential, Principal, PublishRequest,
        PublishResource, StreamPolicy, TakeoverPolicy,
    },
    domain::StreamId,
    observe::lifecycle::Kind,
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
        .authenticate(&request("ignored"))
        .await?;
    assert_eq!(grant.stream_id, StreamId::new("live/presented-key"));
    assert_eq!(grant.principal, Principal("anonymous".into()));
    assert_eq!(grant.policy, StreamPolicy::permissive());
    Ok(())
}

#[tokio::test]
async fn open_authentication_is_the_builtin_default() -> Result<(), Box<dyn Error>> {
    let config = AppConfig::load_from(os(["rushls"]), std::iter::empty())?.resolve()?;
    let grant = config
        .authenticator
        .authenticate(&request("ignored"))
        .await?;

    assert_eq!(grant.stream_id, StreamId::new("live/presented-key"));
    assert_eq!(grant.principal, Principal("anonymous".into()));
    assert_eq!(grant.policy, StreamPolicy::permissive());
    Ok(())
}

#[test]
fn cli_overrides_environment_which_overrides_toml() -> Result<(), Box<dyn Error>> {
    let config = resolve_with(
        r#"
[server]
maximum_concurrent_publishers = 10

[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
key = "from-file"
"#,
        &["--server-maximum-concurrent-publishers", "30"],
        &[("RUSHLS_SERVER_MAXIMUM_CONCURRENT_PUBLISHERS", "20")],
    )??;

    assert_eq!(config.node.maximum_sessions, 30);
    Ok(())
}

#[test]
fn the_removed_publishing_table_is_rejected() -> Result<(), Box<dyn Error>> {
    assert!(
        load_toml(
            r#"
[publishing]
key = "legacy"
stream_id = "live/camera"
"#
        )?
        .is_err()
    );
    Ok(())
}

#[test]
fn the_replaced_array_of_publishers_is_rejected() -> Result<(), Box<dyn Error>> {
    assert!(
        load_toml(
            r#"
[auth]
provider = "static"

[[auth.static.publishers]]
name = "camera"
stream = "live/camera"
key = "key"
"#
        )?
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn static_publishers_select_named_or_builtin_policies() -> Result<(), Box<dyn Error>> {
    let config = resolve_toml(
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
    )??;

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
async fn open_authentication_accepts_any_credential_and_preserves_the_resource()
-> Result<(), Box<dyn Error>> {
    let config = resolve_toml(
        r#"
[auth]
provider = "open"

[auth.policies.restricted]
takeovers = "deny"

[auth.open]
policy = "restricted"
"#,
    )??;

    let grant = config
        .authenticator
        .authenticate(&request("any-value"))
        .await?;
    assert_eq!(grant.stream_id, StreamId::new("live/presented-key"));
    assert_eq!(grant.principal, Principal("anonymous".into()));
    assert_eq!(grant.policy.takeovers, TakeoverPolicy::Deny);
    Ok(())
}

#[tokio::test]
async fn open_authentication_uses_the_builtin_policy_without_a_provider_table()
-> Result<(), Box<dyn Error>> {
    let config = resolve_toml(
        r#"
[auth]
provider = "open"
"#,
    )??;

    let grant = config
        .authenticator
        .authenticate(&request("ignored"))
        .await?;
    assert_eq!(grant.policy, StreamPolicy::permissive());
    Ok(())
}

#[test]
fn configured_but_unselected_auth_providers_are_rejected() -> Result<(), Box<dyn Error>> {
    for configuration in [
        r#"
[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
key = "key"

[auth.open]
policy = "default"
"#,
        r#"
[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
key = "key"

[auth.open]
"#,
        r#"
[auth]
provider = "open"

[auth.static.publishers.camera]
stream = "live/camera"
key = "key"
"#,
    ] {
        assert!(matches!(
            resolve_toml(configuration)?,
            Err(ConfigError::Invalid(_))
        ));
    }
    Ok(())
}

#[test]
fn an_unknown_open_policy_is_rejected() -> Result<(), Box<dyn Error>> {
    let result = resolve_toml(
        r#"
[auth]
provider = "open"

[auth.open]
policy = "missing"
"#,
    )?;

    assert!(matches!(result, Err(ConfigError::Invalid(_))));
    Ok(())
}

#[tokio::test]
async fn a_static_publishing_key_can_be_read_from_a_file() -> Result<(), Box<dyn Error>> {
    let secret = TempConfig::new("mounted-secret")?;
    let config = resolve_toml(&format!(
        r#"
[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
key_file = "{}"
"#,
        secret.path.display()
    ))??;

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
        let result = resolve_toml(&format!(
            r#"
[auth]
provider = "static"
{publishers}
"#
        ))?;

        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }
    Ok(())
}

#[test]
fn an_unknown_static_policy_is_rejected() -> Result<(), Box<dyn Error>> {
    let result = resolve_toml(
        r#"
[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
key = "key"
policy = "missing"
"#,
    )?;

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
        let result = resolve_with_env(&[("RUSHLS_HTTP_CORS_ORIGINS", rejected)])?;

        assert!(
            matches!(result, Err(ConfigError::Invalid(_))),
            "{rejected} should be refused"
        );
    }
    Ok(())
}

#[test]
fn a_wildcard_cors_pattern_is_accepted() -> Result<(), Box<dyn Error>> {
    let config = resolve_with_env(&[(
        "RUSHLS_HTTP_CORS_ORIGINS",
        "https://*.example.com,https://**.video.example.com",
    )])??;

    let AllowedOrigins::Only(patterns) = &config.node.http.cors.allowed_origins else {
        panic!("an allowlist was configured");
    };
    assert_eq!(patterns.len(), 2);
    Ok(())
}

#[test]
fn credentialed_wildcard_cors_is_rejected() -> Result<(), Box<dyn Error>> {
    let result = resolve_with_env(&[("RUSHLS_HTTP_CORS_ALLOW_CREDENTIALS", "true")])?;

    assert!(matches!(result, Err(ConfigError::Invalid(_))));
    Ok(())
}

#[test]
fn metrics_endpoint_and_authentication_resolve_from_configuration() -> Result<(), Box<dyn Error>> {
    let config = resolve_with_env(&[
        ("RUSHLS_METRICS_ENABLED", "true"),
        ("RUSHLS_METRICS_TOKEN", "scrape-secret"),
        ("RUSHLS_METRICS_PER_STREAM", "true"),
    ])??;

    assert!(config.node.metrics.enabled);
    assert!(config.node.metrics.token.is_some());
    assert!(config.node.metrics.export.per_stream);
    Ok(())
}

#[test]
fn an_empty_metrics_token_is_rejected() -> Result<(), Box<dyn Error>> {
    let result = resolve_with_env(&[("RUSHLS_METRICS_TOKEN", "")])?;

    assert!(matches!(result, Err(ConfigError::Invalid(_))));
    Ok(())
}

#[test]
fn existing_optional_secrets_accept_mounted_files() -> Result<(), Box<dyn Error>> {
    let secret = TempConfig::new("mounted-secret")?;
    let config = resolve_toml(&format!(
        r#"
{STATIC_AUTH}

[ingest.srt]
passphrase_file = "{}"

[metrics]
token_file = "{}"
"#,
        secret.path.display(),
        secret.path.display(),
    ))??;

    assert!(config.node.srt.encryption.is_some());
    assert!(config.node.metrics.token.is_some());
    Ok(())
}

#[test]
fn an_inline_secret_and_its_file_are_mutually_exclusive() -> Result<(), Box<dyn Error>> {
    let secret = TempConfig::new("mounted-secret")?;
    let result = resolve_toml(&format!(
        r#"
{STATIC_AUTH}

[metrics]
token = "inline"
token_file = "{}"
"#,
        secret.path.display(),
    ))?;

    assert!(matches!(result, Err(ConfigError::Invalid(_))));
    Ok(())
}

#[test]
fn tls_requires_both_the_certificate_and_key() -> Result<(), Box<dyn Error>> {
    let result = load_with(
        STATIC_AUTH,
        &[],
        &[("RUSHLS_HTTP_TLS_CERTIFICATE", "/tmp/certificate.pem")],
    )?;

    assert!(result.is_err());
    Ok(())
}

#[test]
fn unknown_toml_keys_are_rejected() -> Result<(), Box<dyn Error>> {
    assert!(
        load_toml(
            r#"
[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
key = "key"

[server]
maximum_concurrent_publishers = 10
maximum_concurrent_publisherz = 11
"#
        )?
        .is_err()
    );
    Ok(())
}

#[test]
fn low_level_pipeline_settings_are_not_part_of_the_public_schema() -> Result<(), Box<dyn Error>> {
    assert!(
        load_toml(
            r#"
[auth]
provider = "static"

[auth.static.publishers.camera]
stream = "live/camera"
key = "key"

[ingest.rtmp.avformat]
io_buffer_size = "64KiB"
"#
        )?
        .is_err()
    );
    Ok(())
}

#[test]
fn the_two_publisher_limits_are_configured_independently() -> Result<(), Box<dyn Error>> {
    const LIMITS: &str = r#"
[server]
maximum_concurrent_publishers = 40
maximum_pending_publishers_per_listener = 7
"#;
    let config = resolve_toml(LIMITS)??;

    assert_eq!(config.node.maximum_sessions, 40);
    assert_eq!(config.node.maximum_pending_publishers_per_listener, 7);

    // The admission budget deliberately need not exceed the session cap: it
    // covers a different population, and a slot is returned as soon as a
    // publisher authenticates rather than being held for the session.
    let raised = resolve_with(
        LIMITS,
        &["--server-maximum-pending-publishers-per-listener", "9"],
        &[("RUSHLS_SERVER_MAXIMUM_PENDING_PUBLISHERS_PER_LISTENER", "8")],
    )??;
    assert_eq!(raised.node.maximum_pending_publishers_per_listener, 9);
    Ok(())
}

#[test]
fn the_http_auth_provider_resolves_and_guards_its_own_settings() -> Result<(), Box<dyn Error>> {
    let valid = r#"
[auth]
provider = "http"

[auth.http]
url = "http://auth-sidecar:8081/v1/publish/admit"
"#;
    assert!(
        resolve_toml(valid)?.is_ok(),
        "a minimal http provider works"
    );

    // The internal admission deadline is 10s. A longer request timeout never
    // takes effect: the session gives up first and blames a stage rather than
    // the service that did not answer.
    let outlives_admission = r#"
[auth]
provider = "http"

[auth.http]
url = "http://auth-sidecar:8081/admit"
request_timeout = "30s"
"#;
    // A response may only name a policy this node actually has.
    let unknown_default = r#"
[auth]
provider = "http"

[auth.http]
url = "http://auth-sidecar:8081/admit"
default_policy = "nonexistent"
"#;
    let unusable_url = r#"
[auth]
provider = "http"

[auth.http]
url = "not-a-url"
"#;
    let missing_table = r#"
[auth]
provider = "http"
"#;
    for configuration in [
        outlives_admission,
        unknown_default,
        unusable_url,
        missing_table,
    ] {
        assert!(
            resolve_toml(configuration)?.is_err(),
            "expected a startup error for:{configuration}"
        );
    }
    Ok(())
}

#[test]
fn an_http_provider_cannot_be_combined_with_another() -> Result<(), Box<dyn Error>> {
    let with_static = r#"
[auth]
provider = "http"

[auth.http]
url = "http://auth-sidecar:8081/admit"

[auth.static.publishers.camera]
stream = "live/camera"
key = "key"
"#;
    let unselected_http = r#"
[auth]
provider = "open"

[auth.http]
url = "http://auth-sidecar:8081/admit"
"#;

    for configuration in [with_static, unselected_http] {
        assert!(
            resolve_toml(configuration)?.is_err(),
            "a provider table nobody reads is a typo, not a fallback:{configuration}"
        );
    }
    Ok(())
}

#[test]
fn hooks_are_absent_until_an_endpoint_is_configured() -> Result<(), Box<dyn Error>> {
    let resolved = resolve_toml("[hooks]\nsource = \"urn:rushls:node:studio\"\n")??;

    assert!(
        resolved.hooks.is_none(),
        "a node delivering nothing should not even build an outbound client"
    );
    Ok(())
}

#[test]
fn a_configured_endpoint_resolves_to_a_subscription() -> Result<(), Box<dyn Error>> {
    let resolved = resolve_toml(
        r#"
[hooks]
source = "urn:rushls:node:studio"

[hooks.endpoints.automation]
url = "http://automation:9000/events"
events = ["session.started", "session.ended"]
maximum_attempts = 2
"#,
    )??;

    let hooks = resolved.hooks.ok_or("hooks resolve")?;
    assert_eq!(hooks.config.source, "urn:rushls:node:studio");
    let hook = hooks.config.hooks.first().ok_or("one endpoint")?;
    assert_eq!(&*hook.name, "automation");
    assert_eq!(hook.maximum_attempts, 2);
    assert_eq!(
        hook.events,
        [Kind::SessionStarted, Kind::SessionEnded]
            .into_iter()
            .collect(),
        "a subscription is exactly what was asked for, never widened"
    );
    Ok(())
}

#[test]
fn an_endpoint_that_could_never_deliver_is_rejected() -> Result<(), Box<dyn Error>> {
    let unknown_event = r#"
[hooks.endpoints.automation]
url = "http://automation:9000/events"
events = ["session.exploded"]
"#;
    let no_events = r#"
[hooks.endpoints.automation]
url = "http://automation:9000/events"
events = []
"#;
    let no_attempts = r#"
[hooks.endpoints.automation]
url = "http://automation:9000/events"
events = ["session.ended"]
maximum_attempts = 0
"#;
    let no_queue = r#"
[hooks.endpoints.automation]
url = "http://automation:9000/events"
events = ["session.ended"]
queue_capacity = 0
"#;
    let unusable_url = r#"
[hooks.endpoints.automation]
url = "automation:9000"
events = ["session.ended"]
"#;

    for configuration in [
        unknown_event,
        no_events,
        no_attempts,
        no_queue,
        unusable_url,
    ] {
        assert!(
            resolve_toml(configuration)?.is_err(),
            "expected a startup error for:{configuration}"
        );
    }
    Ok(())
}

/// Writes one TOML fragment and loads it, with optional argument and
/// environment overrides layered on top.
///
/// The nested result separates "the temporary file could not be written" — a
/// broken test — from "the configuration was refused", which is what every
/// caller here is actually asserting on. Loading stops short of [`resolve`],
/// so a test about the *schema* cannot accidentally be satisfied by a later
/// validation failure instead.
///
/// [`resolve`]: AppConfig::resolve
fn load_with(
    configuration: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Result<Result<AppConfig, ConfigError>, Box<dyn Error>> {
    let file = TempConfig::new(configuration)?;
    let mut args = os(["rushls", "--config"]).collect::<Vec<_>>();
    args.push(file.path.clone().into_os_string());
    args.extend(arguments.iter().copied().map(OsString::from));

    Ok(AppConfig::load_from(
        args,
        environment
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into())),
    ))
}

/// As [`load_with`], carried through to a resolved runtime configuration.
fn resolve_with(
    configuration: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Result<Result<ResolvedAppConfig, ConfigError>, Box<dyn Error>> {
    Ok(load_with(configuration, arguments, environment)?.and_then(AppConfig::resolve))
}

/// Loads one TOML fragment on its own, for tests about the schema itself.
fn load_toml(configuration: &str) -> Result<Result<AppConfig, ConfigError>, Box<dyn Error>> {
    load_with(configuration, &[], &[])
}

/// Loads and resolves one TOML fragment on its own.
fn resolve_toml(
    configuration: &str,
) -> Result<Result<ResolvedAppConfig, ConfigError>, Box<dyn Error>> {
    resolve_with(configuration, &[], &[])
}

/// Resolves environment overrides against a minimal authenticated node.
fn resolve_with_env(
    environment: &[(&str, &str)],
) -> Result<Result<ResolvedAppConfig, ConfigError>, Box<dyn Error>> {
    resolve_with(STATIC_AUTH, &[], environment)
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

struct TempConfig {
    path: PathBuf,
}

/// Distinguishes files within one test binary.
///
/// A clock reading cannot: `SystemTime` is not nanosecond-granular on every
/// platform, so two calls close together can produce the same name. A test that
/// writes a secret and then the configuration pointing at it would then have
/// the second file overwrite the first, leaving the secret's path holding TOML
/// — and the first `Drop` deleting the file the second still needed.
static NEXT_TEMPORARY_FILE: AtomicU64 = AtomicU64::new(0);

impl TempConfig {
    fn new(contents: &str) -> std::io::Result<Self> {
        // The process id separates concurrent test binaries; the counter
        // separates files within one of them.
        let path = std::env::temp_dir().join(format!(
            "rushls-config-test-{}-{}.toml",
            std::process::id(),
            NEXT_TEMPORARY_FILE.fetch_add(1, Ordering::Relaxed)
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
