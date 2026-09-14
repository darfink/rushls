#!/usr/bin/env python3
"""One bounded public-path HLS probe. Prints Prometheus text; never prints URLs or tokens."""
import argparse
import datetime
import re
import time
import urllib.parse
import urllib.request
from pathlib import Path

ATTR = re.compile(r'([A-Z0-9-]+)=(?:"([^"]*)"|([^,]*))')


def attributes(line):
    return {key: quoted or plain for key, quoted, plain in ATTR.findall(line.partition(":")[2])}


def origin(url):
    parsed = urllib.parse.urlsplit(url)
    return parsed.scheme, parsed.hostname, parsed.port


class Redirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, newurl):
        redirected = super().redirect_request(request, fp, code, message, headers, newurl)
        if redirected and origin(request.full_url) != origin(newurl):
            redirected.remove_header("Authorization")
        return redirected


class Probe:
    def __init__(self, url, timeout, token=None):
        self.url = url
        self.timeout = timeout
        self.token = token
        self.opener = urllib.request.build_opener(Redirect())

    def fetch(self, url, limit, keep=False):
        if urllib.parse.urlsplit(url).scheme not in ("http", "https"):
            raise ValueError("unsupported URL scheme")
        headers = {"Accept-Encoding": "identity", "User-Agent": "rushls-playback-probe"}
        if self.token and origin(url) == origin(self.url):
            headers["Authorization"] = "Bearer " + self.token
        started = time.monotonic()
        count = 0
        parts = []
        with self.opener.open(urllib.request.Request(url, headers=headers), timeout=self.timeout) as response:
            resolved = response.url
            while chunk := response.read1(min(64 * 1024, limit + 1 - count)):
                if time.monotonic() - started > self.timeout:
                    raise TimeoutError("response deadline exceeded")
                count += len(chunk)
                if count > limit:
                    raise ValueError("response limit exceeded")
                if keep:
                    parts.append(chunk)
        return resolved, b"".join(parts), count, time.monotonic() - started


def resolve(base, uri):
    # Rushls can declare token as EXT-X-DEFINE:QUERYPARAM="token".
    query = urllib.parse.parse_qs(urllib.parse.urlsplit(base).query)
    for key, values in query.items():
        uri = uri.replace("{$" + key + "}", urllib.parse.quote(values[0], safe=""))
    if "{$" in uri:
        raise ValueError("unresolved HLS variable")
    return urllib.parse.urljoin(base, uri)


def playlists(base, text):
    result = []
    variant = False
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("#EXT-X-STREAM-INF:"):
            variant = True
        elif line.startswith("#EXT-X-MEDIA:"):
            attrs = attributes(line)
            if attrs.get("TYPE") != "SUBTITLES" and attrs.get("URI"):
                result.append(resolve(base, attrs["URI"]))
        elif line and not line.startswith("#") and variant:
            result.append(resolve(base, line))
            variant = False
    return list(dict.fromkeys(result))


def latest_media(base, text):
    latest = None
    latest_end = None
    wall = None
    part_duration = 0.0
    duration = 0.0
    gap = False
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("#EXT-X-PROGRAM-DATE-TIME:"):
            stamp = datetime.datetime.fromisoformat(line.partition(":")[2].replace("Z", "+00:00"))
            wall = stamp.timestamp() if stamp.tzinfo else None
        elif line.startswith("#EXT-X-PART:"):
            attrs = attributes(line)
            part_duration += float(attrs.get("DURATION", "0"))
            if attrs.get("GAP") != "YES" and "URI" in attrs:
                latest = resolve(base, attrs["URI"])
                latest_end = None if wall is None else wall + part_duration
        elif line.startswith("#EXTINF:"):
            duration = float(line.partition(":")[2].split(",")[0])
        elif line == "#EXT-X-GAP":
            gap = True
        elif line and not line.startswith("#"):
            if not gap:
                latest = resolve(base, line)
                latest_end = None if wall is None else wall + duration
            if wall is not None:
                wall += duration
            part_duration = 0.0
            gap = False
    return latest, latest_end, "#EXT-X-ENDLIST" in text


def run(url, timeout, token, maximum_renditions, maximum_media_bytes):
    probe = Probe(url, timeout, token)
    base, payload, _, initial_duration = probe.fetch(url, 2 * 1024 * 1024, keep=True)
    text = payload.decode("utf-8")
    if not text.startswith("#EXTM3U"):
        raise ValueError("not an HLS playlist")
    children = playlists(base, text)
    if len(children) > maximum_renditions:
        raise ValueError("rendition limit exceeded")
    rows = []
    for index, child in enumerate(children or [base]):
        if children:
            child, payload, _, duration = probe.fetch(child, 2 * 1024 * 1024, keep=True)
            text = payload.decode("utf-8")
        else:
            duration = initial_duration
        if not text.startswith("#EXTM3U"):
            raise ValueError("not an HLS media playlist")
        media, end, ended = latest_media(child, text)
        if media is None:
            raise ValueError("no published media")
        maps = [attributes(line).get("URI") for line in text.splitlines() if line.startswith("#EXT-X-MAP:")]
        if maps and maps[-1]:
            probe.fetch(resolve(child, maps[-1]), maximum_media_bytes)
        _, _, size, media_duration = probe.fetch(media, maximum_media_bytes)
        labels = f'rendition="{index}"'
        rows.extend([
            ("rushls_probe_playlist_request_duration_seconds", labels, duration),
            ("rushls_probe_media_request_duration_seconds", labels, media_duration),
            ("rushls_probe_media_bytes", labels, size),
            ("rushls_probe_playlist_ended", labels, int(ended)),
        ])
        if end is not None:
            rows.append(("rushls_probe_media_end_timestamp_seconds", labels, end))
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("url", help="Public media or multivariant playlist URL")
    parser.add_argument("--timeout", type=float, default=5)
    parser.add_argument("--bearer-token-file", type=Path)
    parser.add_argument("--maximum-renditions", type=int, default=32)
    parser.add_argument("--maximum-media-bytes", type=int, default=64 * 1024 * 1024)
    args = parser.parse_args()
    if args.timeout <= 0 or args.maximum_renditions <= 0 or args.maximum_media_bytes <= 0:
        parser.error("limits must be positive")
    started = time.monotonic()
    try:
        token = args.bearer_token_file.read_text().strip() if args.bearer_token_file else None
        rows = run(args.url, args.timeout, token, args.maximum_renditions, args.maximum_media_bytes)
        success = 1
    except Exception:
        # Errors can embed signed URLs. The success gauge conveys failure without secrets.
        rows = []
        success = 0
    rows.extend([
        ("rushls_probe_success", "", success),
        ("rushls_probe_duration_seconds", "", time.monotonic() - started),
        ("rushls_probe_timestamp_seconds", "", time.time()),
    ])
    families = sorted({name for name, _, _ in rows})
    for name in families:
        print(f"# TYPE {name} gauge")
        for metric, labels, value in rows:
            if metric == name:
                label_set = "{" + labels + "}" if labels else ""
                print(f"{metric}{label_set} {value}")
    return 0 if success else 1


if __name__ == "__main__":
    raise SystemExit(main())
