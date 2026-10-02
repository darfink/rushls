use super::*;
use std::{error::Error, time::Duration};

#[derive(Debug, Conf)]
#[conf(serde, name = "fixture", env_prefix = "FIXTURE_", version = "1.0")]
pub struct Settings {
    #[conf(long, env, serde(skip))]
    config: Option<PathBuf>,
    #[conf(long, env, env_aliases = ["TITLE"], default_value = "default")]
    name: String,
    #[conf(flatten, prefix)]
    http: Http,
    #[conf(env, secret)]
    token: Option<SecretString>,
    #[conf(env, secret)]
    credential: Option<TextSource>,
}
#[derive(Debug, Conf)]
#[conf(serde)]
pub struct Http {
    #[conf(long, env, default_value = "5s", value_parser = parse_duration, serde(use_value_parser))]
    timeout: Duration,
    #[conf(long, env, default_value = "64KiB", value_parser = parse_bytes, serde(deserialize_with = "deserialize_bytes"))]
    limit: usize,
}
const LOADER: Loader = Loader::new("fixture", "FIXTURE_").explicit_only();

fn file(text: &str) -> Result<tempfile::NamedTempFile, std::io::Error> {
    use std::io::Write;
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(text.as_bytes())?;
    Ok(file)
}
fn load(
    text: &str,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<Loaded<Settings>, Box<dyn Error>> {
    let file = file(text)?;
    let args = [
        OsString::from("fixture"),
        OsString::from("--config"),
        file.path().into(),
    ]
    .into_iter()
    .chain(args.iter().map(OsString::from));
    Ok(LOADER.load_from(args, env.iter().copied())?)
}

#[test]
fn precedence_and_provenance_use_the_same_schema() -> Result<(), Box<dyn Error>> {
    let defaults = LOADER.load_from::<Settings>(["fixture"], Vec::<(&str, &str)>::new())?;
    assert_eq!(defaults.config.name, "default");
    assert!(defaults.config.config.is_none());
    assert_eq!(defaults.sources["name"], Source::Default);
    let toml = "name = '${NAME}'\n[http]\ntimeout = '7s'\nlimit = 1024";
    let from_file = load(toml, &[], &[("NAME", "file")])?;
    assert_eq!(from_file.config.name, "file");
    assert!(matches!(from_file.sources["name"], Source::File(_)));
    assert!(from_file.path.is_some());
    let from_env = load(toml, &[], &[("NAME", "file"), ("FIXTURE_NAME", "env")])?;
    assert_eq!(from_env.config.name, "env");
    assert_eq!(
        from_env.sources["name"],
        Source::Environment("FIXTURE_NAME".into())
    );
    let from_cli = load(
        toml,
        &["--name", "cli", "--http-limit", "2KiB"],
        &[
            ("NAME", "file"),
            ("FIXTURE_NAME", "env"),
            ("FIXTURE_HTTP_TIMEOUT", "9s"),
        ],
    )?;
    assert_eq!(from_cli.config.name, "cli");
    assert_eq!(from_cli.config.http.limit, 2048);
    assert_eq!(from_cli.config.http.timeout, Duration::from_secs(9));
    assert_eq!(from_cli.sources["name"], Source::Cli);
    assert_eq!(
        from_cli.sources["http.timeout"],
        Source::Environment("FIXTURE_HTTP_TIMEOUT".into())
    );
    Ok(())
}

#[test]
fn unknown_document_and_cli_fields_fail_but_environment_names_warn() -> Result<(), Box<dyn Error>> {
    for toml in ["typo = 'secret'", "[http]\ntimout = '1s'"] {
        assert!(load(toml, &["--name", "cli"], &[]).is_err());
    }
    assert!(load("", &["--typo", "secret"], &[]).is_err());
    let loaded = load(
        "",
        &[],
        &[
            ("FIXTURE_Z", "sensitive-value"),
            ("FIXTURE_A", "sensitive-value"),
        ],
    )?;
    assert_eq!(loaded.config.name, "default");
    assert_eq!(
        loaded.warnings,
        [
            "unrecognized environment variable FIXTURE_A is ignored as a configuration override",
            "unrecognized environment variable FIXTURE_Z is ignored as a configuration override",
        ]
    );
    assert!(
        load("", &[], &[("OTHER_APP_SETTING", "anything")])?
            .warnings
            .is_empty()
    );
    Ok(())
}

#[test]
fn explicit_file_selection_has_cli_precedence_and_missing_files_fail() -> Result<(), Box<dyn Error>>
{
    let environment = file("name = 'env-file'")?;
    let command = file("name = 'cli-file'")?;
    let env = [("FIXTURE_CONFIG", environment.path().as_os_str())];
    assert_eq!(
        LOADER.load_from::<Settings>(["fixture"], env)?.config.name,
        "env-file"
    );
    let result = LOADER.load_from::<Settings>(
        [
            OsString::from("fixture"),
            format!("--config={}", command.path().display()).into(),
        ],
        env,
    )?;
    assert_eq!(result.config.name, "cli-file");
    assert_eq!(result.path.as_deref(), Some(command.path()));
    let absent = environment.path().with_extension("missing");
    assert!(matches!(
        LOADER.load_from::<Settings>(["fixture"], [("FIXTURE_CONFIG", absent.as_os_str())]),
        Err(ConfigError::Read { .. })
    ));
    Ok(())
}

#[test]
fn help_and_version_work_without_valid_configuration() -> Result<(), Box<dyn Error>> {
    for flag in ["--help", "-h", "--version", "-V"] {
        let error = LOADER
            .load_from::<Settings>(
                ["fixture", flag],
                [
                    ("FIXTURE_CONFIG", "/missing"),
                    ("FIXTURE_TYPO", "sensitive-sentinel"),
                ],
            )
            .err()
            .ok_or("expected CLI diagnostic")?;
        assert_eq!(error.exit_code(), 0);
        assert!(!error.to_string().contains("sensitive-sentinel"));
    }
    Ok(())
}

#[test]
fn interpolation_is_literal_and_diagnostics_do_not_expose_values() -> Result<(), Box<dyn Error>> {
    let literal = "'\"\n[http]\nlimit = 999999";
    assert_eq!(
        load("name = '${VALUE}'", &[], &[("VALUE", literal)])?
            .config
            .name,
        literal
    );
    assert_eq!(
        load("name = '$${VALUE}/${ABSENT:-fallback}'", &[], &[])?
            .config
            .name,
        "${VALUE}/fallback"
    );
    for toml in [
        "token = 'super-secret-${UNCLOSED'",
        "token = 'super-secret-${bad name}'",
        "token = \"super-secret",
    ] {
        let error = load(toml, &[], &[]).err().ok_or("invalid input accepted")?;
        assert!(!format!("{error:?} {error}").contains("super-secret"));
    }
    let loaded = load("token = '${TOKEN}'", &[], &[("TOKEN", "super-secret")])?;
    assert_eq!(loaded.config.token.as_deref(), Some("super-secret"));
    assert!(!format!("{:?} {:?}", loaded.config, loaded.sources).contains("super-secret"));
    Ok(())
}

#[test]
fn text_sources_are_inline_or_files_by_their_shape() -> Result<(), Box<dyn Error>> {
    let file = file(" secret with spaces \r\n")?;
    let path = file.path().to_str().ok_or("temp path is not UTF-8")?;
    // Mounted files lose trailing line endings only; spaces are credential.
    let from_file = load(&format!("credential = {{ file = '{path}' }}"), &[], &[])?;
    let credential = from_file.config.credential.ok_or("credential missing")?;
    assert_eq!(credential.read("credential")?, " secret with spaces ");
    // Inline values are untouched.
    let inline = load("credential = \"inline\\n\"", &[], &[])?;
    let credential = inline
        .config
        .credential
        .as_ref()
        .ok_or("credential missing")?;
    assert_eq!(credential.read("credential")?, "inline\n");
    // The environment accepts both spellings too.
    let env_file = format!("{{ file = '{path}' }}");
    let from_env = load("", &[], &[("FIXTURE_CREDENTIAL", env_file.as_str())])?;
    let credential = from_env.config.credential.ok_or("credential missing")?;
    assert_eq!(credential.read("credential")?, " secret with spaces ");
    // A malformed reference is refused without echoing the value.
    let error = load("credential = { path = 'super-secret' }", &[], &[])
        .err()
        .ok_or("unknown reference field accepted")?;
    assert!(!format!("{error:?} {error}").contains("super-secret"));
    assert!(!format!("{:?}", inline.config).contains("inline"));
    Ok(())
}

#[test]
fn discovered_paths_are_ordered_and_unique() -> Result<(), Box<dyn Error>> {
    let paths = LOADER.config_paths();
    assert_eq!(
        paths.first(),
        Some(&std::env::current_dir()?.join("fixture.toml"))
    );
    assert_eq!(paths.iter().collect::<BTreeSet<_>>().len(), paths.len());
    if cfg!(unix) {
        assert_eq!(
            paths.last(),
            Some(&std::path::PathBuf::from("/etc/fixture/fixture.toml")),
            "the system-wide file is the last resort"
        );
    }
    Ok(())
}

#[test]
fn declared_environment_aliases_are_known() -> Result<(), Box<dyn Error>> {
    let loaded = LOADER.load_from::<Settings>(["fixture"], [("FIXTURE_TITLE", "alias")])?;
    assert_eq!(loaded.config.name, "alias");
    assert!(loaded.warnings.is_empty());
    Ok(())
}

#[test]
fn secret_type_errors_are_redacted() -> Result<(), Box<dyn Error>> {
    let error = load("token = 918273645", &[], &[])
        .err()
        .ok_or("invalid secret type accepted")?;
    assert!(!format!("{error:?} {error}").contains("918273645"));
    Ok(())
}
