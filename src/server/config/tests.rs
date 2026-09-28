use std::{
    error::Error,
    ffi::OsString,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use crate::{
    admission::{
        Ceiling, ClientInfo, Floor, IngestProtocol, Pace, PresentedCredential, Principal,
        PublishRequest, PublishResource, StreamPolicy, TakeoverPolicy,
    },
    delivery::store::{DurationRule, TargetDurationMultiple},
    domain::{Codec, StreamId},
    observe::lifecycle::Kind,
    server::{
        AllowedOrigins, ResolvedAppConfig,
        http::fixtures::{scratch, write_pair},
    },
};

use super::{AppConfig, ConfigError, decimal_fraction, parse_duration_rule};

mod fixtures;

#[test]
fn the_example_covers_every_toml_option() -> Result<(), Box<dyn Error>> {
    use conf::{Conf, introspection::ProgramOptionMeta};

    let reference = fixtures::Example::read()?;
    for option in AppConfig::program_options().filter(ProgramOptionMeta::has_serde_source) {
        let id = option.id().to_string();
        // Server fields flatten into the document root; their Rust IDs retain `node`.
        let path = id.strip_prefix("node.").unwrap_or(&id);
        assert!(
            fixtures::contains(&reference.document, path),
            "rushls.example.toml does not document {path}"
        );
    }
    Ok(())
}

#[test]
fn all_documented_examples_match_the_configuration_schema() -> Result<(), Box<dyn Error>> {
    use conf::Conf;

    let reference = fixtures::Example::read()?;
    for (path, value) in &reference.examples {
        let mut document = reference.document.clone();
        fixtures::set(&mut document, path, value.clone())?;
        // Parse each alternative, including commented fields. Do not resolve
        // placeholder certificates or mutually exclusive credential examples.
        let config = AppConfig::conf_builder()
            .args(["rushls"])
            .env(std::iter::empty::<(OsString, OsString)>())
            .doc("rushls.example.toml", document)
            .try_parse()
            .map_err(|error| format!("reference example {path}: {error}"))?;
        config.accept.resolve()?;
        if let Some(record) = config.record {
            record.0.validate()?;
        }
    }
    Ok(())
}

const BASE_CONFIG: &str = "";

/// Loads one shipped file exactly as the binary would, without searching
/// well-known paths.
fn load_shipped(name: &str) -> Result<ResolvedAppConfig, Box<dyn Error>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(name);
    Ok(AppConfig::load_and_resolve_from(
        os([
            "rushls",
            "--config",
            path.to_str().ok_or("workspace path is not UTF-8")?,
        ]),
        std::iter::empty(),
    )?)
}

#[tokio::test]
async fn the_starter_file_is_a_loopback_origin_with_compiled_defaults() -> Result<(), Box<dyn Error>>
{
    // The local starter narrows the listeners and nothing else, so anything
    // that drifts away from a compiled default here is an accident.
    let config = load_shipped("rushls.toml")?;
    assert_eq!(
        config.config_file,
        Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("rushls.toml"))
    );

    assert_eq!(
        config.node.rtmp_address,
        "127.0.0.1:1935".parse().expect("constant is valid")
    );
    assert_eq!(
        config.node.http_address,
        Some("127.0.0.1:8080".parse().expect("constant is valid"))
    );
    assert_eq!(config.node.https_address, None);
    assert_eq!(
        config.node.moq_address, None,
        "MOQ stays off unless the operator turns it on"
    );
    assert_eq!(
        config.node.metrics.listen, None,
        "metrics are off by default"
    );

    let grant = config
        .authenticator
        .authenticate(&request("ignored"))
        .await?;
    assert_eq!(
        grant.policy,
        StreamPolicy {
            // The one narrowing the compiled default makes: silent
            // replacement of a live publisher needs an explicit opt-in.
            takeovers: TakeoverPolicy::Deny,
            input_mode: crate::domain::InputMode::Strict,
            ..StreamPolicy::permissive()
        },
        "anyone may publish anything this origin can mux, at any speed"
    );
    Ok(())
}

#[tokio::test]
async fn the_example_file_is_the_starter_with_optional_documentation() -> Result<(), Box<dyn Error>>
{
    let starter: toml::Value = toml::from_str(include_str!("../../../rushls.toml"))?;
    let example: toml::Value = toml::from_str(include_str!("../../../rushls.example.toml"))?;
    assert_eq!(example, starter, "only the loopback listeners are active");
    let config = load_shipped("rushls.example.toml")?;
    assert!(config.hooks.is_none());
    assert!(config.playback.is_none());
    assert!(config.node.metrics.listen.is_none());
    assert!(config.node.moq_address.is_none());
    let grant = config
        .authenticator
        .authenticate(&request("ignored"))
        .await?;
    assert_eq!(grant.principal, Principal("anonymous".into()));
    Ok(())
}

#[tokio::test]
async fn open_authentication_is_the_builtin_default() -> Result<(), Box<dyn Error>> {
    let config = AppConfig::load_from(os(["rushls"]), std::iter::empty())?
        .resolve_from(std::iter::empty())?;
    let grant = config
        .authenticator
        .authenticate(&request("ignored"))
        .await?;

    assert_eq!(grant.stream_id, StreamId::new("live/presented-key"));
    assert_eq!(grant.principal, Principal("anonymous".into()));
    assert_eq!(
        grant.policy,
        StreamPolicy {
            takeovers: TakeoverPolicy::Deny,
            input_mode: crate::domain::InputMode::Strict,
            ..StreamPolicy::permissive()
        }
    );
    Ok(())
}

#[test]
fn cli_overrides_environment_which_overrides_toml() -> Result<(), Box<dyn Error>> {
    let config = resolve_with(
        r"
[capacity]
publishers = 10
",
        &["--capacity-publishers", "30"],
        &[("RUSHLS_CAPACITY_PUBLISHERS", "20")],
    )??;

    assert_eq!(config.node.maximum_sessions, 30);
    Ok(())
}

#[test]
fn secret_files_accept_a_cli_path_and_secret_values_do_not() -> Result<(), Box<dyn Error>> {
    let secret = TempConfig::new("mounted-secret")?;
    let path = secret.path.to_str().ok_or("temp path is not UTF-8")?;

    let config = resolve_with(
        r#"
[auth.publish]
url = "http://127.0.0.1/admit"
"#,
        &[
            "--auth-publish-token-file",
            path,
            "--metrics-token-file",
            path,
            "--srt-passphrase-file",
            path,
        ],
        &[],
    )??;
    assert!(config.node.metrics.token.is_some());
    assert!(config.node.srt.encryption.is_some());

    for flag in [
        "--auth-publish-token",
        "--metrics-token",
        "--srt-passphrase",
    ] {
        let result = load_with("", &[flag, "inline-secret"], &[])?;
        assert!(
            result.is_err(),
            "{flag} is a secret value and must not be a command-line flag"
        );
    }
    Ok(())
}

#[test]
fn an_explicit_config_path_is_recorded() -> Result<(), Box<dyn Error>> {
    let file = TempConfig::new("[capacity]\npublishers = 11\n")?;
    let resolved = AppConfig::load_and_resolve_from(
        os([
            "rushls",
            "--config",
            file.path.to_str().ok_or("temp path is not UTF-8")?,
        ]),
        std::iter::empty(),
    )?;
    assert_eq!(resolved.config_file.as_deref(), Some(file.path.as_path()));
    assert_eq!(resolved.node.maximum_sessions, 11);
    Ok(())
}

#[test]
fn rushls_config_is_used_when_the_cli_omits_the_file() -> Result<(), Box<dyn Error>> {
    let file = TempConfig::new("[capacity]\npublishers = 12\n")?;
    let resolved = AppConfig::load_and_resolve_from(
        os(["rushls"]),
        [(
            OsString::from("RUSHLS_CONFIG"),
            file.path.clone().into_os_string(),
        )],
    )?;
    assert_eq!(resolved.config_file.as_deref(), Some(file.path.as_path()));
    assert_eq!(resolved.node.maximum_sessions, 12);
    Ok(())
}

#[test]
fn a_cli_config_path_wins_over_rushls_config() -> Result<(), Box<dyn Error>> {
    let cli = TempConfig::new("[capacity]\npublishers = 13\n")?;
    let env = TempConfig::new("[capacity]\npublishers = 14\n")?;
    let resolved = AppConfig::load_and_resolve_from(
        os([
            "rushls",
            "--config",
            cli.path.to_str().ok_or("temp path is not UTF-8")?,
        ]),
        [(
            OsString::from("RUSHLS_CONFIG"),
            env.path.clone().into_os_string(),
        )],
    )?;
    assert_eq!(resolved.config_file.as_deref(), Some(cli.path.as_path()));
    assert_eq!(resolved.node.maximum_sessions, 13);
    Ok(())
}

#[test]
fn a_missing_config_file_is_an_error() -> Result<(), Box<dyn Error>> {
    let missing = std::env::temp_dir().join("rushls-config-does-not-exist.toml");
    let mut args = os(["rushls", "--config"]).collect::<Vec<_>>();
    args.push(missing.clone().into_os_string());
    let error = AppConfig::load_from(args, std::iter::empty())
        .err()
        .ok_or("a missing --config path must fail")?;
    assert!(
        matches!(error, ConfigError::Loading(rushls_config::ConfigError::Read { ref path, .. }) if *path == missing),
        "unexpected error: {error}"
    );
    Ok(())
}

