//! Run `mediastreamvalidator` and fail on any error or warning in its JSON.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

pub fn mediastreamvalidator_available() -> bool {
    Command::new("mediastreamvalidator")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Validates `url` and requires a clean report: no error or warning entries.
pub fn validate(url: &str) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 1..=3 {
        match validate_once(url) {
            Ok(()) => return Ok(()),
            Err(error) if crashed_without_report(&error) && attempt < 3 => {
                last = error;
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(error) => return Err(error),
        }
    }
    Err(last)
}

fn crashed_without_report(error: &str) -> bool {
    error.contains("wrote no JSON")
}

fn validate_once(url: &str) -> Result<(), String> {
    let report = temporary_path("json");
    let output = Command::new("mediastreamvalidator")
        .args(["--timeout", "30", "--validation-data-path"])
        .arg(&report)
        .arg(url)
        .output()
        .map_err(|error| format!("mediastreamvalidator failed to start: {error}"))?;
    let json = fs::read_to_string(&report).ok();
    let _ = fs::remove_file(&report);

    let Some(json) = json else {
        return Err(format!(
            "mediastreamvalidator wrote no JSON (status {}):\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    };

    let value: Value = serde_json::from_str(&json)
        .map_err(|error| format!("mediastreamvalidator JSON did not parse ({error}):\n{json}"))?;
    let issues = issues(&value, "$");
    if issues.is_empty() {
        return Ok(());
    }
    Err(format!(
        "mediastreamvalidator reported errors or warnings (process {}):\n{}\n\n{}\n{}",
        output.status,
        issues.join("\n"),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn issues(value: &Value, path: &str) -> Vec<String> {
    let mut found = Vec::new();
    collect(value, path, &mut found);
    found
}

fn collect(value: &Value, path: &str, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            let status = map
                .get("status")
                .and_then(Value::as_str)
                .or_else(|| map.get("errorStatus").and_then(Value::as_str))
                .unwrap_or("");
            if is_problem_status(status) {
                found.push(format!(
                    "{path}: {status}: {}",
                    map.get("errorComment")
                        .or_else(|| map.get("comment"))
                        .or_else(|| map.get("details"))
                        .cloned()
                        .unwrap_or_else(|| Value::Object(map.clone()))
                ));
            } else if map.get("mustFix").and_then(Value::as_bool) == Some(true)
                || map.get("shouldFix").and_then(Value::as_bool) == Some(true)
            {
                found.push(format!("{path}: {}", Value::Object(map.clone())));
            }
            for (key, child) in map {
                collect(child, &format!("{path}.{key}"), found);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect(child, &format!("{path}[{index}]"), found);
            }
        }
        _ => {}
    }
}

fn is_problem_status(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "error" | "warning" | "fail" | "failed" | "mustfix" | "shouldfix"
    )
}

pub fn wait_for_playlist(url: &str, ca_pem: &Path) -> Result<(), String> {
    let deadline = SystemTime::now() + std::time::Duration::from_secs(25);
    let mut last = String::new();
    while SystemTime::now() < deadline {
        let output = Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--fail",
                "--max-time",
                "20",
                "--http2",
                "--cacert",
            ])
            .arg(ca_pem)
            .arg(url)
            .output()
            .map_err(|error| format!("curl failed to start: {error}"))?;
        if output.status.success() {
            let body = String::from_utf8_lossy(&output.stdout);
            if body.contains("#EXTM3U") {
                return Ok(());
            }
            last = format!("playlist was not M3U8:\n{body}");
        } else {
            last = format!(
                "curl {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err(format!("playlist was not served in time: {last}"))
}

fn temporary_path(extension: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time follows the epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("rushls-apple-hls-{nonce}.{extension}"))
}
