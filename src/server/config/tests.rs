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
        ClientInfo, IngestProtocol, PresentedCredential, Principal,
        PublishRequest, PublishResource, StreamPolicy, TakeoverPolicy,
    },
    delivery::store::{DurationRule, TargetDurationMultiple},
    domain::{Codec, StreamId},
    observe::lifecycle::Kind,
    server::{AllowedOrigins, NodeConfig, ResolvedAppConfig},
};

use super::{AppConfig, ConfigError, decimal_fraction, parse_playlist_window};

const BASE_CONFIG: &str = "";

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
    .resolve_from(std::iter::empty())?;

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
    let config = AppConfig::load_from(os(["rushls"]), std::iter::empty())?
        .resolve_from(std::iter::empty())?;
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
        r"
[server]
maximum_concurrent_publishers = 10
",
        &["--server-maximum-concurrent-publishers", "30"],
        &[("RUSHLS_SERVER_MAXIMUM_CONCURRENT_PUBLISHERS", "20")],
    )??;

    assert_eq!(config.node.maximum_sessions, 30);
    Ok(())
}

#[test]
fn unknown_rushls_environment_variables_warn_instead_of_failing() -> Result<(), Box<dyn Error>> {
    let env = [
        // A recognized override and a variable outside the namespace stay quiet.
        ("RUSHLS_SERVER_MAXIMUM_CONCURRENT_PUBLISHERS", "20"),
        ("PAGER", "less"),
        // Two misspellings are named, in a stable order.
        ("RUSHLS_SERVER_MAXIMUM_CONCURRENT_PUBLISHER", "20"),
        ("RUSHLS_TLS_CERTIFICATE", "/tmp/certificate.pem"),
    ]
    .into_iter()
    .map(|(key, value)| (OsString::from(key), OsString::from(value)));
    let resolved = AppConfig::load_and_resolve_from(os(["rushls"]), env)?;

    assert_eq!(resolved.node.maximum_sessions, 20);
    assert_eq!(
        resolved.warnings.as_slice(),
        [
            "unrecognized environment variable RUSHLS_SERVER_MAXIMUM_CONCURRENT_PUBLISHER is ignored",
            "unrecognized environment variable RUSHLS_TLS_CERTIFICATE is ignored",
        ]
    );
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
        r#"
[auth.policies.default]
takeovers = "deny"
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
async fn open_authentication_uses_the_builtin_policy_when_http_is_omitted()
-> Result<(), Box<dyn Error>> {
    let config = resolve_toml("")??;

    let grant = config
        .authenticator
        .authenticate(&request("ignored"))
        .await?;
    assert_eq!(grant.policy, StreamPolicy::permissive());
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
        ("RUSHLS_METRICS_LISTEN", "127.0.0.1:9090"),
        ("RUSHLS_METRICS_TOKEN", "scrape-secret"),
        ("RUSHLS_METRICS_PER_STREAM", "true"),
    ])??;

    assert!(config.node.metrics.listen.is_some());
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
{BASE_CONFIG}

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
{BASE_CONFIG}

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
        BASE_CONFIG,
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
            r"
[server]
maximum_concurrent_publishers = 10
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
fn the_admission_budget_derives_from_the_publisher_budget() -> Result<(), Box<dyn Error>> {
    // A pending admission is a precursor to an ingest session, so the
    // publisher budget is its parent. The stream budget deliberately is not: a
    // node holding many retained streams has no reason to accept more
    // unauthenticated sockets.
    let config = resolve_toml(
        r"
[server]
maximum_concurrent_publishers = 40
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
[auth.http]
url = "http://auth-sidecar:8081/v1/publish/admit"
"#;
    assert!(
        resolve_toml(valid)?.is_ok(),
        "a minimal http auth table works"
    );

    // The internal admission deadline is 10s. A longer request timeout never
    // takes effect: the session gives up first and blames a stage rather than
    // the service that did not answer.
    let outlives_admission = r#"
[auth.http]
url = "http://auth-sidecar:8081/admit"
request_timeout = "30s"
"#;
    // A response may only name a policy this node actually has.
    let unknown_default = r#"
[auth.http]
url = "http://auth-sidecar:8081/admit"
default_policy = "nonexistent"
    "#;
    let unusable_url = r#"
[auth.http]
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
fn http_auth_is_selected_by_the_optional_table() -> Result<(), Box<dyn Error>> {
    let configuration = r#"
[auth.http]
url = "http://auth-sidecar:8081/admit"
"#;
    assert!(resolve_toml(configuration)?.is_ok());
    assert!(resolve_toml("[auth.open]\n")?.is_err());
    Ok(())
}

#[test]
fn hooks_are_absent_until_an_endpoint_is_configured() -> Result<(), Box<dyn Error>> {
    let resolved = resolve_toml("[server]\nname = \"studio\"\n")??;

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
[server]
name = "studio"

[hooks.endpoints.automation]
url = "http://automation:9000/events"
events = ["session.started", "session.ended"]
maximum_attempts = 2
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
[ingest.rtmp]
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
[ingest.rtmp]
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
[ingest.rtmp]
timeout = "off"
"#,
    )?
    .unwrap_or_else(|error| panic!("a disabled timeout must resolve: {error}"));

    let timeouts = resolved.node.rtmp.timeouts;
    assert_eq!(timeouts.session_read, None);
    assert_eq!(timeouts.write, None);
    assert_eq!(timeouts.handshake_read, Some(Duration::from_secs(5)));
    assert_eq!(
        resolved.warnings.len(),
        1,
        "an unbounded wait on a public bind should not be invisible: {:?}",
        resolved.warnings
    );
    Ok(())
}
#[test]
fn the_stall_deadline_follows_the_segment_duration() -> Result<(), Box<dyn Error>> {
    // The reason this is relative. An absolute value sized against a 6s
    // segment becomes stall detection that fires before one segment can
    // complete the moment segmentation is retuned.
    let resolved = resolve_toml(
        r#"
[hls]
segment_duration = "10s"
part_duration = "1s"
"#,
    )?
    .unwrap_or_else(|error| panic!("a longer segment duration must resolve: {error}"));

    let health = resolved.node.session.supervision.health;
    assert_eq!(health.stall, Duration::from_secs(20));
    Ok(())
}

#[test]
fn a_stall_deadline_may_be_pinned_to_an_absolute() -> Result<(), Box<dyn Error>> {
    // A deployment with its own reasons keeps the fixed form available.
    let resolved = resolve_toml(
        r#"
[ingest.health]
stall = "20s"
"#,
    )?
    .unwrap_or_else(|error| panic!("a fixed stall deadline must resolve: {error}"));

    let health = resolved.node.session.supervision.health;
    assert_eq!(health.stall, Duration::from_secs(20));
    Ok(())
}

#[test]
fn a_stall_deadline_shorter_than_its_sampling_is_refused() -> Result<(), Box<dyn Error>> {
    // Sampling cannot observe a deadline shorter than its own period, so such
    // a value is not the tighter detection it appears to be.
    let Err(error) = resolve_toml(
        r#"
[ingest.health]
stall = "500ms"
health_interval = "1s"
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
[ingest.srt]
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
certificate = "{}"
key = "{}"
handshake_timeout = "2s"
maximum_pending_handshakes = 32
"#,
        certificate.path.display(),
        key.path.display()
    ))?
    .unwrap_or_else(|error| panic!("TLS admission limits must resolve: {error}"));

    let tls = resolved.node.http.tls.expect("TLS is configured");
    assert_eq!(tls.handshake_timeout, Duration::from_secs(2));
    assert_eq!(tls.maximum_pending_handshakes, 32);
    Ok(())
}

