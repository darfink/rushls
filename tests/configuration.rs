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

#[tokio::test]
async fn readme_and_operator_guide_toml_examples_resolve() -> Result<(), Box<dyn std::error::Error>>
{
    use rushls::server::config::AppConfig;
    use std::{ffi::OsString, fs};

    // Replace deployment resources only. Keep field names, durations, capacities,
    // claims, and protocol choices unchanged so documentation drift fails here.
    fn resources(value: &mut toml::Value, root: &std::path::Path) {
        if let toml::Value::Table(table) = value {
            for (key, value) in table {
                let file = match key.as_str() {
                    "cert" => Some("cert.pem"),
                    "key" => Some("key.pem"),
                    "signing_secret_file" => Some("signing"),
                    "token_file" | "passphrase_file" => Some("token"),
                    "dir" => Some("storage"),
                    _ => None,
                };
                if let Some(file) = file {
                    *value = toml::Value::String(root.join(file).to_string_lossy().into_owned());
                } else {
                    resources(value, root);
                }
            }
        }
    }

    let directory = std::env::temp_dir().join(format!("rushls-docs-{}", uuid::Uuid::now_v7()));
    fs::create_dir_all(&directory)?;
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
    fs::write(directory.join("cert.pem"), certified.cert.pem())?;
    fs::write(
        directory.join("key.pem"),
        certified.signing_key.serialize_pem(),
    )?;
    fs::write(directory.join("token"), "documentation-test-token")?;
    fs::write(
        directory.join("signing"),
        format!("whsec_{}", "YWFh".repeat(12)),
    )?;

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut count = 0;
        for (name, text) in [
            ("README.md", include_str!("../README.md")),
            ("docs/publishing.md", include_str!("../docs/publishing.md")),
            ("docs/deployment.md", include_str!("../docs/deployment.md")),
        ] {
            let text = text.replace("\r\n", "\n");
            for (index, block) in text.split("```toml\n").skip(1).enumerate() {
                let source = block.split_once("```").ok_or("unclosed TOML fence")?.0;
                let mut document: toml::Value = toml::from_str(source)?;
                resources(&mut document, &directory);
                let path = directory.join("example.toml");
                fs::write(&path, toml::to_string(&document)?)?;
                AppConfig::load_and_resolve_from(
                    [
                        OsString::from("rushls"),
                        OsString::from("--config"),
                        path.into_os_string(),
                    ],
                    [],
                )
                .map_err(|error| format!("{name} TOML example {}: {error}", index + 1))?;
                count += 1;
            }
        }
        assert!(count > 0, "expected the operator configuration examples");
        Ok(())
    })();
    fs::remove_dir_all(directory)?;
    result
}
