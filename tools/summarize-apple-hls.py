#!/usr/bin/env python3
"""Render the original two-hour Apple report as a GitHub job summary."""
import argparse
import html
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import re


class Findings(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.rows = []
        self.section = "General requirements"
        self.severity = None
        self.capture = None
        self.text = []

    def handle_starttag(self, tag, attrs):
        if tag in ("h2", "h3", "h4", "li"):
            self.capture = tag
            self.text = []

    def handle_data(self, data):
        if self.capture:
            self.text.append(data)

    def handle_endtag(self, tag):
        if tag != self.capture:
            return
        text = " ".join("".join(self.text).split())
        self.capture = None
        if tag == "h2":
            self.section = text
            self.severity = None
        elif tag == "h3":
            # Preserve Apple's severity labels, including the 1.26 split.
            self.severity = text if any(word in text for word in (
                "Must Fix Issues", "Should Fix Issues", "Advisories",
                "Requirements with no validation performed",
            )) else None
        elif tag == "h4" and self.severity:
            self.rows.append([self.section, self.severity, re.sub(r"^\d+\.\s*", "", text), []])
        elif tag == "li" and self.severity and self.rows:
            self.rows[-1][3].append(text)


def escape(value):
    text = html.escape(str(value), quote=False)
    return re.sub(r"([\\`*_{}\[\]()#+.!|])", r"\\\1", text).replace("\n", " ")


def summarize(directory, outcome="not recorded", artifact_url=""):
    directory = Path(directory)
    stem = directory / "two_hour_live_authoring"
    lines = ["## Two-hour Apple HLS audit", "", f"**Test outcome:** {escape(outcome)}", ""]
    if artifact_url.startswith("https://"):
        lines += [f"[Download original HTML, JSON, and playlists](<{artifact_url}>)", ""]
    report = stem.with_suffix(".html")
    if not report.exists():
        return "\n".join(lines + ["**No two-hour HTML report was produced.** Check the setup and audit logs.", ""])
    data_path = stem.with_suffix(".json")
    if data_path.exists():
        data = json.loads(data_path.read_text())
        lines += [f"Validator version: {escape(data.get('validatorVersion', 'unknown'))}", ""]
    windows = stem.with_suffix(".windows.json")
    if windows.exists():
        values = json.loads(windows.read_text())
        if values:
            lines += [f"Retained playlists: **{len(values)}**. Shortest DVR window: **{min(v['seconds'] for v in values):.3f} seconds**.", ""]
    trace = stem.with_suffix(".requests.jsonl")
    if trace.exists():
        requests = errors = 0
        with trace.open() as source:
            for line in source:
                entry = json.loads(line)
                requests += 1
                errors += bool(entry.get("error"))
        lines += [f"HTTP requests: **{requests:,}**. Delivery errors: **{errors:,}**.", ""]
    parsed = Findings()
    parsed.feed(report.read_text())
    lines += ["### Apple's findings", "", "These findings are copied from the original report without exemptions or severity changes.", ""]
    if parsed.rows:
        lines += ["| Rule set | Severity | Finding | Applies to |", "| --- | --- | --- | --- |"]
        for section, severity, title, scopes in parsed.rows:
            lines.append("| " + " | ".join(map(escape, (section, severity, title, "; ".join(scopes)))) + " |")
    else:
        lines += ["No findings were extracted. Consult the original HTML; this summary alone does not establish conformance."]
    lines += ["", "The downloadable HTML also contains the bitrate and rendition overview tables.", ""]
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--output", type=Path, default=os.environ.get("GITHUB_STEP_SUMMARY"))
    args = parser.parse_args()
    text = summarize(args.directory, os.environ.get("AUDIT_OUTCOME", "not recorded"), os.environ.get("ARTIFACT_URL", ""))
    # GitHub limits each step summary to 1 MiB. Never silently omit findings.
    if len(text.encode()) > 1_000_000:
        raise SystemExit("Apple report summary exceeds the GitHub size limit; use the original HTML artifact.")
    if args.output:
        with args.output.open("a") as output:
            output.write(text)
    else:
        print(text)


if __name__ == "__main__":
    main()