#[test]
fn an_rtmp_timeout_below_a_keyframe_interval_is_refused() -> Result<(), Box<dyn Error>> {
    // Hardening that drops legitimate publishers is an outage, not a defence.
    let Err(error) = resolve_toml(
        r#"
[ingest.rtmp]
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
        parse_playlist_window("6x")?,
        DurationRule::MultipleOfTarget(TargetDurationMultiple::integer(6))
    );
    assert_eq!(
        parse_playlist_window("1.5x")?,
        DurationRule::MultipleOfTarget(TargetDurationMultiple::new(3, nz::u32!(2)))
    );
    assert_eq!(
        parse_playlist_window("18s")?,
        DurationRule::Fixed(Duration::from_secs(18))
    );
    assert!(parse_playlist_window("0x").is_err(), "zero is refused");
    assert!(
        parse_playlist_window("x").is_err(),
        "bare suffix is refused"
    );
    assert!(
        parse_playlist_window("1.2.3x").is_err(),
        "one decimal point only"
    );
    assert!(parse_playlist_window("abc").is_err(), "garbage is refused");
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
part_duration = "1s"
hold_back = "4x"
"#,
    )??;
    assert_eq!(
        relative.node.hls.timing.part_hold_back.resolve(Duration::from_secs(1)),
        Duration::from_secs(4)
    );

    let absolute = resolve_toml(
        r#"
[hls]
part_duration = "1s"
hold_back = "2500ms"
"#,
    )??;
    assert_eq!(
        absolute.node.hls.timing.part_hold_back.resolve(Duration::from_secs(1)),
        Duration::from_millis(2_500)
    );

    // Below two parts a client runs out of buffered media on any loss, which
    // is a latency the protocol cannot deliver rather than an aggressive one.
    let error = resolve_toml(
        r#"
[hls]
part_duration = "1s"
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
fn the_playlist_window_becomes_the_retention_window() -> Result<(), Box<dyn Error>> {
    let config = resolve_toml(
        r#"
[hls]
segment_duration = "6s"
playlist_window = "18s"
"#,
    )??;

    let retention = config.node.store.retention;
    assert_eq!(retention.retain, Duration::from_secs(18));
    Ok(())
}

#[test]
fn a_fixed_window_below_three_target_durations_is_refused() -> Result<(), Box<dyn Error>> {
    assert!(
        resolve_toml(
            r#"
[hls]
segment_duration = "6s"
playlist_window = "12s"
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
segment_duration = "6s"

[storage]
inactive_stream_retention = "5s"
"#,
        )?
        .is_err(),
        "retiring a stream after 5s strands a viewer holding 18s of HOLD-BACK"
    );
    assert!(
        resolve_toml(
            r#"
[storage]
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
        let _ = parse_playlist_window(&value);
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
    let key_length = load_toml(&format!(
        "{BASE_CONFIG}\n[ingest.srt]\nencryption_key_length = \"aes999\"\n"
    ))?
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
[auth.policies.default]
subtitle_codecs = ["text", "webvtt"]
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
[auth.policies.default]
video_codecs = ["text"]
"#,
    )?;
    assert!(matches!(wrong_kind, Err(ConfigError::Invalid(_))));
    Ok(())
}
