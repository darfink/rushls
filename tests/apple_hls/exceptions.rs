//! Evidence required before accepting known Apple 1.26.143 validator defects.
use std::{collections::BTreeSet, process::Command};

use serde_json::Value;
use url::Url;

use crate::report::{Judgement, Source, Verdict};

const VERSION: &str = "1.26.143 (1.26.143-260527)";
const HOLD_BACK: &str = "Apple 1.26.143 incorrectly reports missing I-frame HOLD-BACK; the reported value and fetched playlist both satisfy 3 x TARGETDURATION";
const TLS: &str = "Apple 1.26.143 exports missing TLS metadata as false; independent curl verification passed and every reported URL uses the same HTTPS origin";

pub fn apply(value: &Value, judgement: &mut Judgement) -> Result<(), String> {
    if value["validatorVersion"].as_str() != Some(VERSION)
        || value["dataVersion"].as_f64() != Some(1.3)
    {
        return Ok(());
    }
    let mut objects = Vec::new();
    collect(value, &mut objects);
    let hold_back_verified = verify_hold_back(&objects)?;
    let needs_tls = judgement
        .findings
        .iter()
        .any(|(finding, _)| finding.source == Source::Authoring && tls_title(&finding.title));
    let tls_verified = needs_tls && https_origin(value, &objects).is_some();
    if tls_verified {
        verify_tls(value)?;
    }
    let media: BTreeSet<_> = objects
        .iter()
        .filter(|object| {
            object["playlistKind"].as_str() == Some("media")
                && object["iframeOnly"].as_bool() != Some(true)
        })
        .filter_map(|object| object["url"].as_str())
        .collect();
    let needs_he_aac = judgement
        .findings
        .iter()
        .any(|(finding, _)| he_aac_finding(finding));
    let he_aac_verified = needs_he_aac && verify_he_aac(&objects)?;
    for (finding, verdict) in &mut judgement.findings {
        if media.len() == 1
            && match finding.source {
                Source::Validator => {
                    finding.title == "Low-latency playlist MUST declare EXT-X-RENDITION-REPORT tags"
                        && media.contains(finding.context.as_str())
                }
                Source::Authoring => {
                    finding.title
                        == "Low-latency playlist MUST declare EXT-X-RENDITION-REPORT tags [#-50125]"
                        && !finding.scopes.is_empty()
                        && finding.scopes.iter().all(|scope| {
                            matches!(
                                scope.as_str(),
                                "All Variants" | "All URI Renditions" | "All Renditions"
                            )
                        })
                }
            }
        {
            // RFC 8216bis Appendix B.1 excludes the current and I-frame playlists.
            *verdict = Verdict::NotApplicable(
                "only one regular media playlist exists; there is no other rendition to report",
            );
        }
        if he_aac_verified && he_aac_finding(finding) {
            *verdict = Verdict::CompatibilityException(
                "Apple 1.26.143 labels this HE-AAC stream AAC-LC, including when FFmpeg packages it; ffprobe independently confirmed HE-AAC on every audio rendition",
            );
        }

        if tls_verified && finding.source == Source::Authoring && tls_title(&finding.title) {
            *verdict = Verdict::CompatibilityException(TLS);
        }
        if hold_back_verified
            && match finding.source {
                Source::Validator => {
                    finding.title == "Missing server control attribute — attribute:HOLD-BACK"
                }
                Source::Authoring => {
                    finding.title == "Missing server control attribute [#-50096]"
                        && !finding.scopes.is_empty()
                        && finding
                            .scopes
                            .iter()
                            .all(|scope| scope == "All I-Frame Variants")
                }
            }
        {
            *verdict = Verdict::CompatibilityException(HOLD_BACK);
        }
    }
    Ok(())
}

fn tls_title(title: &str) -> bool {
    matches!(
        title,
        "Multivariant playlists SHOULD be delivered using Transport Layer Security (TLS) [#1041]"
            | "Media playlists SHOULD be delivered using TLS [#1042]"
            | "Media segments SHOULD be delivered over TLS [#1043]"
    )
}

