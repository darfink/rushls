//! The same startup contract runs against both application binaries.

use std::{error::Error, process::Command};

pub fn cli_contract(binary: &str, prefix: &str) -> Result<(), Box<dyn Error>> {
    let missing = std::env::temp_dir()
        .join(format!("cc-config-absent-{}", std::process::id()))
        .join("absent.toml");
    let config_var = format!("{prefix}CONFIG");
    let typo_var = format!("{prefix}UNKNOWN_CONFIG_OPTION");
    for flag in ["--help", "--version"] {
        let output = Command::new(binary)
            .env_clear()
            .env(&config_var, &missing)
            .env(&typo_var, "sensitive-sentinel")
            .arg(flag)
            .output()?;
        assert!(
            output.status.success(),
            "{prefix} {flag}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("sensitive-sentinel"));
    }
    let output = Command::new(binary)
        .env_clear()
        .env(&config_var, &missing)
        .env(&typo_var, "sensitive-sentinel")
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    // Unknown environment names cannot mask a genuine missing-file error.
    let diagnostic = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        diagnostic.contains("could not read configuration file"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("sensitive-sentinel"));
    let output = Command::new(binary)
        .env_clear()
        .env(&config_var, &missing)
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    Ok(())
}
