//! Apple's conformance findings, and what this origin is answerable for.
//!
//! Two tools look at the same publication. `mediastreamvalidator` checks the
//! stream against the HLS specification and reports through `messages` arrays
//! in its JSON. `hlsreport` reads that JSON and checks it against the *HLS
//! Authoring Specification for Apple Devices*, which is a different document
//! with a different audience: much of it is advice to whoever designs a bitrate
//! ladder, not to whoever writes the packager.
//!
//! That difference is the whole reason this module exists. "You MUST provide
//! multiple bit rates of video" is a true statement about a shippable service
//! and says nothing about whether this origin packaged its single input
//! correctly. "Partial segments that contain a sync frame SHOULD be marked
//! INDEPENDENT" is entirely about the packager. Treating both as pass/fail
//! makes the suite either useless or permanently red, so each finding is
//! judged against what the case under test deliberately published.
//!
//! # Reading a failure
//!
//! Findings are printed in full — including the ones that did not fail the
//! test — so a run answers "what is still missing?" and not only "did it
//! pass?". Set `RUSHLS_TEST_REPORT_DIR` to keep each case's JSON and HTML.

use std::fmt::Write as _;

/// What a case deliberately published, so an inapplicable finding can be told
/// apart from a defect.
///
/// Every field is a claim the test makes about its own input. A case that sets
/// `subtitles` is promising there is a subtitle rendition, which turns "captions
/// SHOULD be provided" from noise into a genuine failure.
//
// Several flags rather than an enum because they are independent: a case can
// publish a ladder, captions, and an unlisted codec at once, and each answers a
// different finding.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug)]
pub struct Expect {
    /// Names the case in printed output and in `RUSHLS_TEST_REPORT_DIR` files.
    pub name: &'static str,
    /// A subtitle or closed-caption rendition is published.
    pub captions: bool,
    /// More than one video rendition is published.
    pub ladder: bool,
    /// No video track is published.
    pub audio_only: bool,
    /// Delivery is over TLS, so the transport findings are real.
    pub tls: bool,
    /// The cadence under test is deliberately not Apple's recommended one.
    ///
    /// True for almost every case: six-second targets would make each test
    /// wait half a minute for enough segments to validate.
    pub unconventional_cadence: bool,
    /// The case publishes a codec Apple's HLS profile does not list.
    ///
    /// Packaging it correctly and Apple being willing to play it are separate
    /// questions. A case that sets this is asking the first one.
    pub unlisted_codec: bool,
}

impl Default for Expect {
    fn default() -> Self {
        Self {
            name: "apple-hls",
            captions: false,
            ladder: false,
            audio_only: false,
            tls: false,
            unconventional_cadence: true,
            unlisted_codec: false,
        }
    }
}

impl Expect {
    pub fn captions(mut self) -> Self {
        self.captions = true;
        self
    }

    pub fn ladder(mut self) -> Self {
        self.ladder = true;
        self
    }

    pub fn audio_only(mut self) -> Self {
        self.audio_only = true;
        self
    }

    pub fn apple_cadence(mut self) -> Self {
        self.unconventional_cadence = false;
        self
    }

    pub fn unlisted_codec(mut self) -> Self {
        self.unlisted_codec = true;
        self
    }
}

/// Which tool raised a finding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    /// A `messages` entry in the validator's JSON: HLS specification conformance.
    Validator,
    /// An entry in `hlsreport`'s HTML: Apple authoring guidance.
    Authoring,
}

/// How Apple's own report files a finding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Level {
    MustFix,
    ShouldFix,
    /// Listed under "Requirements with no validation performed": Apple could
    /// not check it, so it is never a failure, only a reminder.
    NotChecked,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub source: Source,
    pub level: Level,
    /// The `hlsreport` heading a finding sits under, or the JSON path the
    /// validator message came from.
    pub context: String,
    pub title: String,
    /// Which playlists the finding applies to, as `hlsreport` scopes them.
    pub scopes: Vec<String>,
}

impl Finding {
    pub fn describe(&self) -> String {
        let mut text = format!("[{}] {}", self.context, self.title);
        if !self.scopes.is_empty() {
            let _ = write!(text, " ({})", self.scopes.join("; "));
        }
        text
    }

    /// The same text, wrapped onto a second line when a URL makes it long.
    pub fn describe_indented(&self) -> String {
        let mut text = self.title.clone();
        let _ = write!(text, "\n      at {}", self.context);
        if !self.scopes.is_empty() {
            let _ = write!(text, "\n      scope: {}", self.scopes.join("; "));
        }
        text
    }
}