fn collect<'a>(value: &'a Value, objects: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            objects.push(value);
            for child in map.values() {
                collect(child, objects);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect(item, objects);
            }
        }
        _ => {}
    }
}

fn https_origin(value: &Value, objects: &[&Value]) -> Option<String> {
    let root = Url::parse(value["url"].as_str()?).ok()?;
    if root.scheme() != "https" {
        return None;
    }
    let mut origins = BTreeSet::new();
    for object in objects {
        if let Some(raw) = object.get("url") {
            let url = Url::parse(raw.as_str()?).ok()?;
            if url.scheme() != "https" {
                return None;
            }
            origins.insert(url.origin().ascii_serialization());
        }
    }
    (origins.len() == 1 && origins.contains(&root.origin().ascii_serialization()))
        .then(|| root.origin().ascii_serialization())
}

fn valid_hold_back(value: &Value) -> bool {
    value["iframeOnly"].as_bool() == Some(true)
        && value["messages"].as_array().is_some_and(|messages| {
            messages
                .iter()
                .filter(|message| message["errorStatusCode"].as_i64() == Some(-50096))
                .all(|message| message["errorDetail"].as_str() == Some("attribute:HOLD-BACK"))
        })
        && valid_duration(
            value["playlistHoldBackDuration"].as_f64(),
            value["playlistTargetDuration"].as_f64(),
        )
}

fn valid_duration(hold: Option<f64>, target: Option<f64>) -> bool {
    matches!((hold, target), (Some(hold), Some(target))
        if hold.is_finite() && target.is_finite() && target > 0.0 && hold >= 3.0 * target)
}

fn valid_playlist(playlist: &str) -> bool {
    let target = playlist
        .lines()
        .find_map(|line| line.strip_prefix("#EXT-X-TARGETDURATION:")?.parse().ok());
    let hold = playlist.lines().find_map(|line| {
        line.strip_prefix("#EXT-X-SERVER-CONTROL:")?
            .split(',')
            .find_map(|attribute| attribute.strip_prefix("HOLD-BACK=")?.parse().ok())
    });
    playlist.lines().any(|line| line == "#EXT-X-I-FRAMES-ONLY") && valid_duration(hold, target)
}

#[test]
fn hold_back_exception_requires_independent_evidence() {
    let valid = "#EXTM3U\n#EXT-X-I-FRAMES-ONLY\n#EXT-X-TARGETDURATION:6\n#EXT-X-SERVER-CONTROL:HOLD-BACK=18,CAN-BLOCK-RELOAD=YES\n";
    assert!(valid_playlist(valid));
    for invalid in [
        valid.replace("HOLD-BACK=18,", ""),
        valid.replace("=18", "=17.9"),
        valid.replace("#EXT-X-I-FRAMES-ONLY\n", ""),
        valid.replace("=18", "=NaN"),
    ] {
        assert!(!valid_playlist(&invalid));
    }
    let mut value = serde_json::json!({"iframeOnly":true,"playlistHoldBackDuration":18,
        "playlistTargetDuration":6,"messages":[{"errorStatusCode":-50096,"errorDetail":"attribute:HOLD-BACK"}]});
    assert!(valid_hold_back(&value));
    value["messages"][0]["errorDetail"] = "attribute:PART-HOLD-BACK".into();
    assert!(!valid_hold_back(&value));
}

#[test]
fn tls_exception_rejects_mixed_origins_and_cleartext() {
    let mut value = serde_json::json!({"url":"https://example.com/master.m3u8","segments":[{"url":"https://example.com/segment.m4s"}]});
    let mut objects = Vec::new();
    collect(&value, &mut objects);
    assert!(https_origin(&value, &objects).is_some());
    for url in [
        "http://example.com/segment.m4s",
        "https://other.example/segment.m4s",
        "segment.m4s",
    ] {
        value["segments"][0]["url"] = url.into();
        let mut objects = Vec::new();
        collect(&value, &mut objects);
        assert!(https_origin(&value, &objects).is_none());
    }
    assert!(!tls_title("A new TLS error [#1044]"));
}

