#[path = "../crates/rushls-config/tests/support/fixtures.rs"]
mod fixtures;

#[test]
fn shared_configuration_startup_contract() -> Result<(), Box<dyn std::error::Error>> {
    fixtures::cli_contract(env!("CARGO_BIN_EXE_rushls"), "RUSHLS_")
}

#[test]
fn print_config_example_bypasses_configuration_loading() -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command;

    // A path inside a regular file cannot exist, regardless of the host's temp files.
    let missing = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("Cargo.toml")
        .join("missing.toml");
    for explicit_path in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rushls"));
        command
            .env_clear()
            .env("RUSHLS_CONFIG", &missing)
            .env("RUSHLS_SHUTDOWN", "invalid-duration")
            .env("RUSHLS_METRICS_TOKEN", "sensitive-sentinel")
            .env("RUST_LOG", "invalid[filter")
            .arg("--print-config-example");
        if explicit_path {
            command.arg("--config").arg(&missing);
        }
        let output = command.output()?;
        assert!(output.status.success(), "{:?}", output.stderr);
        assert_eq!(output.stdout, include_bytes!("../rushls.example.toml"));
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    }
    Ok(())
}

#[test]
fn print_config_example_obeys_argument_boundaries() -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command;

    for args in [
        vec!["--", "--print-config-example"],
        vec!["--name=--print-config-example"],
        vec!["--config", "--print-config-example"],
        vec!["--print-config-example", "--unknown-option"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rushls"))
            .env_clear()
            .env("RUSHLS_SHUTDOWN", "invalid-duration")
            .args(args)
            .output()?;
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    let help = Command::new(env!("CARGO_BIN_EXE_rushls"))
        .env_clear()
        .arg("--help")
        .output()?;
    assert!(help.status.success());
    assert!(String::from_utf8(help.stdout)?.contains("--print-config-example"));
    Ok(())
}