/// What the suite does about a finding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// The origin got this wrong. Fails the test.
    Defect,
    /// True of this origin, but a deliberate scope decision rather than a bug.
    /// Printed on every run so the gap stays visible; never fails.
    Gap(&'static str),
    /// An artefact of how the test publishes or is served, not of the origin.
    NotApplicable(&'static str),
}

/// Findings decided by whoever configures an encoder, not by this origin.
///
/// The origin packages the renditions it is given and invents none, so a rule
/// about which bitrates or profiles *should exist* is addressed to somebody
/// else. Kept as one table because the distinction that matters is who the rule
/// is for, not which section of Apple's document it appears in.
const PUBLISHER_CHOICES: [&str; 14] = [
    "multiple bit rates",
    "peak bandwidth is less than or equal",
    "default video variant",
    "stream failover",
    "full range of variants",
    "average bandwidth",
    "variant whose peak bandwidth",
    "should be the 2000 kb/s",
    "should be the 730 kb/s",
    "key frames (idrs) should be present",
    "should use high profile",
    "backward compatibility",
    "stereo audio in aac-lc",
    "should be encoded with",
];

/// Findings squarely about what this origin wrote into a playlist or segment.
const PACKAGER_DUTIES: [&str; 18] = [
    "partial segment",
    "independent",
    "did not refresh",
    "ext-x-map",
    "part-hold-back",
    "hold-back",
    "sync frame",
    "playlist attribute",
    "target duration",
    "duration of each media segment",
    "media sequence",
    "codecs attribute",
    "framerate change",
    "discontinuity",
    "must have the same set of members",
    "video resolution",
    "video range",
    "autoselected languages",
];

/// Decides what a finding means for the case that produced it.
///
/// Unrecognised findings are [`Verdict::Gap`] rather than [`Verdict::Defect`]:
/// Apple ships new authoring rules between tool releases, and a rule nobody has
/// read yet should make a run noisy rather than red. `RUSHLS_TEST_HLSREPORT`
/// set to `strict` promotes every unrecognised authoring finding to a defect,
/// which is how the table below is kept honest.
///
/// Validator messages are judged the other way round: the default for an
/// unrecognised one is [`Verdict::Defect`], because that tool reports
/// specification conformance and this origin is answerable for all of it.
pub fn judge(finding: &Finding, expect: &Expect) -> Verdict {
    let title = finding.title.to_ascii_lowercase();
    let scopes = finding.scopes.join("; ").to_ascii_lowercase();

    conditional(&title, &scopes, expect)
        .or_else(|| unimplemented_feature(&title))
        .or_else(|| {
            PACKAGER_DUTIES
                .iter()
                .any(|needle| title.contains(needle))
                .then_some(Verdict::Defect)
        })
        .unwrap_or_else(|| unrecognised(finding))
}

/// Findings whose meaning depends on what the case deliberately published.
fn conditional(title: &str, scopes: &str, expect: &Expect) -> Option<Verdict> {
    // A fetch that failed is never guidance. It is the origin answering a
    // request wrongly, and every finding below it is unreliable as a result.
    if title.contains("http 4") || title.contains("http 5") || title.contains("kcferrordomain") {
        return Some(Verdict::Defect);
    }

    // Codecs Apple does not carry in HLS. Whether this origin packaged one
    // correctly is a question its own tests answer; Apple can only say it will
    // not play it, which a case that chose the codec already knows.
    if title.contains("unrecognized codec")
        || title.contains("unsupported audio track codec")
        || title.contains("extraneous codecs")
        || title.contains("supported stereo audio formats are")
    {
        return Some(when(
            expect.unlisted_codec,
            "the case publishes a codec Apple's HLS profile omits",
        ));
    }

    // An audio-only presentation has nowhere else to put its audio. Apple's
    // rule is written for a presentation that also has video, where an
    // audio-only variant would be selectable by mistake.
    if title.contains("no audio-only variants") {
        return Some(when(
            expect.audio_only,
            "the case publishes no video at all",
        ));
    }

    // Transport. True of any cleartext run and of nothing else.
    if title.contains("http/2")
        || (title.contains("tls") && title.contains("should"))
        || title.contains("transport layer security")
    {
        return Some(when(!expect.tls, "the suite runs cleartext; see certs.rs"));
    }

    // Cadence. The suite runs short targets so a case finishes in seconds.
    if title.contains("target durations should be")
        || title.contains("recommended part target duration")
    {
        return Some(when(
            expect.unconventional_cadence,
            "the case deliberately uses a non-Apple cadence",
        ));
    }

    // Accessibility. Only meaningful when the case published captions.
    if title.contains("captions should be provided") {
        return Some(when(
            !expect.captions,
            "the case publishes no caption or subtitle track",
        ));
    }

    if PUBLISHER_CHOICES
        .iter()
        .any(|needle| title.contains(needle))
    {
        return Some(if expect.ladder {
            Verdict::Gap("what to encode is the publisher's choice, not this origin's")
        } else {
            Verdict::NotApplicable("the case deliberately publishes one encoding")
        });
    }

    // Apple's own segment MIME table gives text/vtt for WebVTT, which is what
    // this origin serves and what hlsreport reports as wrong. Its expectation of
    // text/plain cannot be reconciled with the specification it is checking, so
    // a subtitle-only occurrence is reported and not failed. An audio rendition
    // served as video/iso.segment is unambiguous.
    if title.contains("mime type") {
        return Some(
            if scopes.contains("subtitle") && !scopes.contains("audio") {
                Verdict::Gap(
                    "hlsreport expects text/plain for WebVTT; Apple's own table says text/vtt",
                )
            } else {
                Verdict::Defect
            },
        );
    }

    None
}

/// Not applicable when the case's own claim makes it so, a defect otherwise.
fn when(inapplicable: bool, reason: &'static str) -> Verdict {
    if inapplicable {
        Verdict::NotApplicable(reason)
    } else {
        Verdict::Defect
    }
}

/// Features this origin does not implement at all.
///
/// Named individually so the list doubles as the roadmap Apple would like this
/// origin to have, rather than collapsing into one "not supported" bucket that
/// says nothing about what is missing.
fn unimplemented_feature(title: &str) -> Option<Verdict> {
    let reason = if title.contains("i-frame playlist") || title.contains("ext-x-i-frame-stream-inf")
    {
        "no EXT-X-I-FRAME-STREAM-INF is produced"
    } else if title.contains("ext-x-daterange") {
        "no EXT-X-DATERANGE is produced"
    } else if title.contains("dynamic range control") {
        "no DRC metadata is carried through"
    } else if title.contains("content steering") || title.contains("pathway") {
        "no content steering is produced"
    } else if title.contains("loudness") {
        "no loudness metadata is carried through"
    } else if title.contains("ext-x-playlist-type") {
        "no EXT-X-PLAYLIST-TYPE is produced"
    } else if title.contains("in a vod playlist") {
        // Both VOD findings are consequences of one decision: an ended
        // publication keeps the low-latency tags it was serving while live.
        "low-latency tags are retained after EXT-X-ENDLIST"
    } else {
        return None;
    };
    Some(Verdict::Gap(reason))
}

fn unrecognised(finding: &Finding) -> Verdict {
    if finding.level == Level::NotChecked {
        return Verdict::Gap("Apple performed no validation for this requirement");
    }
    match finding.source {
        // Specification conformance: unrecognised means unreviewed, and this
        // origin is answerable for the whole document.
        Source::Validator => Verdict::Defect,
        Source::Authoring if authoring_mode() == AuthoringMode::Strict => Verdict::Defect,
        Source::Authoring => Verdict::Gap("unclassified authoring finding"),
    }
}

/// How much of Apple's authoring guidance a run is willing to fail on.
///
/// `RUSHLS_TEST_HLSREPORT`: `off` skips `hlsreport` entirely and validates
/// against the specification alone, which is the smallest useful failure set.
/// `strict` fails on every authoring finding this table does not recognise,
/// which is how the table is kept from going stale. The default is between the
/// two: recognised authoring defects fail, unrecognised ones are printed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthoringMode {
    Off,
    Default,
    Strict,
}