#[test]
fn other_versions_and_real_missing_hold_back_remain_defects() -> Result<(), String> {
    use crate::report::{Expect, Finding, Level};
    let finding = Finding {
        source: Source::Authoring,
        level: Level::MustFix,
        context: "General requirements".into(),
        title: "Missing server control attribute [#-50096]".into(),
        scopes: vec!["All I-Frame Variants".into()],
    };
    for version in [VERSION, "1.26.144"] {
        let value = serde_json::json!({"validatorVersion":version,"dataVersion":1.3,
            "variants":[{"iframeOnly":true,"playlistTargetDuration":6,
                "messages":[{"errorStatusCode":-50096,"errorDetail":"attribute:HOLD-BACK"}]}]});
        let mut judgement = Judgement::new(
            vec![finding.clone()],
            &Expect {
                full_authoring: true,
                ..Expect::default()
            },
        );
        apply(&value, &mut judgement)?;
        assert_eq!(judgement.defects().len(), 1);
    }
    Ok(())
}

fn he_aac_finding(finding: &crate::report::Finding) -> bool {
    finding.source == Source::Authoring
        && finding.title == "The CODECS attribute MUST include every media format present [#1051]"
        && (finding.scopes == ["All URI Renditions, AAC-LC is not in AAC-HE"]
            || finding.scopes == ["All Renditions, AAC-LC is not in AAC-HE"])
}

fn verified_he_aac(value: &Value) -> bool {
    value["streams"].as_array().is_some_and(|streams| {
        !streams.is_empty()
            && streams
                .iter()
                .all(|stream| stream["codec_name"] == "aac" && stream["profile"] == "HE-AAC")
    })
}

#[test]
fn he_aac_exception_rejects_lc_or_missing_tracks() {
    assert!(verified_he_aac(
        &serde_json::json!({"streams":[{"codec_name":"aac","profile":"HE-AAC"}]})
    ));
    for value in [
        serde_json::json!({"streams":[{"codec_name":"aac","profile":"LC"}]}),
        serde_json::json!({"streams":[]}),
        serde_json::json!({}),
    ] {
        assert!(!verified_he_aac(&value));
    }
}

fn verify_hold_back(objects: &[&Value]) -> Result<bool, String> {
    let affected: Vec<_> = objects
        .iter()
        .filter(|object| {
            object["messages"].as_array().is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| message["errorStatusCode"].as_i64() == Some(-50096))
            })
        })
        .collect();
    // Validate every occurrence before exempting the report's aggregated finding.
    // An actual missing attribute on even one playlist must still fail.
    let mut hold_back_verified = !affected.is_empty();
    for object in affected {
        if !valid_hold_back(object) {
            hold_back_verified = false;
            break;
        }
        let url = object["url"].as_str().ok_or("I-frame finding has no URL")?;
        let playlist = crate::validator::fetch(url, None)?;
        if !valid_playlist(&playlist) {
            return Err(format!("independent HOLD-BACK check failed for {url}"));
        }
    }
    Ok(hold_back_verified)
}

fn verify_tls(value: &Value) -> Result<(), String> {
    let url = value["url"].as_str().ok_or("TLS report has no root URL")?;
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--max-time",
            "20",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code} %{ssl_verify_result}",
        ])
        .arg(url)
        .output()
        .map_err(|error| format!("independent TLS check: {error}"))?;
    if !output.status.success() || output.stdout != b"200 0" {
        return Err(format!(
            "independent TLS verification failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn verify_he_aac(objects: &[&Value]) -> Result<bool, String> {
    let audio: BTreeSet<_> = objects
        .iter()
        .filter(|object| object["playlistMediaType"].as_str() == Some("soun"))
        .filter_map(|object| object["url"].as_str())
        .collect();
    let he_aac_verified = !audio.is_empty();
    for url in audio {
        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-rw_timeout",
                "20000000",
                "-select_streams",
                "a",
                "-show_entries",
                "stream=codec_name,profile",
                "-of",
                "json",
                url,
            ])
            .output()
            .map_err(|error| format!("HE-AAC verification: {error}"))?;
        let data: Value =
            serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
        if !output.status.success() || !verified_he_aac(&data) {
            return Err(format!("independent HE-AAC verification failed for {url}"));
        }
    }
    Ok(he_aac_verified)
}
