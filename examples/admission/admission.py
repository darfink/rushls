#!/usr/bin/env python3
"""A minimal Rushls admission service: a stream-key allowlist.

Each publisher presents a stream key. This service looks it up in a TOML file
and admits the publisher to the stream ID the file assigns, or refuses it.
The public stream name therefore never has to be the secret key.

    ./examples/admission/admission.py examples/admission/keys.toml

Point Rushls at it:

    [publish.auth]
    url = "http://127.0.0.1:8090/admit"
    token = { file = "/run/secrets/admission-token" }

Set the same token here with ADMISSION_TOKEN, so only Rushls can ask. Where
the key comes from depends on the protocol:

    RTMP   rtmp://origin/live/<key>                  (the stream name)
    SRT    streamid=publish:<key>                    (or publish:<name>:<key>)
    MoQ    ?token=<key> on the connect URL

Stdlib only (Python 3.11+). The keys file is read on every request, so edits
take effect immediately. It is an example: a real service would keep keys in a
database, hash them, and rate-limit failed attempts.
"""

from __future__ import annotations

import argparse
import base64
import hmac
import json
import os
import sys
import tomllib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MAXIMUM_BODY = 64 * 1024


def presented_key(request: dict) -> str | None:
    """The credential as text; Rushls always sends it base64-encoded."""
    credential = request.get("credential") or {}
    try:
        return base64.b64decode(credential.get("value", ""), validate=True).decode()
    except (ValueError, UnicodeDecodeError):
        return None


def decide(request: dict, keys: dict) -> dict:
    key = presented_key(request)
    # Compare against every entry in constant time, so response timing does
    # not reveal how much of a guessed key was right.
    match = None
    for candidate, entry in keys.items():
        if key is not None and hmac.compare_digest(candidate.encode(), key.encode()):
            match = entry
    if match is None or match.get("disabled", False):
        return {"decision": "deny", "reason": "unknown_or_disabled_key"}
    decision = {
        "decision": "allow",
        "stream_id": match["stream"],
        "principal": match.get("principal", match["stream"]),
    }
    if "profile" in match:
        decision["profile"] = match["profile"]
    return decision


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    keys_file: Path
    token: str | None

    def do_POST(self) -> None:  # noqa: N802 - the name is BaseHTTPRequestHandler's
        if self.path.rstrip("/") != "/admit":
            return self.respond(404, {"error": "unknown path"})
        if self.token is not None and not hmac.compare_digest(
            self.headers.get("Authorization", ""), f"Bearer {self.token}"
        ):
            return self.respond(401, {"error": "unauthorized"})
        length = int(self.headers.get("Content-Length", "0") or 0)
        if length > MAXIMUM_BODY:
            return self.respond(413, {"error": "body too large"})
        try:
            request = json.loads(self.rfile.read(length))
            keys = tomllib.loads(self.keys_file.read_text()).get("keys", {})
        except (ValueError, OSError) as error:
            # Anything but a 200 makes Rushls refuse the publisher: fail closed.
            print(f"error: {error}", file=sys.stderr, flush=True)
            return self.respond(500, {"error": "unavailable"})
        decision = decide(request, keys)
        client = (request.get("client") or {}).get("remote_address", "?")
        print(
            f"{request.get('protocol', '?')} {client}: {decision['decision']}"
            f" {decision.get('stream_id', decision.get('reason', ''))}",
            flush=True,
        )
        return self.respond(200, decision)

    def respond(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_args) -> None:
        """Silences the default access log; `do_POST` prints one line per decision."""


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("keys", type=Path, help="TOML file with a [keys] table")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8090)
    args = parser.parse_args()

    Handler.keys_file = args.keys
    Handler.token = os.environ.get("ADMISSION_TOKEN")
    if Handler.token is None:
        print("warning: ADMISSION_TOKEN is unset; anyone can query this service", flush=True)
    tomllib.loads(args.keys.read_text())  # refuse to start on a malformed file

    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"admission at http://{args.host}:{args.port}/admit", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