pub fn authoring_mode() -> AuthoringMode {
    match std::env::var("RUSHLS_TEST_HLSREPORT") {
        Ok(value) if value.eq_ignore_ascii_case("off") => AuthoringMode::Off,
        Ok(value) if value.eq_ignore_ascii_case("strict") => AuthoringMode::Strict,
        _ => AuthoringMode::Default,
    }
}

/// Every finding for one case, with the verdict each was given.
pub struct Judgement {
    pub findings: Vec<(Finding, Verdict)>,
}

impl Judgement {
    pub fn new(findings: Vec<Finding>, expect: &Expect) -> Self {
        Self {
            findings: findings
                .into_iter()
                .map(|finding| {
                    let verdict = judge(&finding, expect);
                    (finding, verdict)
                })
                .collect(),
        }
    }

    pub fn defects(&self) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|(_, verdict)| *verdict == Verdict::Defect)
            .map(|(finding, _)| finding)
            .collect()
    }

    /// The whole picture, defects included, for a human reading a test run.
    ///
    /// Grouped by verdict rather than by the order Apple reported them, so the
    /// defects are together at the top and the rest reads as a list of what
    /// this origin has decided not to do.
    pub fn render(&self, expect: &Expect) -> String {
        let mut text = format!("=== Apple conformance report: {} ===\n", expect.name);
        if self.findings.is_empty() {
            text.push_str("no findings\n");
            return text;
        }
        for wanted in [Label::Defect, Label::Gap, Label::NotApplicable] {
            for (finding, verdict) in &self.findings {
                if Label::of(verdict) != wanted {
                    continue;
                }
                let reason = match verdict {
                    Verdict::Gap(reason) | Verdict::NotApplicable(reason) => format!(" — {reason}"),
                    Verdict::Defect => String::new(),
                };
                let _ = writeln!(
                    text,
                    "  {:<6} {}{reason}",
                    wanted.text(),
                    finding.describe()
                );
            }
        }
        text
    }
}