#[test]
fn compiled_defaults_do_not_search_well_known_files() -> Result<(), Box<dyn Error>> {
    // The crate tree has a rushls.toml. A fixture that asked for compiled
    // defaults must not pick it up.
    let resolved = AppConfig::load_and_resolve_from(os(["rushls"]), std::iter::empty())?;
    assert_eq!(resolved.config_file, None);
    Ok(())
}

#[test]
fn unknown_rushls_environment_variables_warn_without_blocking_startup() -> Result<(), Box<dyn Error>>
{
    let env = [
        ("RUSHLS_CAPACITY_PUBLISHERS", "20"),
        ("PAGER", "less"),
        ("RUSHLS_TLS_CERTIFICATE", "do-not-print-this"),
        ("RUSHLS_CAPACITY_MAXIMUM_CONCURRENT_PUBLISHER", "20"),
    ]
    .map(|(key, value)| (OsString::from(key), OsString::from(value)));
    let resolved = AppConfig::load_and_resolve_from(os(["rushls"]), env)?;
    assert_eq!(resolved.node.maximum_sessions, 20);
    let warnings: Vec<_> = resolved
        .warnings
        .iter()
        .filter(|warning| warning.starts_with("unrecognized"))
        .collect();
    assert_eq!(warnings.len(), 2);
    assert!(warnings[0].contains("RUSHLS_CAPACITY_MAXIMUM_CONCURRENT_PUBLISHER"));
    assert!(warnings[1].contains("RUSHLS_TLS_CERTIFICATE"));
    assert!(!format!("{warnings:?}").contains("do-not-print-this"));
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
fn static_authentication_is_removed() -> Result<(), Box<dyn Error>> {
    assert!(load_toml("[auth]\nprovider = \"static\"\n")?.is_err());
    assert!(load_toml("[auth.static.publishers.camera]\nkey = \"key\"\n")?.is_err());
    Ok(())
}

#[tokio::test]
async fn open_authentication_accepts_any_credential_and_preserves_the_resource()
-> Result<(), Box<dyn Error>> {
    let config = resolve_toml(
        r"
[accept]
takeover = false
",
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
async fn a_ceiling_pace_alone_is_realtime_with_no_head_start() -> Result<(), Box<dyn Error>> {
    let from_file = resolve_toml(
        r#"
[accept]
ceiling = { pace = "1x" }
"#,
    )??;
    assert_eq!(
        default_policy(&from_file).await?.ceiling,
        Some(Ceiling {
            pace: Pace::realtime(),
            burst: Duration::ZERO,
        })
    );

    let from_flag = resolve_with("", &["--accept-ceiling-pace", "1x"], &[])??;
    assert_eq!(
        default_policy(&from_flag).await?.ceiling,
        Some(Ceiling {
            pace: Pace::realtime(),
            burst: Duration::ZERO,
        })
    );
    Ok(())
}

#[tokio::test]
async fn ceiling_pace_and_burst_flatten_to_cli_flags() -> Result<(), Box<dyn Error>> {
    let config = resolve_with(
        "",
        &[
            "--accept-ceiling-pace",
            "1x",
            "--accept-ceiling-burst",
            "10s",
        ],
        &[],
    )??;
    assert_eq!(
        default_policy(&config).await?.ceiling,
        Some(Ceiling {
            pace: Pace::realtime(),
            burst: Duration::from_secs(10),
        })
    );
    Ok(())
}

#[test]
fn a_ceiling_burst_without_pace_is_refused() -> Result<(), Box<dyn Error>> {
    let error = resolve_with("", &["--accept-ceiling-burst", "10s"], &[])?
        .err()
        .ok_or("burst without pace must fail")?;
    assert!(
        error.to_string().contains("pace"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn floor_pace_and_window_flatten_to_cli_flags() -> Result<(), Box<dyn Error>> {
    let from_file = resolve_toml(
        r#"
[accept]
floor = { pace = "0.5x", window = "30s" }
"#,
    )??;
    let expected = Floor {
        pace: Pace::new(nz::u32!(1), nz::u32!(2)),
        window: Duration::from_secs(30),
    };
    assert_eq!(default_policy(&from_file).await?.floor, Some(expected));

    let from_flag = resolve_with(
        "",
        &[
            "--accept-floor-pace",
            "0.5x",
            "--accept-floor-window",
            "30s",
        ],
        &[],
    )??;
    assert_eq!(default_policy(&from_flag).await?.floor, Some(expected));
    Ok(())
}

#[test]
fn a_floor_pace_without_a_window_is_refused() -> Result<(), Box<dyn Error>> {
    let error = resolve_with("", &["--accept-floor-pace", "0.5x"], &[])?
        .err()
        .ok_or("pace without window must fail")?;
    assert!(
        error.to_string().contains("window"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn open_authentication_uses_the_builtin_policy_when_http_is_omitted()
-> Result<(), Box<dyn Error>> {
    let config = resolve_toml("")??;

    let grant = config
        .authenticator
        .authenticate(&request("ignored"))
        .await?;
    assert_eq!(
        grant.policy,
        StreamPolicy {
            takeovers: TakeoverPolicy::Deny,
            input_mode: crate::domain::InputMode::Strict,
            ..StreamPolicy::permissive()
        }
    );
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
    let result = resolve_with_env(&[("RUSHLS_HTTP_CORS_CREDENTIALS", "true")])?;

    assert!(matches!(result, Err(ConfigError::Invalid(_))));
    Ok(())
}

#[test]
fn metrics_endpoint_and_authentication_resolve_from_configuration() -> Result<(), Box<dyn Error>> {
    let config = resolve_with_env(&[
        ("RUSHLS_METRICS_LISTEN", "127.0.0.1:9090"),
        ("RUSHLS_METRICS_TOKEN", "scrape-secret"),
    ])??;

    assert!(config.node.metrics.listen.is_some());
    assert!(config.node.metrics.token.is_some());
    Ok(())
}

#[test]
fn a_metrics_per_stream_flag_is_no_longer_accepted() -> Result<(), Box<dyn Error>> {
    let result = load_toml(
        r#"
[metrics]
listen = "127.0.0.1:9090"
per_stream = true
"#,
    )?;
    assert!(
        result.is_err(),
        "cardinality is the scraper's choice of /metrics vs /metrics/streams"
    );
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
{BASE_CONFIG}

[srt]
passphrase_file = "{}"

[metrics]
token_file = "{}"
"#,
        fixtures::toml_path_contents(&secret.path),
        fixtures::toml_path_contents(&secret.path),
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
{BASE_CONFIG}

[metrics]
token = "inline"
token_file = "{}"
"#,
        fixtures::toml_path_contents(&secret.path),
    ))?;

    assert!(matches!(
        result,
        Err(ConfigError::Loading(
            rushls_config::ConfigError::SecretConflict(_)
        ))
    ));
    Ok(())
}

#[test]
fn compiled_defaults_leave_moq_off() -> Result<(), Box<dyn Error>> {
    // Certificates are required to bind WebTransport, so the compiled default
    // must boot without them.
    let resolved = AppConfig::load_and_resolve_from(os(["rushls"]), std::iter::empty())?;
    assert_eq!(resolved.node.moq_address, None);
    assert!(resolved.node.moq.tls.is_none());
    Ok(())
}

#[test]
fn a_moq_listener_without_certificates_is_refused() -> Result<(), Box<dyn Error>> {
    let Err(error) = resolve_toml(
        r#"
[moq]
listen = "127.0.0.1:4433"
"#,
    )?
    else {
        panic!("MOQ listen without a certificate pair must be refused");
    };

    assert!(
        error.to_string().contains("certificate"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[test]
fn a_moq_listener_with_certificates_resolves() -> Result<(), Box<dyn Error>> {
    let directory = scratch("moq-listen");
    let (settings, _) = write_pair(&directory, "origin.internal");
    let resolved = resolve_toml(&format!(
        r#"
[rtmp]
listen = "127.0.0.1:1935"

[srt]
listen = "127.0.0.1:9000"

[moq]
listen = "127.0.0.1:4433"
cert = "{}"
key = "{}"
timeout = "8s"
"#,
        fixtures::toml_path_contents(&settings.certificate),
        fixtures::toml_path_contents(&settings.key)
    ))?
    .unwrap_or_else(|error| panic!("MOQ with certificates must resolve: {error}"));

    assert_eq!(
        resolved.node.moq_address,
        Some("127.0.0.1:4433".parse().expect("constant is valid"))
    );
    assert_eq!(resolved.node.moq.idle_timeout, Some(Duration::from_secs(8)));
    assert_eq!(
        resolved.node.moq.handshake_timeout,
        Duration::from_secs(5),
        "an established timeout still leaves SETUP bounded"
    );
    let tls = resolved.node.moq.tls.expect("MOQ TLS is configured");
    assert_eq!(tls.certificate, settings.certificate);
    assert_eq!(tls.key, settings.key);
    Ok(())
}

#[test]
fn a_public_moq_listener_with_open_auth_is_warned() -> Result<(), Box<dyn Error>> {
    let directory = scratch("moq-public");
    let (settings, _) = write_pair(&directory, "origin.internal");
    let resolved = resolve_toml(&format!(
        r#"
[rtmp]
listen = "127.0.0.1:1935"

[srt]
listen = "127.0.0.1:9000"

[moq]
listen = "0.0.0.0:4433"
cert = "{}"
key = "{}"
"#,
        fixtures::toml_path_contents(&settings.certificate),
        fixtures::toml_path_contents(&settings.key)
    ))?
    .unwrap_or_else(|error| panic!("public MOQ must still resolve: {error}"));

    assert!(
        resolved
            .warnings
            .iter()
            .any(|warning| warning.contains("public address") && warning.contains("[auth.publish]")),
        "open publish on a public MOQ bind should not be invisible: {:?}",
        resolved.warnings
    );
    Ok(())
}

#[test]
fn a_disabled_moq_timeout_still_bounds_the_handshake() -> Result<(), Box<dyn Error>> {
    let directory = scratch("moq-timeout-off");
    let (settings, _) = write_pair(&directory, "origin.internal");
    let resolved = resolve_toml(&format!(
        r#"
[rtmp]
listen = "127.0.0.1:1935"

[srt]
listen = "127.0.0.1:9000"

[moq]
listen = "127.0.0.1:4433"
cert = "{}"
key = "{}"
timeout = "off"
"#,
        fixtures::toml_path_contents(&settings.certificate),
        fixtures::toml_path_contents(&settings.key)
    ))?
    .unwrap_or_else(|error| panic!("a disabled MOQ timeout must resolve: {error}"));

    assert_eq!(resolved.node.moq.idle_timeout, None);
    assert_eq!(resolved.node.moq.handshake_timeout, Duration::from_secs(5));
    assert!(
        resolved
            .warnings
            .iter()
            .any(|warning| warning.contains("MOQ timeout is disabled")),
        "an unbounded wait on a bound listener should not be invisible: {:?}",
        resolved.warnings
    );
    Ok(())
}

#[test]
fn a_moq_timeout_below_a_keyframe_interval_is_refused() -> Result<(), Box<dyn Error>> {
    let Err(error) = resolve_toml(
        r#"
[moq]
timeout = "500ms"
"#,
    )?
    else {
        panic!("a sub-second MOQ idle must be refused");
    };

    assert!(
        error.to_string().contains("below one second"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[test]
fn tls_requires_both_the_certificate_and_key() -> Result<(), Box<dyn Error>> {
    let result = load_with(
        BASE_CONFIG,
        &[],
        &[("RUSHLS_HTTP_TLS_CERT", "/tmp/certificate.pem")],
    )?;

    assert!(result.is_err());
    Ok(())
}

#[test]
fn unknown_toml_keys_are_rejected() -> Result<(), Box<dyn Error>> {
    assert!(
        load_toml(
            r"
[capacity]
publishers = 10
maximum_concurrent_publisherz = 11
"
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
[ingest.rtmp.avformat]
io_buffer_size = "64KiB"
"#
        )?
        .is_err()
    );
    Ok(())
}

#[test]
fn file_values_interpolate_from_the_environment() -> Result<(), Box<dyn Error>> {
    let resolved = resolve_with(
        r#"
name = "${NODE_NAME}"

[auth.publish]
url = "http://${AUTH_HOST}/admit"
token = "${AUTH_TOKEN}"
"#,
        &[],
        &[
            ("NODE_NAME", "origin-7"),
            ("AUTH_HOST", "auth.internal:8081"),
            ("AUTH_TOKEN", "s3cret"),
        ],
    )??;

    assert_eq!(&*resolved.node.name, "origin-7");
    Ok(())
}

#[test]
fn an_undefined_variable_stops_startup() -> Result<(), Box<dyn Error>> {
    // The failure that matters most: substituting an empty string into a token
    // would silently disable the check it was protecting.
    let error = resolve_with(r#"name = "${MISSING}""#, &[], &[])?
        .err()
        .ok_or("an undefined variable is refused")?
        .to_string();

    assert!(error.contains("MISSING"), "{error}");
    Ok(())
}

#[test]
fn interpolation_composes_with_an_environment_override() -> Result<(), Box<dyn Error>> {
    // Interpolation runs on the file before overrides apply, so the two
    // compose rather than compete: the file resolves its reference, and a
    // RUSHLS_ override still replaces the result.
    let resolved = resolve_with(
        r#"name = "${NODE_NAME}""#,
        &[],
        &[("NODE_NAME", "from-file"), ("RUSHLS_NAME", "from-override")],
    )??;

    assert_eq!(&*resolved.node.name, "from-override");
    Ok(())
}

#[tokio::test]
async fn mutual_tls_material_resolves_for_the_admission_service() -> Result<(), Box<dyn Error>> {
    // The admission service may widen what this node accepts, so on an
    // untrusted network it should be authenticated by more than a bearer
    // token -- and this node should prove itself to it in return.
    let directory = scratch("auth-mtls");
    let (settings, _) = write_pair(&directory, "origin.internal");

    let configuration = format!(
        r#"
[auth.publish]
url = "https://auth.internal/admit"
client_cert = "{}"
client_key = "{}"
ca = "{}"
"#,
        fixtures::toml_path_contents(&settings.certificate),
        fixtures::toml_path_contents(&settings.key),
        fixtures::toml_path_contents(&settings.certificate),
    );

    let resolved = resolve_toml(&configuration)??;
    assert_eq!(
        resolved.outbound_tls.len(),
        1,
        "the material is held for its rotation watch, not dropped once the \
         client is built"
    );
    Ok(())
}

#[tokio::test]
async fn half_a_client_certificate_pair_is_refused() -> Result<(), Box<dyn Error>> {
    // Presenting a certificate needs its key, and a key alone proves nothing.
    // Refused rather than ignored: a node that silently presented no identity
    // would be rejected by the service later, with nothing here to explain it.
    let directory = scratch("auth-mtls-half");
    let (settings, _) = write_pair(&directory, "origin.internal");

    for (line, missing) in [
        (
            format!(
                "client_cert = \"{}\"",
                fixtures::toml_path_contents(&settings.certificate)
            ),
            "client_key",
        ),
        (
            format!(
                "client_key = \"{}\"",
                fixtures::toml_path_contents(&settings.key)
            ),
            "client_cert",
        ),
    ] {
        let error = resolve_toml(&format!(
            "[auth.publish]\nurl = \"https://auth.internal/admit\"\n{line}\n"
        ))?
        .err()
        .ok_or("half a pair is refused")?
        .to_string();
        assert!(error.contains(missing), "{error}");
    }
    Ok(())
}

#[tokio::test]
async fn an_unreadable_client_certificate_stops_startup() -> Result<(), Box<dyn Error>> {
    let directory = scratch("auth-mtls-missing");
    let error = resolve_toml(&format!(
        r#"
[auth.publish]
url = "https://auth.internal/admit"
client_cert = "{}"
client_key = "{}"
"#,
        fixtures::toml_path_contents(&directory.join("absent.pem")),
        fixtures::toml_path_contents(&directory.join("absent.key")),
    ))?
    .err()
    .ok_or("an unreadable identity is refused")?
    .to_string();

    assert!(error.contains("[auth.publish]"), "{error}");
    Ok(())
}

#[tokio::test]
async fn a_hook_may_present_its_own_identity() -> Result<(), Box<dyn Error>> {
    // A hook endpoint is as much an operator-run service as the admission one,
    // reached over the same networks, so it gets the same three fields. The
    // difference is that hooks share one client by default: only a destination
    // configuring material of its own is given a pool of its own, because a
    // pooled connection would present one service's certificate to another.
    let directory = scratch("hook-mtls");
    let (settings, _) = write_pair(&directory, "origin.internal");

    let resolved = resolve_toml(&format!(
        r#"
[hook.archive]
url = "https://archive.internal/rushls"
events = ["session.started"]
client_cert = "{}"
client_key = "{}"
ca = "{}"
"#,
        fixtures::toml_path_contents(&settings.certificate),
        fixtures::toml_path_contents(&settings.key),
        fixtures::toml_path_contents(&settings.certificate),
    ))??;

    assert_eq!(
        resolved.outbound_tls.len(),
        1,
        "held for its rotation watch, exactly as the admission material is"
    );
    let hooks = resolved.hooks.ok_or("the hook resolves")?;
    assert!(
        hooks.config.hooks[0].client.is_some(),
        "a destination that asked for an identity gets its own pool"
    );
    Ok(())
}

#[tokio::test]
async fn a_hook_without_tls_material_shares_the_process_client() -> Result<(), Box<dyn Error>> {
    let resolved = resolve_toml(
        r#"
[hook.automation]
url = "http://automation.internal/rushls"
events = ["session.started"]
"#,
    )??;

    let hooks = resolved.hooks.ok_or("the hook resolves")?;
    assert!(
        hooks.config.hooks[0].client.is_none(),
        "the ordinary case pays for one TLS setup and one connection cache"
    );
    assert!(resolved.outbound_tls.is_empty());
    Ok(())
}

#[tokio::test]
async fn half_a_client_certificate_pair_is_refused_for_a_hook_too() -> Result<(), Box<dyn Error>> {
    // Named by hook, so an operator running several knows which to fix.
    let directory = scratch("hook-mtls-half");
    let (settings, _) = write_pair(&directory, "origin.internal");

    let error = resolve_toml(&format!(
        r#"
[hook.archive]
url = "https://archive.internal/rushls"
events = ["session.started"]
client_cert = "{}"
"#,
        fixtures::toml_path_contents(&settings.certificate),
    ))?
    .err()
    .ok_or("half a pair is refused")?
    .to_string();

    assert!(error.contains("archive"), "{error}");
    assert!(error.contains("client_key"), "{error}");
    Ok(())
}

#[test]
fn keys_for_unbuilt_features_are_refused_by_name() -> Result<(), Box<dyn Error>> {
    // The reference describes several features this node does not have. Their
    // keys must fail startup rather than be accepted and do nothing, which is
    // the silent-misconfiguration failure the whole design is against.
    for configuration in [
        // Mutual TLS to the admission service.
        "[auth.publish]\nurl = \"http://auth\"\nclient_cert = \"/x.pem\"\n",
        // Payload-carrying hooks.
        "[hook.archive]\nurl = \"http://archive\"\nevents = [\"session.started\"]\npayload = true\n",
    ] {
        assert!(
            resolve_toml(configuration)?.is_err(),
            "expected a startup error for:\n{configuration}"
        );
    }
    Ok(())
}

#[test]
fn disk_tier_defaults_the_directory_and_refuses_a_path_without_a_cap() -> Result<(), Box<dyn Error>>
{
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/rushls-disk-config-test");
    let both = format!(
        "[capacity]\ndisk_per_stream = \"8GiB\"\ndir = \"{}\"\n",
        fixtures::toml_path_contents(&dir)
    );
    let config = resolve_toml(&both)??;
    let disk = config
        .node
        .store
        .disk
        .as_ref()
        .ok_or("disk tier should be configured")?;
    assert_eq!(disk.maximum_payload_bytes, 8 * 1024 * 1024 * 1024_usize);
    assert_eq!(disk.directory, dir);

    let defaulted = resolve_toml("[capacity]\ndisk_per_stream = \"8GiB\"\n")??;
    let defaulted = defaulted
        .node
        .store
        .disk
        .as_ref()
        .ok_or("disk_per_stream alone should use the cache directory")?;
    assert_eq!(
        defaulted.maximum_payload_bytes,
        8 * 1024 * 1024 * 1024_usize
    );
    assert!(
        super::paths::is_under_cache_dir(&defaulted.directory),
        "default overflow is the platform cache, not a required dir key: {}",
        fixtures::toml_path_contents(&defaulted.directory)
    );

    for configuration in [
        "[capacity]\ndir = \"/var/lib/rushls\"\n",
        "[capacity]\ndisk_per_stream = \"0\"\ndir = \"/var/lib/rushls\"\n",
    ] {
        assert!(
            resolve_toml(configuration)?.is_err(),
            "expected a startup error for:\n{configuration}"
        );
    }
    Ok(())
}

#[test]
fn the_admission_budget_derives_from_the_publisher_budget() -> Result<(), Box<dyn Error>> {
    // A pending admission is a precursor to an ingest session, so the
    // publisher budget is its parent. The stream budget deliberately is not: a
    // node holding many retained streams has no reason to accept more
    // unauthenticated sockets.
    let config = resolve_toml(
        r"
[capacity]
publishers = 40
",
    )??;

    assert_eq!(config.node.maximum_sessions, 40);
    assert_eq!(
        config.node.maximum_pending_publishers_per_listener(),
        40,
        "counted per listener, so the process-wide ceiling is this times the \
         number of ingest transports"
    );
    Ok(())
}
#[test]
fn configured_http_auth_resolves_and_guards_its_own_settings() -> Result<(), Box<dyn Error>> {
    let valid = r#"
[auth.publish]
url = "http://auth-sidecar:8081/v1/publish/admit"
max_response_bytes = "64KiB"
"#;
    assert!(
        resolve_toml(valid)?.is_ok(),
        "a minimal http auth table works"
    );

    // The internal admission deadline is 10s. A longer request timeout never
    // takes effect: the session gives up first and blames a stage rather than
    // the service that did not answer.
    let outlives_admission = r#"
[auth.publish]
url = "http://auth-sidecar:8081/admit"
timeout = "30s"
"#;
    // A response may only name a policy this node actually has.
    let unknown_default = r#"
[auth.publish]
url = "http://auth-sidecar:8081/admit"
default_policy = "nonexistent"
    "#;
    let unusable_url = r#"
[auth.publish]
url = "not-a-url"
"#;
    for configuration in [outlives_admission, unknown_default, unusable_url] {
        assert!(
            resolve_toml(configuration)?.is_err(),
            "expected a startup error for:{configuration}"
        );
    }
    assert!(resolve_toml("[auth]\n")?.is_ok());
    Ok(())
}

#[test]
fn playback_auth_requires_one_key_source_and_iss_aud() -> Result<(), Box<dyn Error>> {
    let hmac = r#"
[auth.playback]
secret = "playback-hmac-secret"
[auth.playback.claims]
iss = "https://issuer.example"
aud = "rushls-origin"
tier = "premium"
n = 7
ok = true
"#;
    let resolved = resolve_toml(hmac)??;
    let playback = resolved
        .playback
        .as_ref()
        .ok_or("playback authorization should resolve")?;
    assert_eq!(playback.issuer, "https://issuer.example");
    assert_eq!(playback.audience, "rushls-origin");
    assert_eq!(playback.stream_claim, "stream");
    assert_eq!(playback.leeway, Duration::from_secs(30));
    assert_eq!(
        playback.extra.get("tier"),
        Some(&crate::server::http::playback::ClaimValue::String(
            "premium".into()
        ))
    );
    assert_eq!(
        playback.extra.get("n"),
        Some(&crate::server::http::playback::ClaimValue::Integer(7))
    );
    assert_eq!(
        playback.extra.get("ok"),
        Some(&crate::server::http::playback::ClaimValue::Boolean(true))
    );
    assert!(matches!(
        playback.keys,
        crate::server::http::playback::PlaybackKeyMaterial::Secret(_)
    ));

    let jwks = r#"
[auth.playback]
jwks_url = "https://issuer.example/.well-known/jwks.json"
[auth.playback.claims]
iss = "https://issuer.example"
aud = "rushls-origin"
"#;
    let jwks_resolved = resolve_toml(jwks)??;
    assert!(
        matches!(
            jwks_resolved.playback.unwrap().keys,
            crate::server::http::playback::PlaybackKeyMaterial::Jwks { .. }
        ),
        "a JWKS URL is accepted at resolve without being fetched"
    );

    for configuration in [
        "[auth.playback]\nsecret = \"shh\"\n",
        r#"
[auth.playback]
secret = "shh"
jwks_url = "https://issuer.example/jwks.json"
[auth.playback.claims]
iss = "https://issuer.example"
aud = "rushls-origin"
"#,
        r#"
[auth.playback]
secret = ""
[auth.playback.claims]
iss = "https://issuer.example"
aud = "rushls-origin"
"#,
        r#"
[auth.playback]
secret = "shh"
[auth.playback.claims]
iss = 1
aud = "rushls-origin"
"#,
        r#"
[auth.playback]
secret = "shh"
jwks_url = "https://issuer.example/jwks.json"
public_key = "-----BEGIN PUBLIC KEY-----\nMIIB\n-----END PUBLIC KEY-----"
[auth.playback.claims]
iss = "https://issuer.example"
aud = "rushls-origin"
"#,
    ] {
        assert!(
            resolve_toml(configuration)?.is_err(),
            "expected a startup error for:\n{configuration}"
        );
    }
    Ok(())
}

#[test]
fn http_auth_is_selected_by_the_optional_table() -> Result<(), Box<dyn Error>> {
    let configuration = r#"
[auth.publish]
url = "http://auth-sidecar:8081/admit"
"#;
    assert!(resolve_toml(configuration)?.is_ok());
    assert!(resolve_toml("[auth.open]\n")?.is_err());
    Ok(())
}

#[test]
fn hooks_are_absent_until_an_endpoint_is_configured() -> Result<(), Box<dyn Error>> {
    let resolved = resolve_toml("name = \"studio\"\n")??;

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
name = "studio"

[hook.automation]
url = "http://automation:9000/events"
events = ["session.started", "session.ended", "segment.ready"]
max_attempts = 2
"#,
    )??;

    let hooks = resolved.hooks.ok_or("hooks resolve")?;
    assert_eq!(
        hooks.config.source, "studio",
        "the node name is the producer identity: one field, not a second \
         spelling of the same node"
    );
    let hook = hooks.config.hooks.first().ok_or("one endpoint")?;
    assert_eq!(&*hook.name, "automation");
    assert_eq!(hook.maximum_attempts, 2);
    assert_eq!(
        hook.events,
        [Kind::SessionStarted, Kind::SessionEnded, Kind::SegmentReady]
            .into_iter()
            .collect(),
        "a subscription is exactly what was asked for, never widened"
    );
    Ok(())
}

#[test]
fn an_endpoint_that_could_never_deliver_is_rejected() -> Result<(), Box<dyn Error>> {
    let unknown_event = r#"
[hook.automation]
url = "http://automation:9000/events"
events = ["session.exploded"]
"#;
    let no_events = r#"
[hook.automation]
url = "http://automation:9000/events"
events = []
"#;
    let no_attempts = r#"
[hook.automation]
url = "http://automation:9000/events"
events = ["session.ended"]
max_attempts = 0
"#;
    let no_queue = r#"
[hook.automation]
url = "http://automation:9000/events"
events = ["session.ended"]
queue_size = 0
"#;
    let unusable_url = r#"
[hook.automation]
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
    let env: Vec<(OsString, OsString)> = environment
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect();
    Ok(load_with(configuration, arguments, environment)?
        .and_then(|config| config.resolve_from(env)))
}

/// Loads one TOML fragment on its own, for tests about the schema itself.
fn load_toml(configuration: &str) -> Result<Result<AppConfig, ConfigError>, Box<dyn Error>> {
    load_with(configuration, &[], &[])
}

#[test]
fn the_rtmp_timeout_derives_a_tighter_handshake_limit() -> Result<(), Box<dyn Error>> {
    // One operator-facing value. The unauthenticated phase is bounded by a
    // constant the session limit cannot loosen, because holding a socket open
    // before proving anything is the cheapest attack against an ingest node.
    let resolved = resolve_toml(
        r#"
[rtmp]
timeout = "15s"
"#,
    )?
    .unwrap_or_else(|error| panic!("a timeout alone must resolve: {error}"));

    let timeouts = resolved.node.rtmp.timeouts;
    assert_eq!(timeouts.session_read, Some(Duration::from_secs(15)));
    assert_eq!(timeouts.write, Some(Duration::from_secs(15)));
    assert_eq!(
        timeouts.handshake_read,
        Some(Duration::from_secs(5)),
        "a generous session timeout must not become a generous handshake one"
    );
    Ok(())
}

#[test]
fn a_tight_rtmp_timeout_also_tightens_the_handshake() -> Result<(), Box<dyn Error>> {
    // The derivation is the lesser of the two, so tightening the session
    // limit never leaves the unauthenticated phase the more patient one.
    let resolved = resolve_toml(
        r#"
[rtmp]
timeout = "2s"
"#,
    )?
    .unwrap_or_else(|error| panic!("a tight timeout must resolve: {error}"));

    let timeouts = resolved.node.rtmp.timeouts;
    assert_eq!(timeouts.handshake_read, Some(Duration::from_secs(2)));
    assert_eq!(timeouts.session_read, Some(Duration::from_secs(2)));
    Ok(())
}

#[test]
fn a_disabled_rtmp_timeout_still_bounds_the_handshake() -> Result<(), Box<dyn Error>> {
    // "No timeout" stays expressible for a trusted link, but it applies to the
    // publisher that proved itself, never to a peer that has not.
    let resolved = resolve_toml(
        r#"
[rtmp]
timeout = "off"
"#,
    )?
    .unwrap_or_else(|error| panic!("a disabled timeout must resolve: {error}"));

    let timeouts = resolved.node.rtmp.timeouts;
    assert_eq!(timeouts.session_read, None);
    assert_eq!(timeouts.write, None);
    assert_eq!(timeouts.handshake_read, Some(Duration::from_secs(5)));
    assert!(
        resolved
            .warnings
            .iter()
            .any(|warning| warning.contains("RTMP timeout is disabled")),
        "an unbounded wait on a public bind should not be invisible: {:?}",
        resolved.warnings
    );
    Ok(())
}
#[test]
fn the_stall_deadline_is_an_absolute_duration() -> Result<(), Box<dyn Error>> {
    // Absolute rather than a segment multiple: `stall` measures the publisher,
    // not the playlist, so it is the same question whatever cadence this node
    // happens to be packaging at.
    let resolved = resolve_toml(
        r#"
[accept]
stall = "20s"
"#,
    )?
    .unwrap_or_else(|error| panic!("a stall deadline must resolve: {error}"));

    assert_eq!(
        resolved.node.session.supervision.health.stall,
        Duration::from_secs(20)
    );
    Ok(())
}

#[test]
fn a_stall_shorter_than_one_segment_is_refused() -> Result<(), Box<dyn Error>> {
    // A deadline inside one segment fails a publisher that is merely between
    // keyframes, which is hardening turned into an outage.
    let Err(error) = resolve_toml(
        r#"
[hls]
segment = { target = "10s" }

[accept]
stall = "5s"
"#,
    )?
    else {
        panic!("a stall inside one segment must be refused");
    };

    assert!(
        error.to_string().contains("segment duration"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[test]
fn a_stall_may_be_disabled_on_a_trusted_link() -> Result<(), Box<dyn Error>> {
    let resolved = resolve_toml(
        r#"
[accept]
stall = "off"
"#,
    )?
    .unwrap_or_else(|error| panic!("a disabled stall must resolve: {error}"));

    assert_eq!(
        resolved.node.session.supervision.health.stall,
        Duration::MAX
    );
    assert!(
        resolved
            .warnings
            .iter()
            .any(|warning| warning.contains("accept.stall is off")),
        "an unbounded wait should not be invisible: {:?}",
        resolved.warnings
    );
    Ok(())
}
#[test]
fn a_stall_deadline_shorter_than_its_sampling_is_refused() -> Result<(), Box<dyn Error>> {
    // Sampling cannot observe a deadline shorter than its own period, so such
    // a value is not the tighter detection it appears to be.
    let Err(error) = resolve_toml(
        r#"
[accept]
stall = "500ms"
"#,
    )?
    else {
        panic!("an unobservable stall deadline must be refused");
    };

    assert!(
        error.to_string().contains("cannot be observed"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[test]
fn an_srt_idle_deadline_inside_the_latency_window_is_refused() -> Result<(), Box<dyn Error>> {
    // Firing inside the receiver's own reorder window would drop packets the
    // transport is still legitimately waiting for.
    let Err(error) = resolve_toml(
        r#"
[srt]
latency = "2s"
timeout = "1s"
"#,
    )?
    else {
        panic!("an idle deadline inside the latency window must be refused");
    };

    assert!(
        error
            .to_string()
            .contains("must exceed the receive latency"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[test]
fn tls_admission_limits_are_configurable() -> Result<(), Box<dyn Error>> {
    // The unauthenticated edge: the cost a peer can impose is the product of
    // these two, so both have to be reachable.
    let certificate = TempConfig::new("certificate")?;
    let key = TempConfig::new("key")?;
    let resolved = resolve_toml(&format!(
        r#"
[http.tls]
cert = "{}"
key = "{}"
handshake_timeout = "2s"
max_handshakes = 32
"#,
        fixtures::toml_path_contents(&certificate.path),
        fixtures::toml_path_contents(&key.path)
    ))?
    .unwrap_or_else(|error| panic!("TLS admission limits must resolve: {error}"));

    let tls = resolved.node.http.tls.expect("TLS is configured");
    assert_eq!(tls.handshake_timeout, Duration::from_secs(2));
    assert_eq!(tls.maximum_pending_handshakes, 32);
    Ok(())
}

#[test]
fn tls_version_bounds_resolve_and_reject_invalid_ranges() -> Result<(), Box<dyn Error>> {
    use rushls_tls::TlsVersion::{Tls12, Tls13};
    let certificate = TempConfig::new("certificate")?;
    let key = TempConfig::new("key")?;
    let base = format!(
        "[http.tls]\ncert = {:?}\nkey = {:?}\n",
        certificate.path, key.path
    );
    for (toml, args, env, expected) in [
        (base.clone(), vec![], vec![], (Tls13, Tls13)),
        (
            format!("{base}version = {{ min = \"1.2\" }}\n"),
            vec![],
            vec![],
            (Tls12, Tls13),
        ),
        (
            format!("{base}version = {{ min = \"1.2\", max = \"1.2\" }}\n"),
            vec![],
            vec![],
            (Tls12, Tls12),
        ),
        (
            base.clone(),
            vec!["--http-tls-version-min=1.2", "--http-tls-version-max=1.2"],
            vec![],
            (Tls12, Tls12),
        ),
        (
            base.clone(),
            vec![],
            vec![
                ("RUSHLS_HTTP_TLS_VERSION_MIN", "1.2"),
                ("RUSHLS_HTTP_TLS_VERSION_MAX", "1.2"),
            ],
            (Tls12, Tls12),
        ),
        (
            format!("{base}version = {{ min = \"1.2\" }}\n"),
            vec!["--http-tls-version-min=1.3"],
            vec![],
            (Tls13, Tls13),
        ),
    ] {
        let resolved = resolve_with(&toml, &args, &env)??;
        let tls = resolved.node.http.tls.unwrap();
        assert_eq!((tls.min_version, tls.max_version), expected);
    }
    let invalid = resolve_toml(&format!("{base}version = {{ max = \"1.2\" }}\n"))?;
    assert!(
        matches!(invalid, Err(ConfigError::Invalid(message)) if message.contains("http.tls.version.min must not exceed http.tls.version.max"))
    );
    for version in ["1.0", "1.1", "1.4", "garbage"] {
        assert!(load_toml(&format!("{base}version = {{ min = {version:?} }}\n"))?.is_err());
    }
    Ok(())
}

#[test]
fn an_rtmp_timeout_below_a_keyframe_interval_is_refused() -> Result<(), Box<dyn Error>> {
    // Hardening that drops legitimate publishers is an outage, not a defence.
    let Err(error) = resolve_toml(
        r#"
[rtmp]
timeout = "500ms"
"#,
    )?
    else {
        panic!("a sub-second session read must be refused");
    };

    assert!(
        error.to_string().contains("below one second"),
        "unexpected error: {error}"
    );
    Ok(())
}

/// Loads and resolves one TOML fragment on its own.
fn resolve_toml(
    configuration: &str,
) -> Result<Result<ResolvedAppConfig, ConfigError>, Box<dyn Error>> {
    resolve_with(configuration, &[], &[])
}

async fn default_policy(config: &ResolvedAppConfig) -> Result<StreamPolicy, Box<dyn Error>> {
    Ok(config
        .authenticator
        .authenticate(&request("ignored"))
        .await?
        .policy)
}

/// Resolves environment overrides against a minimal node.
fn resolve_with_env(
    environment: &[(&str, &str)],
) -> Result<Result<ResolvedAppConfig, ConfigError>, Box<dyn Error>> {
    resolve_with(BASE_CONFIG, &[], environment)
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

#[test]
fn playlist_windows_parse_as_durations_or_target_multiples() -> Result<(), Box<dyn Error>> {
    assert_eq!(
        parse_duration_rule("6x")?,
        DurationRule::MultipleOfTarget(TargetDurationMultiple::integer(6))
    );
    assert_eq!(
        parse_duration_rule("1.5x")?,
        DurationRule::MultipleOfTarget(TargetDurationMultiple::new(3, nz::u32!(2)))
    );
    assert_eq!(
        parse_duration_rule("18s")?,
        DurationRule::Fixed(Duration::from_secs(18))
    );
    assert!(parse_duration_rule("0x").is_err(), "zero is refused");
    assert!(parse_duration_rule("x").is_err(), "bare suffix is refused");
    assert!(
        parse_duration_rule("1.2.3x").is_err(),
        "one decimal point only"
    );
    assert!(parse_duration_rule("abc").is_err(), "garbage is refused");
    Ok(())
}

#[test]
fn decimal_fractions_reduce_exactly() -> Result<(), Box<dyn Error>> {
    assert_eq!(decimal_fraction("6")?, (6, 1));
    assert_eq!(decimal_fraction("1.5")?, (3, 2));
    assert_eq!(decimal_fraction("0.5")?, (1, 2));
    assert_eq!(
        decimal_fraction("0.333333333")?,
        (333_333_333, 1_000_000_000)
    );
    assert!(
        decimal_fraction("0.3333333333").is_err(),
        "ninth digit is the ceiling"
    );
    Ok(())
}

#[test]
fn hold_back_accepts_both_forms_and_refuses_a_stalling_one() -> Result<(), Box<dyn Error>> {
    // The multiple form tracks a retuned part cadence, which is why it is the
    // default; the absolute exists for a deployment that pins latency itself.
    let relative = resolve_toml(
        r#"
[hls]
part = { target = "1s" }
hold_back = "4x"
"#,
    )??;
    assert_eq!(
        relative
            .node
            .hls
            .timing
            .part_hold_back
            .resolve(Duration::from_secs(1)),
        Duration::from_secs(4)
    );

    let absolute = resolve_toml(
        r#"
[hls]
part = { target = "1s", max = "1x" }
hold_back = "2500ms"
"#,
    )??;
    assert_eq!(
        absolute
            .node
            .hls
            .timing
            .part_hold_back
            .resolve(Duration::from_secs(1)),
        Duration::from_millis(2_500)
    );

    // Below two parts a client runs out of buffered media on any loss, which
    // is a latency the protocol cannot deliver rather than an aggressive one.
    let error = resolve_toml(
        r#"
[hls]
part = { target = "1s" }
hold_back = "1s"
"#,
    )?
    .err()
    .ok_or("a hold-back under two parts is refused")?
    .to_string();
    assert!(error.contains("hold_back"), "{error}");
    Ok(())
}

#[test]
fn a_hold_back_between_two_and_three_parts_is_warned_rather_than_refused()
-> Result<(), Box<dyn Error>> {
    // Two thresholds because the specification has two. Three parts is a
    // SHOULD, so a deployment on a good network may legitimately want the
    // lower latency and only needs telling; two is a MUST and stays refused.
    let resolved = resolve_toml(
        r#"
[hls]
part = { target = "1s", max = "1x" }
hold_back = "2s"
"#,
    )??;

    assert_eq!(
        resolved
            .node
            .hls
            .timing
            .part_hold_back
            .resolve(Duration::from_secs(1)),
        Duration::from_secs(2),
        "the operator's value is honoured, not raised to the recommendation"
    );
    assert!(
        resolved
            .warnings
            .iter()
            .any(|warning| warning.contains("hold_back")),
        "{:?}",
        resolved.warnings
    );
    Ok(())
}

#[test]
fn the_playlist_window_becomes_the_retention_window() -> Result<(), Box<dyn Error>> {
    let config = resolve_toml(
        r#"
[hls]
segment = { target = "6s", max = "1x" }
retain = "18s"
"#,
    )??;

    let retention = config.node.store.retention;
    assert_eq!(
        retention.retain.resolve(Duration::from_secs(3)),
        Duration::from_secs(18)
    );
    Ok(())
}

#[test]
fn a_fixed_window_below_three_target_durations_is_refused() -> Result<(), Box<dyn Error>> {
    assert!(
        resolve_toml(
            r#"
[hls]
segment = { target = "6s" }
retain = "12s"
"#,
        )?
        .is_err(),
        "a live playlist must never drop below three times the target duration"
    );
    Ok(())
}

#[test]
fn a_reconnect_window_below_the_hold_back_is_refused() -> Result<(), Box<dyn Error>> {
    assert!(
        resolve_toml(
            r#"
[hls]
segment = { target = "6s" }

[capacity]
inactive_stream_retention = "5s"
"#,
        )?
        .is_err(),
        "retiring a stream after 5s strands a viewer holding 18s of HOLD-BACK"
    );
    assert!(
        resolve_toml(
            r#"
[capacity]
inactive_stream_retention = "0s"
"#,
        )?
        .is_err(),
        "a zero reconnect window retires every idle stream on the next tick"
    );
    Ok(())
}

#[test]
fn hostile_playlist_windows_never_panic() {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    for _ in 0..4_000 {
        let value = crate::test_fuzz::string(&mut state, 24);
        // A window either parses into a rule or is refused; both are fine.
        let _ = parse_duration_rule(&value);
    }
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

#[test]
fn an_unknown_enumerated_value_names_the_alternatives() -> Result<(), Box<dyn Error>> {
    // The point of these messages is that an operator who typos one does not
    // have to go and read the reference file to find out what was allowed.
    let key_length = load_toml(&format!("{BASE_CONFIG}\n[srt]\nencryption = \"aes999\"\n"))?
        .err()
        .ok_or("an unknown key length is refused")?
        .to_string();
    assert!(key_length.contains("aes128"), "{key_length}");
    assert!(key_length.contains("aes256"), "{key_length}");
    Ok(())
}

#[tokio::test]
async fn in_band_text_captions_are_enabled_per_policy() -> Result<(), Box<dyn Error>> {
    let config = resolve_toml(
        r#"
[auth]
[accept]
subtitles = { codecs = ["text", "webvtt"] }
"#,
    )??;

    let grant = config
        .authenticator
        .authenticate(&request("camera-key"))
        .await?;
    assert_eq!(
        grant.policy.subtitles.codecs,
        crate::admission::Codecs::OneOf(vec![Codec::Text, Codec::WebVtt])
    );

    // A codec name is valid for one media kind only: in-band text is not
    // something a video ladder can carry, and accepting it there would let a
    // misplaced entry silently widen what a publisher may send.
    let wrong_kind = resolve_toml(
        r#"
[accept]
video = { codecs = ["text"] }
"#,
    )?;
    assert!(matches!(wrong_kind, Err(ConfigError::Invalid(_))));
    Ok(())
}

#[test]
fn hls_objects_resolve_targets_maxima_and_symmetric_jitter() -> Result<(), Box<dyn Error>> {
    let bounded =
        resolve_toml("[hls]\nsegment = { target = '2s' }\npart = { target = '500ms' }\n")??;
    let policy = bounded.node.session.segmentation;
    assert_eq!(policy.maximum_part_duration, Duration::from_secs(1));
    assert_eq!(policy.segment_cap(), Duration::from_secs(4));
    assert_eq!(policy.late_boundary, Duration::ZERO);
    let explicit = resolve_toml(
        "[hls]\nsegment = { target = '2s', max = '1.5x', jitter = '0.05x' }\npart = { target = '500ms', max = '750ms' }\n",
    )??;
    let policy = explicit.node.session.segmentation;
    assert_eq!(policy.segment_cap(), Duration::from_secs(3));
    assert_eq!(policy.early_boundary, Duration::from_millis(100));
    assert_eq!(policy.late_boundary, Duration::from_millis(100));
    assert_eq!(policy.maximum_part_duration, Duration::from_millis(750));
    let nested = resolve_toml("[hls.segment]\nmax = '1x'\n[hls.part]\nmax = '1x'\n")??;
    assert_eq!(
        nested.node.session.segmentation.segment_cap(),
        Duration::from_secs(6)
    );
    assert_eq!(
        nested.node.session.segmentation.maximum_part_duration,
        Duration::from_secs(1)
    );
    for invalid in [
        "segment = '6s'",
        "part = '1s'",
        "admission = 'hard'",
        "maximum_segment = '2x'",
        "maximum_part = '2x'",
        "early_boundary = '0s'",
        "late_boundary = '0s'",
        "segment = { max = '1s' }",
        "segment = { target = '0s' }",
        "part = { target = '0s' }",
        "segment = { jitter = '1x' }",
        "part = { jitter = '0s' }",
        "part = { max = '20s' }",
    ] {
        assert!(
            resolve_toml(&format!("[hls]\n{invalid}\n"))?.is_err(),
            "{invalid}"
        );
    }
    assert!(
        super::resolve_hls_rule(
            DurationRule::MultipleOfTarget(TargetDurationMultiple::integer(2)),
            Duration::MAX
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn fixed_delivery_budgets_cover_the_entire_admission_range() -> Result<(), Box<dyn Error>> {
    assert!(resolve_toml("[hls]\npart = { target = '1s' }\nhold_back = '3s'\n")?.is_err());
    assert!(resolve_toml("[hls]\nsegment = { target = '6s' }\nretain = '18s'\n")?.is_err());
    assert!(resolve_toml("[hls]\nsegment = { max = '1x' }\npart = { max = '1x' }\nhold_back = '3s'\nretain = '18s'\n")?.is_ok());
    Ok(())
}

#[test]
fn nested_hls_sources_preserve_precedence() -> Result<(), Box<dyn Error>> {
    let resolved = resolve_with(
        "[hls]\nsegment = { target = '2s', max = '3x' }\npart = { target = '100ms' }\n",
        &["--hls-segment-target", "4s", "--hls-part-max", "4x"],
        &[
            ("RUSHLS_HLS_SEGMENT_TARGET", "3s"),
            ("RUSHLS_HLS_PART_TARGET", "200ms"),
        ],
    )??;
    let policy = resolved.node.session.segmentation;
    assert_eq!(policy.desired_segment_duration, Duration::from_secs(4));
    assert_eq!(policy.maximum_segment_duration, Duration::from_secs(12));
    assert_eq!(policy.desired_part_duration, Duration::from_millis(200));
    assert_eq!(policy.maximum_part_duration, Duration::from_millis(800));
    for flag in [
        "--hls-segment",
        "--hls-part",
        "--hls-admission",
        "--hls-maximum-segment",
        "--hls-maximum-part",
        "--hls-early-boundary",
        "--hls-late-boundary",
    ] {
        assert!(
            resolve_with("", &[flag, "1s"], &[])?.is_err(),
            "removed flag {flag}"
        );
    }
    Ok(())
}

#[test]
fn scrubbing_is_enabled_by_default_and_configurable() -> Result<(), Box<dyn Error>> {
    assert!(resolve_toml("")??.node.hls.playlist.iframe_playlists);
    assert!(
        load_shipped("rushls.example.toml")?
            .node
            .hls
            .playlist
            .iframe_playlists
    );
    assert!(
        !resolve_toml("[hls]\nscrubbing = false")??
            .node
            .hls
            .playlist
            .iframe_playlists
    );
    assert!(
        !resolve_with_env(&[("RUSHLS_HLS_SCRUBBING", "false")])??
            .node
            .hls
            .playlist
            .iframe_playlists
    );
    assert!(
        resolve_with("", &["--hls-scrubbing", "true"], &[])??
            .node
            .hls
            .playlist
            .iframe_playlists
    );
    assert!(
        !resolve_with(
            "[hls]\nscrubbing = true",
            &["--hls-scrubbing", "false"],
            &[]
        )??
        .node
        .hls
        .playlist
        .iframe_playlists
    );
    assert!(load_toml("[hls]\nscrubbing = 1")?.is_err());
    assert!(
        resolve_with("", &["--hls-scrubbing"], &[])??
            .node
            .hls
            .playlist
            .iframe_playlists
    );
    Ok(())
}

#[test]
fn recording_patterns_and_hook_signing_are_validated_at_startup() -> Result<(), Box<dyn Error>> {
    let resolved = resolve_toml(
        "[record]\ndir = '/archive'\npattern = '{stream}/{publication}/{time:%Y%m%d}/{rendition}_{segment}.mp4'\n",
    )??;
    assert_eq!(
        resolved
            .node
            .record
            .as_ref()
            .map(|record| record.queue_capacity),
        Some(128)
    );
    for config in [
        "[record]\ndir = 'https://archive'",
        "[record]\ndir = '/archive'\npattern = '{typo}.mp4'",
        "[record]\ndir = '/archive'\npattern = '../{stream}.mp4'",
        // The `.vtt` suffix of a subtitle rendition replaces everything after
        // the last dot, so this would name one file for every subtitle segment.
        "[record]\ndir = '/archive'\npattern = '{rendition}.{segment}'",
        "[record]\ndir = '/archive'\nqueue_size = 0",
        "[record]\ndir = '/archive'\nmax_pending_bytes = 0",
        "[record]\ndir = '/archive'\nunknown = 1",
        "[hook.test]\nurl = 'http://localhost'\nevents = ['session.started']\nsigning_secret = 'bad-secret'",
        "[hook.test]\nurl = 'http://localhost'\nevents = ['session.started']\nsigning_secret = 'bad-secret'\nsigning_secret_file = '/missing'",
    ] {
        assert!(!matches!(resolve_toml(config), Ok(Ok(_))), "{config}");
    }
    let signed = resolve_toml(
        "[hook.test]\nurl = 'http://localhost'\nevents = ['session.started']\nsigning_secret = 'whsec_BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc='\n",
    )??;
    assert!(signed.hooks.is_some());
    Ok(())
}

#[test]
fn secret_files_tolerate_a_trailing_newline() -> Result<(), Box<dyn Error>> {
    // Every ordinary way of writing a secret file -- `openssl rand -base64 32
    // > file`, a heredoc, a Kubernetes secret projection -- ends it with a
    // newline. base64 decoding and header parsing both reject that byte, so
    // the resolver has to absorb it rather than fail startup with a message
    // that blames the secret's format.
    let signing = TempConfig::new("whsec_BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=\n")?;
    let token = TempConfig::new("hunter2\n")?;
    let resolved = resolve_toml(&format!(
        "[hook.test]\nurl = 'http://localhost'\nevents = ['session.started']\nsigning_secret_file = '{}'\ntoken_file = '{}'\n",
        fixtures::toml_path_contents(&signing.path),
        fixtures::toml_path_contents(&token.path),
    ))??;
    assert!(resolved.hooks.is_some());

    // A file that is genuinely not a valid secret still fails, and still
    // names the format rather than the whitespace it once carried.
    let malformed = TempConfig::new("whsec_not+base64!\n")?;
    let Err(error) = resolve_toml(&format!(
        "[hook.test]\nurl = 'http://localhost'\nevents = ['session.started']\nsigning_secret_file = '{}'\n",
        fixtures::toml_path_contents(&malformed.path),
    ))?
    else {
        panic!("a malformed signing secret must be refused");
    };
    assert!(error.to_string().contains("base64"), "{error}");
    Ok(())
}

#[test]
fn http_capacity_is_configurable_and_must_be_positive() -> Result<(), Box<dyn Error>> {
    let defaults = resolve_toml("")??;
    assert_eq!(
        defaults.node.http.limits,
        crate::server::http::HttpLimits::default()
    );
    let config = resolve_toml("[http]\nmax_connections = 32\nmax_requests = 48\n")??;
    assert_eq!(config.node.http.limits.maximum_connections, 32);
    assert_eq!(config.node.http.limits.maximum_requests, 48);
    for field in ["max_connections", "max_requests"] {
        assert!(resolve_toml(&format!("[http]\n{field} = 0\n"))?.is_err());
    }
    Ok(())
}

#[test]
fn cors_exposed_headers_resolve_from_file_env_and_cli() -> Result<(), Box<dyn Error>> {
    let file = r#"[http.cors]
expose_headers = ["X-Request-Id", "Date"]
"#;
    let config = resolve_toml(file)??;
    assert_eq!(
        config.node.http.cors.expose_headers,
        ["x-request-id", "date"]
    );
    let env = [("RUSHLS_HTTP_CORS_EXPOSE_HEADERS", "x-trace-id, date")];
    let config = resolve_with(file, &[], &env)??;
    assert_eq!(config.node.http.cors.expose_headers, ["x-trace-id", "date"]);
    let config = resolve_with(
        file,
        &["--http-cors-expose-headers", "x-debug-id,date"],
        &env,
    )??;
    assert_eq!(config.node.http.cors.expose_headers, ["x-debug-id", "date"]);
    let config = resolve_toml("[http.cors]\nexpose_headers = []")??;
    assert!(config.node.http.cors.expose_headers.is_empty());
    let config = resolve_with(file, &["--http-cors-expose-headers="], &env)??;
    assert!(config.node.http.cors.expose_headers.is_empty());
    assert!(load_toml("[http.cors]\nexpose_headers = 'date'")?.is_err());
    let config = resolve_with_env(&[("RUSHLS_HTTP_CORS_EXPOSE_HEADERS", "")])??;
    assert!(config.node.http.cors.expose_headers.is_empty());
    let config = resolve_with_env(&[])??;
    assert_eq!(
        config.node.http.cors.expose_headers,
        ["content-length", "content-range", "date"]
    );
    Ok(())
}

#[test]
fn invalid_cors_exposed_headers_stop_startup() -> Result<(), Box<dyn Error>> {
    for value in [
        r#"["bad name"]"#,
        r#"["date,content-range"]"#,
        r#"[""]"#,
        r#"["*"]"#,
    ] {
        assert!(
            resolve_toml(&format!("[http.cors]\nexpose_headers = {value}"))?.is_err(),
            "{value}"
        );
    }
    for value in [
        "bad name",
        "x-header: value",
        "date,",
        "*",
        "x-id\r\nx-injected",
    ] {
        let result = resolve_with_env(&[("RUSHLS_HTTP_CORS_EXPOSE_HEADERS", value)])?;
        assert!(matches!(result, Err(ConfigError::Invalid(_))), "{value:?}");
    }
    Ok(())
}

#[tokio::test]
async fn strict_default_and_overrides() -> Result<(), Box<dyn Error>> {
    use crate::domain::InputMode;
    assert_eq!(
        default_policy(&resolve_toml("")??).await?.input_mode,
        InputMode::Strict
    );
    assert_eq!(
        default_policy(&resolve_toml("[accept]\nstrict = true")??)
            .await?
            .input_mode,
        InputMode::Strict
    );
    assert_eq!(
        default_policy(&resolve_with("", &["--accept-strict", "true"], &[])??)
            .await?
            .input_mode,
        InputMode::Strict
    );
    assert_eq!(
        default_policy(&resolve_with_env(&[("RUSHLS_ACCEPT_STRICT", "true")])??)
            .await?
            .input_mode,
        InputMode::Strict
    );
    let named: super::PolicyValue = toml::from_str("")?;
    assert_eq!(named.resolve("named")?.input_mode, InputMode::Strict);
    assert_eq!(
        toml::from_str::<super::PolicyValue>("strict = true")?
            .resolve("named")?
            .input_mode,
        InputMode::Strict
    );
    assert_eq!(
        default_policy(&resolve_with(
            "[accept]\nstrict = true",
            &["--accept-strict=false"],
            &[]
        )??)
        .await?
        .input_mode,
        InputMode::Permissive
    );
    assert!(resolve_toml("[accept]\nstrict = 'invalid'")?.is_err());
    Ok(())
}

const CONCISE_SETTINGS: &str = r#"
[http]
max_connections = 12
max_requests = 13
[http.cors]
credentials = true
[http.tls]
cert = "/tls.pem"
key = "/tls.key"
max_handshakes = 14
[moq]
cert = "/moq.pem"
[srt]
encryption = "aes128"
[auth.publish]
url = "http://auth"
timeout = "1s"
max_response_bytes = "32KiB"
client_cert = "/client.pem"
[hook.example]
url = "http://hook"
events = ["session.ended"]
queue_size = 15
max_in_flight = 2
max_attempts = 3
client_cert = "/hook.pem"
[record]
dir = "/archive"
queue_size = 16
max_pending_bytes = 1024
"#;

#[test]
fn concise_settings_match_file_environment_and_cli_names() -> Result<(), Box<dyn Error>> {
    // Loading is sufficient here: certificate paths are deliberately not read.
    // Resolution and certificate validation have their own integration coverage.

    let config = load_toml(CONCISE_SETTINGS)??;
    assert_eq!(config.http.max_connections, 12);
    assert_eq!(config.http.max_requests, 13);
    assert!(config.http.cors.credentials);
    assert_eq!(config.http.tls.as_ref().unwrap().max_handshakes, 14);
    assert_eq!(
        config.http.tls.as_ref().unwrap().cert,
        PathBuf::from("/tls.pem")
    );
    assert_eq!(config.moq.cert, Some(PathBuf::from("/moq.pem")));
    let auth = config.auth.publish.as_ref().unwrap();
    assert_eq!(auth.timeout, Duration::from_secs(1));
    assert_eq!(auth.client_cert, Some(PathBuf::from("/client.pem")));
    assert_eq!(
        super::nonzero_bytes("test", auth.max_response_bytes)?,
        32 * 1024
    );
    let hook = &config.hook.as_ref().unwrap().0["example"];
    assert_eq!(
        (hook.queue_size, hook.max_in_flight, hook.max_attempts),
        (15, 2, 3)
    );
    assert_eq!(hook.client_cert, Some(PathBuf::from("/hook.pem")));
    let record = &config.record.as_ref().unwrap().0;
    assert_eq!(
        (record.queue_capacity, record.maximum_pending_bytes),
        (16, 1024)
    );

    let environment = [
        ("RUSHLS_HTTP_MAX_CONNECTIONS", "21"),
        ("RUSHLS_HTTP_MAX_REQUESTS", "22"),
        ("RUSHLS_HTTP_CORS_CREDENTIALS", "false"),
        ("RUSHLS_HTTP_TLS_CERT", "/env.pem"),
        ("RUSHLS_HTTP_TLS_MAX_HANDSHAKES", "23"),
        ("RUSHLS_MOQ_CERT", "/env-moq.pem"),
        ("RUSHLS_AUTH_PUBLISH_CLIENT_CERT", "/env-client.pem"),
        ("RUSHLS_AUTH_PUBLISH_TIMEOUT", "500ms"),
        ("RUSHLS_AUTH_PUBLISH_MAX_RESPONSE_BYTES", "16KiB"),
        ("RUSHLS_SRT_ENCRYPTION", "aes256"),
    ];
    let env = load_with(CONCISE_SETTINGS, &[], &environment)??;
    assert_eq!((env.http.max_connections, env.http.max_requests), (21, 22));
    assert!(!env.http.cors.credentials);
    assert_eq!(env.http.tls.as_ref().unwrap().max_handshakes, 23);
    assert_eq!(
        env.http.tls.as_ref().unwrap().cert,
        PathBuf::from("/env.pem")
    );
    assert_eq!(env.moq.cert, Some(PathBuf::from("/env-moq.pem")));
    let auth = env.auth.publish.unwrap();
    assert_eq!(auth.client_cert, Some(PathBuf::from("/env-client.pem")));
    assert_eq!(auth.timeout, Duration::from_millis(500));
    assert_eq!(
        super::nonzero_bytes("test", auth.max_response_bytes)?,
        16 * 1024
    );

    let cli = load_with(
        CONCISE_SETTINGS,
        &[
            "--http-max-connections=31",
            "--http-max-requests=32",
            "--http-cors-credentials=true",
            "--http-tls-cert=/cli.pem",
            "--http-tls-max-handshakes=33",
            "--moq-cert=/cli-moq.pem",
            "--auth-publish-client-cert=/cli-client.pem",
            "--auth-publish-timeout=250ms",
            "--auth-publish-max-response-bytes=8KiB",
            "--srt-encryption=aes128",
        ],
        &environment,
    )??;
    assert_eq!((cli.http.max_connections, cli.http.max_requests), (31, 32));
    assert!(cli.http.cors.credentials);
    assert_eq!(cli.http.tls.as_ref().unwrap().max_handshakes, 33);
    assert_eq!(
        cli.http.tls.as_ref().unwrap().cert,
        PathBuf::from("/cli.pem")
    );
    assert_eq!(cli.moq.cert, Some(PathBuf::from("/cli-moq.pem")));
    let auth = cli.auth.publish.unwrap();
    assert_eq!(auth.client_cert, Some(PathBuf::from("/cli-client.pem")));
    assert_eq!(auth.timeout, Duration::from_millis(250));
    assert_eq!(
        super::nonzero_bytes("test", auth.max_response_bytes)?,
        8 * 1024
    );
    Ok(())
}

#[tokio::test]
async fn flac_is_an_audio_policy_codec() -> Result<(), Box<dyn Error>> {
    let config = resolve_toml("[auth]\n[accept.audio]\ncodecs = ['flac']\n")??;
    let grant = config
        .authenticator
        .authenticate(&request("flac-key"))
        .await?;
    assert_eq!(
        grant.policy.audio.codecs,
        crate::admission::Codecs::OneOf(vec![Codec::Flac])
    );
    assert!(matches!(
        resolve_toml("[accept.video]\ncodecs = ['flac']\n")?,
        Err(ConfigError::Invalid(_))
    ));
    assert_eq!(
        crate::domain::rfc6381(Codec::Flac, None).as_deref(),
        Some("fLaC")
    );
    Ok(())
}

#[test]
fn malformed_secret_documents_do_not_print_credentials() -> Result<(), Box<dyn Error>> {
    for text in [
        "[metrics]\ntoken = 918273645",
        "[auth.publish]\ntoken = 918273645",
        "[hook.test]\nurl = 'http://localhost'\nevents = ['session.started']\ntoken = 918273645",
        "[hook.test]\nurl = 'http://localhost'\nevents = ['session.started']\ntoken = 'sensitive-token'\nqueue_size = 'bad'",
    ] {
        let error = load_toml(text)?.err().ok_or("invalid document accepted")?;
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains("918273645"), "{diagnostic}");
        assert!(!diagnostic.contains("sensitive-token"), "{diagnostic}");
    }
    Ok(())
}

#[test]
fn strict_policies_configure_the_independence_contract() -> Result<(), Box<dyn Error>> {
    assert!(resolve_toml("")??.node.store.independent_segments);
    assert!(
        !resolve_toml("[accept]\nstrict = false")??
            .node
            .store
            .independent_segments
    );
    assert!(
        !resolve_toml("[accept.policy.legacy]\nstrict = false")??
            .node
            .store
            .independent_segments
    );
    assert!(
        resolve_toml("[accept.policy.normal]")??
            .node
            .store
            .independent_segments
    );
    assert!(
        !resolve_with("", &["--accept-strict=false"], &[])??
            .node
            .store
            .independent_segments
    );
    Ok(())
}

#[test]
fn fixture_paths_escape_windows_separators_and_quotes() -> Result<(), Box<dyn Error>> {
    let path = std::path::Path::new(r#"C:\folder with "quotes"\file.pem"#);
    let document: toml::Value = toml::from_str(&format!(
        "path = \"{}\"",
        fixtures::toml_path_contents(path)
    ))?;
    assert_eq!(document["path"].as_str(), path.to_str());
    Ok(())
}

#[test]
fn pipeline_memory_has_independent_configuration_and_precedence() -> Result<(), Box<dyn Error>> {
    let defaults = resolve_toml("")??;
    assert_eq!(
        defaults.node.session.memory_per_publisher,
        128 * 1024 * 1024
    );
    let configured = resolve_with(
        "[pipeline]\nmemory_per_publisher = '192MiB'\n",
        &["--pipeline-memory-per-publisher", "320MiB"],
        &[("RUSHLS_PIPELINE_MEMORY_PER_PUBLISHER", "256MiB")],
    )??;
    assert_eq!(
        configured.node.session.memory_per_publisher,
        320 * 1024 * 1024
    );
    assert_eq!(
        configured.node.store.retention.maximum_payload_bytes,
        defaults.node.store.retention.maximum_payload_bytes
    );
    assert!(resolve_toml("[pipeline]\nmemory_per_publisher = '32MiB'\n")?.is_err());
    assert!(resolve_toml("[pipeline]\nmemory_per_publisher = '0'\n")?.is_err());
    Ok(())
}
