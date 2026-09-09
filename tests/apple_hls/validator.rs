//! Runs Apple's two conformance tools and judges what they say.
//!
//! `mediastreamvalidator` produces JSON; `hlsreport` turns that JSON into an
//! HTML report against the authoring specification. Both are read here, and
//! [`crate::report`] decides which of their findings this origin is answerable
//! for.
//!
//! # Where the findings actually are
//!
//! The validator does not use a `status` field. Every finding it makes is an
//! entry in a `messages` array, and those arrays are scattered through the
//! document — on the multivariant playlist, on each variant, on each rendition,
//! on individual segments and partial segments. A walk that looks anywhere else
//! reports a clean stream no matter what the validator found, which is worth
//! stating plainly because that is exactly what this file used to do.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

use crate::report::{
    AuthoringMode, Expect, Finding, Judgement, Level, Source, authoring_mode, parse_hlsreport,
};

pub fn mediastreamvalidator_available() -> bool {
    Command::new("mediastreamvalidator")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn hlsreport_available() -> bool {
    Command::new("hlsreport")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Validates `url` and fails on any finding this origin is answerable for.
pub fn validate(url: &str, expect: &Expect) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 1..=3 {
        match validate_once(url, expect) {
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

fn validate_once(url: &str, expect: &Expect) -> Result<(), String> {
    let report = temporary_path(expect.name, "json");
    let output = Command::new("mediastreamvalidator")
        .args(["--timeout", "30", "--validation-data-path"])
        .arg(&report)
        .arg(url)
        .output()
        .map_err(|error| format!("mediastreamvalidator failed to start: {error}"))?;
    let json = fs::read_to_string(&report).ok();

    let Some(json) = json else {
        let _ = fs::remove_file(&report);
        return Err(format!(
            "mediastreamvalidator wrote no JSON (status {}):\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    };

    let value: Value = serde_json::from_str(&json)
        .map_err(|error| format!("mediastreamvalidator JSON did not parse ({error}):\n{json}"))?;

    let mut findings = validator_findings(&value);
    match authoring_findings(&report) {
        Ok(mut authoring) => findings.append(&mut authoring),
        // A missing or broken hlsreport must not hide what the validator said.
        Err(reason) => eprintln!("hlsreport unavailable for {}: {reason}", expect.name),
    }
    keep_artifacts(&report, expect.name);
    let _ = fs::remove_file(&report);

    let judgement = Judgement::new(findings, expect);
    eprint!("{}", judgement.render(expect));
    let defects = judgement.defects();
    if defects.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Apple conformance defects in {} (validator process {}):\n{}",
        expect.name,
        output.status,
        defects
            .iter()
            .map(|finding| format!("  {}", finding.describe_indented()))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

/// Runs `hlsreport` over the validator's JSON and parses its HTML.
///
/// The tool has no machine-readable output mode — `--verbose` changes what the
/// HTML contains rather than emitting anything else — so the HTML is what there
/// is to read. Every rule set is requested so a finding that applies only to
/// tvOS or AirPlay 2 still reaches the report.
fn authoring_findings(json: &Path) -> Result<Vec<Finding>, String> {
    if authoring_mode() == AuthoringMode::Off {
        return Ok(Vec::new());
    }
    if !hlsreport_available() {
        return Err("not on PATH".into());
    }
    let html = json.with_extension("html");
    let output = Command::new("hlsreport")
        .args(["--rule-set=all", "--output"])
        .arg(&html)
        .arg(json)
        .output()
        .map_err(|error| format!("failed to start: {error}"))?;
    let text = fs::read_to_string(&html).map_err(|error| {
        format!(
            "wrote no HTML ({error}, status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })?;
    Ok(parse_hlsreport(&text))
}

/// Copies a case's JSON and HTML aside when an operator asked to keep them.
fn keep_artifacts(json: &Path, name: &str) {
    let Some(directory) = std::env::var_os("RUSHLS_TEST_REPORT_DIR") else {
        let _ = fs::remove_file(json.with_extension("html"));
        return;
    };
    let directory = PathBuf::from(directory);
    if fs::create_dir_all(&directory).is_err() {
        return;
    }
    for extension in ["json", "html"] {
        let from = json.with_extension(extension);
        let _ = fs::rename(&from, directory.join(format!("{name}.{extension}")));
        let _ = fs::remove_file(&from);
    }
}

/// Collects every `messages` entry, plus the states that imply one.
fn validator_findings(value: &Value) -> Vec<Finding> {
    let mut found = Vec::new();
    collect(value, "$", None, &mut found);
    dedupe(found)
}

fn collect(value: &Value, path: &str, url: Option<&str>, found: &mut Vec<Finding>) {
    match value {
        Value::Object(map) => {
            // The nearest enclosing URL is far more useful than a JSON path
            // when the finding is "this playlist 503'd", so it is threaded down.
            let url = map.get("url").and_then(Value::as_str).or(url);
            let context = url.map_or_else(|| path.to_owned(), ToOwned::to_owned);

            if let Some(Value::Array(messages)) = map.get("messages") {
                for message in messages {
                    found.push(Finding {
                        source: Source::Validator,
                        level: requirement_level(message),
                        context: context.clone(),
                        title: message_text(message),
                        scopes: Vec::new(),
                    });
                }
            }
            // A playlist the validator could not parse produces no further
            // findings about its contents, so the absence of them is not
            // evidence of correctness. Say so explicitly.
            if map.get("parseFailed").and_then(Value::as_bool) == Some(true) {
                found.push(Finding {
                    source: Source::Validator,
                    level: Level::MustFix,
                    context: context.clone(),
                    title: "playlist could not be parsed (parseFailed)".into(),
                    scopes: Vec::new(),
                });
            }

            for (key, child) in map {
                if key == "messages" {
                    continue;
                }
                collect(child, &format!("{path}.{key}"), url, found);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect(child, &format!("{path}[{index}]"), url, found);
            }
        }
        _ => {}
    }
}

/// Apple encodes severity as an integer whose meaning it does not document.
///
/// Observed reports use `1` for everything including outright fetch failures,
/// so it is read as a hint and nothing is filtered out by it: severity comes
/// from [`crate::report::judge`], which knows what the finding says.
fn requirement_level(message: &Value) -> Level {
    match message.get("errorRequirementLevel").and_then(Value::as_i64) {
        Some(0) => Level::MustFix,
        _ => Level::ShouldFix,
    }
}

fn message_text(message: &Value) -> String {
    let comment = message
        .get("errorComment")
        .and_then(Value::as_str)
        .unwrap_or("unnamed validator message");
    match message.get("errorDetail").and_then(Value::as_str) {
        Some(detail) if detail != comment => format!("{comment} — {detail}"),
        _ => comment.to_owned(),
    }
}

/// Folds repeats of one finding into a single entry with an occurrence count.
///
/// A partial-segment rule broken once is broken on every part, and printing it
/// forty times buries the other findings.
fn dedupe(findings: Vec<Finding>) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    for finding in findings {
        if let Some(index) = out.iter().position(|kept| kept.title == finding.title) {
            counts[index] += 1;
            if !out[index].scopes.contains(&finding.context) {
                out[index].scopes.push(finding.context);
            }
        } else {
            out.push(Finding {
                scopes: vec![finding.context.clone()],
                ..finding
            });
            counts.push(1);
        }
    }
    for (finding, count) in out.iter_mut().zip(counts) {
        if count > 1 {
            finding.scopes.push(format!("x{count}"));
        }
        // Long scope lists are one URL per segment; keep the shape, drop the bulk.
        if finding.scopes.len() > 4 {
            let extra = finding.scopes.len() - 3;
            finding.scopes.truncate(3);
            finding.scopes.push(format!("and {extra} more"));
        }
    }
    out
}

pub fn wait_for_playlist(url: &str, ca_pem: Option<&Path>) -> Result<(), String> {
    let deadline = SystemTime::now() + std::time::Duration::from_secs(25);
    let mut last = String::new();
    while SystemTime::now() < deadline {
        let mut command = Command::new("curl");
        command.args(["--silent", "--show-error", "--fail", "--max-time", "20"]);
        if let Some(ca_pem) = ca_pem {
            command.arg("--http2").arg("--cacert").arg(ca_pem);
        }
        let output = command
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

/// Fetches `url` once and returns its body, for cases that assert on playlist text.
pub fn fetch(url: &str, ca_pem: Option<&Path>) -> Result<String, String> {
    let mut command = Command::new("curl");
    command.args(["--silent", "--show-error", "--fail", "--max-time", "20"]);
    if let Some(ca_pem) = ca_pem {
        command.arg("--cacert").arg(ca_pem);
    }
    let output = command
        .arg(url)
        .output()
        .map_err(|error| format!("curl failed to start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "curl {} for {url}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn temporary_path(name: &str, extension: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time follows the epoch")
        .as_nanos();
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    std::env::temp_dir().join(format!("rushls-{safe}-{nonce}.{extension}"))
}