/// The column a verdict prints under, which is also how findings are grouped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Label {
    Defect,
    Gap,
    NotApplicable,
}

impl Label {
    fn of(verdict: &Verdict) -> Self {
        match verdict {
            Verdict::Defect => Self::Defect,
            Verdict::Gap(_) => Self::Gap,
            Verdict::NotApplicable(_) => Self::NotApplicable,
        }
    }

    fn text(self) -> &'static str {
        match self {
            Self::Defect => "DEFECT",
            Self::Gap => "GAP",
            Self::NotApplicable => "N/A",
        }
    }
}

/// Extracts findings from `hlsreport`'s HTML.
///
/// Hand-written rather than a parser dependency: the document is generated by
/// one tool with a fixed shape — `h2` names a rule set, `h3` names a severity,
/// `h4` names a finding, and the `ul` after it names the playlists it applies
/// to. Everything after the "Detailed Output" heading is per-segment tables
/// with no findings in them, so scanning stops there.
pub fn parse_hlsreport(html: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut rule_set = String::from("General requirements");
    let mut level = None;
    for (tag, body) in tags(html) {
        match tag {
            "h2" => {
                if body.starts_with("HLS Validation Report") {
                    continue;
                }
                rule_set = body;
                level = None;
            }
            "h3" => {
                level = match body.as_str() {
                    "Must Fix Issues" => Some(Level::MustFix),
                    "Should Fix Issues" => Some(Level::ShouldFix),
                    "Requirements with no validation performed" => Some(Level::NotChecked),
                    // "Detailed Output - Variants" and everything after it.
                    _ => None,
                };
            }
            "h4" => {
                if let Some(level) = level {
                    findings.push(Finding {
                        source: Source::Authoring,
                        level,
                        context: rule_set.clone(),
                        title: strip_index(&body),
                        scopes: Vec::new(),
                    });
                }
            }
            "li" => {
                if level.is_some()
                    && let Some(finding) = findings.last_mut()
                {
                    finding.scopes.push(body);
                }
            }
            _ => {}
        }
    }
    findings
}

/// Drops the "12. " ordinal `hlsreport` numbers each finding with.
///
/// The number is a position in one report and changes whenever an earlier
/// finding appears or disappears, so keeping it would make the classification
/// table depend on how many *other* things went wrong.
fn strip_index(title: &str) -> String {
    match title.split_once(". ") {
        Some((index, rest)) if index.chars().all(|c| c.is_ascii_digit()) => rest.trim().to_owned(),
        _ => title.trim().to_owned(),
    }
}

/// Yields `(tag, text)` for the few block elements findings live in.
fn tags(html: &str) -> Vec<(&'static str, String)> {
    const WANTED: [&str; 4] = ["h2", "h3", "h4", "li"];
    let mut out = Vec::new();
    let bytes = html.as_bytes();
    let mut cursor = 0;
    while let Some(open) = html[cursor..].find('<') {
        let start = cursor + open;
        let Some(close) = html[start..].find('>') else {
            break;
        };
        let inner = &html[start + 1..start + close];
        cursor = start + close + 1;
        let name = inner
            .split([' ', '\t', '\n', '>'])
            .next()
            .unwrap_or_default();
        let Some(tag) = WANTED.iter().find(|wanted| **wanted == name) else {
            continue;
        };
        let closing = format!("</{tag}>");
        let Some(end) = html[cursor..].find(&closing) else {
            continue;
        };
        out.push((*tag, text_of(&html[cursor..cursor + end])));
        cursor += end + closing.len();
        let _ = bytes;
    }
    out
}

/// Strips nested markup and decodes the entities `hlsreport` emits.
fn text_of(fragment: &str) -> String {
    let mut text = String::with_capacity(fragment.len());
    let mut depth = 0_usize;
    for character in fragment.chars() {
        match character {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => text.push(character),
            _ => {}
        }
    }
    decode_entities(text.trim())
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        rest = &rest[index..];
        let Some(end) = rest.find(';').filter(|end| *end <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix('#')
                .and_then(|number| match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        if let Some(character) = replacement {
            out.push(character);
            rest = &rest[end + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}
