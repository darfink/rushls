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

pub fn tools_available() -> bool {
    mediastreamvalidator_available() && hlsreport_available()
}

fn hlsreport_available() -> bool {
    Command::new("hlsreport")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// One live scan can postpone fetching the initial playlist's parts until its end.
pub const SCAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Validates `url` and fails on any finding this origin is answerable for.
pub fn validate(url: &str, expect: &Expect) -> Result<(), String> {
    if expect.full_authoring && (authoring_mode() == AuthoringMode::Off || !tools_available()) {
        return Err(
            "the full authoring audit requires both Apple tools and hlsreport enabled".into(),
        );
    }
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
        .arg("--timeout")
        .arg(SCAN_TIMEOUT.as_secs().to_string())
        .arg("--validation-data-path")
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
        Err(reason)
            if expect.full_authoring
                || std::env::var_os("RUSHLS_TEST_REQUIRE_APPLE_TOOLS").is_some() =>
        {
            keep_artifacts(&report, expect.name)?;
            return Err(format!("hlsreport failed: {reason}"));
        }
        Err(reason) => eprintln!("hlsreport unavailable for {}: {reason}", expect.name),
    }
    keep_artifacts(&report, expect.name)?;
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
    let original = render_hlsreport(json, None)?;
    let version = Command::new("hlsreport")
        .arg("--version")
        .output()
        .map_err(|error| format!("cannot identify hlsreport: {error}"))?;
    let mut value: Value =
        serde_json::from_slice(&fs::read(json).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    let version_text = format!(
        "{}{}",
        String::from_utf8_lossy(&version.stdout),
        String::from_utf8_lossy(&version.stderr)
    );
    if version.status.success() && normalize_tls12_enum(&mut value, version_text.trim()) {
        // Preserve Apple's original artifacts. This copy changes representation,
        // not the negotiated protocol or any cipher, finding, or stream metadata.
        let compatible = json.with_extension("hlsreport-compat.json");
        fs::write(
            &compatible,
            serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let text = render_hlsreport(
            &compatible,
            Some(
                "Compatibility report: hlsreport 1.20.7 expects the legacy TLS 1.2 enum (8). The validator recorded the equivalent wire value 771 (0x0303). Only that representation was converted; original JSON and HTML are retained separately.",
            ),
        )?;
        return Ok(parse_hlsreport(&text));
    }
    Ok(parse_hlsreport(&original))
}

/// Version-specific bridge between Apple's two protocol enums. Do not translate
/// TLS 1.3 into TLS 1.2, or guess how a different tool/schema version behaves.
fn normalize_tls12_enum(value: &mut Value, report_version: &str) -> bool {
    if report_version != "hlsreport: Version 1.20.7 (618.19-230505)"
        || value.get("validatorVersion").and_then(Value::as_str) != Some("1.20.7 (618.19-230505)")
        || value.get("dataVersion").and_then(Value::as_f64) != Some(1.1)
    {
        return false;
    }
    let Some(hosts) = value.get_mut("sslHosts").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for host in hosts.values_mut() {
        if let Some(version) = host.get_mut("sslNegotiatedProtocolVersion")
            && version.as_u64() == Some(0x0303)
        {
            *version = Value::from(8);
            changed = true;
        }
    }
    changed
}

fn render_hlsreport(json: &Path, note: Option<&str>) -> Result<String, String> {
    let html = json.with_extension("html");
    let mut command = Command::new("hlsreport");
    command.args(["--rule-set=all", "--output"]).arg(&html);
    if let Some(note) = note {
        command.arg("--note").arg(note);
    }
    let output = command
        .arg(json)
        .output()
        .map_err(|error| format!("failed to start: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "hlsreport exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = fs::read_to_string(&html).map_err(|error| format!("wrote no HTML: {error}"))?;
    if !text.contains("HLS Validation Report") {
        return Err("hlsreport output contains no HLS Validation Report heading".into());
    }
    Ok(text)
}

/// Copies a case's JSON and HTML aside when an operator asked to keep them.
fn keep_artifacts(json: &Path, name: &str) -> Result<(), String> {
    let Some(directory) = std::env::var_os("RUSHLS_TEST_REPORT_DIR") else {
        for extension in ["html", "hlsreport-compat.json", "hlsreport-compat.html"] {
            let _ = fs::remove_file(json.with_extension(extension));
        }
        return Ok(());
    };
    let directory = PathBuf::from(directory);
    fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create report directory: {error}"))?;
    for extension in [
        "json",
        "html",
        "hlsreport-compat.json",
        "hlsreport-compat.html",
    ] {
        let from = json.with_extension(extension);
        if from.exists() {
            fs::copy(&from, directory.join(format!("{name}.{extension}")))
                .map_err(|error| format!("cannot preserve {extension} report: {error}"))?;
        }
        let _ = fs::remove_file(&from);
    }
    Ok(())
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

/// Fetch a retained cue segment together with its optional WebVTT initialization.
/// `EXT-X-MAP` permits the WEBVTT header to live outside each media segment.
pub fn fetch_webvtt(playlist: &str, ca_pem: Option<&Path>) -> Result<String, String> {
    let segment = playlist
        .lines()
        .find(|line| !line.starts_with('#') && !line.is_empty())
        .ok_or("no retained subtitle segment")?;
    let mut text = String::new();
    if let Some(map) = playlist
        .lines()
        .find(|line| line.starts_with("#EXT-X-MAP:"))
    {
        let uri = map
            .split("URI=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .ok_or("subtitle initialization has no URI")?;
        text.push_str(&fetch(uri, ca_pem)?);
    }
    text.push_str(&fetch(segment, ca_pem)?);
    Ok(text)
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

#[test]
fn tls_enum_compatibility_preserves_evidence_and_is_version_scoped() {
    let original = serde_json::json!({
        "validatorVersion": "1.20.7 (618.19-230505)", "dataVersion": 1.1,
        "messages": [{"errorComment": "keep this finding"}],
        "sslHosts": {
            "tls12": {"sslNegotiatedProtocolVersion": 771, "sslNegotiatedCipher": 49196},
            "tls13": {"sslNegotiatedProtocolVersion": 772, "sslNegotiatedCipher": 4866},
            "tls11": {"sslNegotiatedProtocolVersion": 770},
            "unknown": {"sslNegotiatedProtocolVersion": 999},
            "missing": {}
        }
    });
    let version = "hlsreport: Version 1.20.7 (618.19-230505)";
    let mut converted = original.clone();
    assert!(normalize_tls12_enum(&mut converted, version));
    let mut expected = original.clone();
    expected["sslHosts"]["tls12"]["sslNegotiatedProtocolVersion"] = Value::from(8);
    assert_eq!(converted, expected);
    assert!(!normalize_tls12_enum(&mut converted, version));
    let mut unchanged = original.clone();
    assert!(!normalize_tls12_enum(
        &mut unchanged,
        "hlsreport: Version 1.21"
    ));
    assert_eq!(unchanged, original);
    for (field, new_value) in [
        ("validatorVersion", Value::from("1.21")),
        ("dataVersion", Value::from(1.2)),
    ] {
        let mut unrecognized = original.clone();
        unrecognized[field] = new_value;
        let expected = unrecognized.clone();
        assert!(!normalize_tls12_enum(&mut unrecognized, version));
        assert_eq!(unrecognized, expected);
    }
}
