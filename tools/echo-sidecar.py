#!/usr/bin/env python3
"""A sidecar that answers Rushls' outbound calls and prints what it was sent.

Stdlib only, one file, no install step. Serves both surfaces at once because in
practice you want to watch a publication end to end: admission at ``/admit``,
lifecycle deliveries at ``/events``.

    ./tools/echo-sidecar.py                 # allow everyone, print everything
    ./tools/echo-sidecar.py --deny          # refuse every publisher
    ./tools/echo-sidecar.py --port 9000

Point Rushls at it with the tables below. They are deliberately separate: you
can run either half on its own.

    [auth]
    provider = "http"

    [auth.http]
    url = "http://127.0.0.1:8081/admit"

    [hooks.endpoints.echo]
    url = "http://127.0.0.1:8081/events"
    events = ["session.started", "stream.available", "session.ended"]

This is a development tool. It has no authentication of its own, answers every
caller, and should not be reachable from anywhere you do not control.
"""

from __future__ import annotations

import argparse
import base64
import json
import sys
import threading
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Rushls refuses a response body over `max_response_bytes`, and reading an
# unbounded request would let a bug here become a memory problem.
MAXIMUM_BODY = 1 << 20

RESET, DIM, BOLD = "\033[0m", "\033[2m", "\033[1m"
GREEN, RED, BLUE, YELLOW = "\033[32m", "\033[31m", "\033[34m", "\033[33m"


class Options:
    """What the operator asked for, shared by every handler thread."""

    def __init__(self, args: argparse.Namespace) -> None:
        self.deny = args.deny
        self.profile = args.profile
        self.stream = args.stream
        self.token = args.token
        self.quiet = args.quiet
        self.colour = sys.stdout.isatty() and not args.no_colour


OPTIONS: Options


def paint(text: str, colour: str) -> str:
    return f"{colour}{text}{RESET}" if OPTIONS.colour else text


def log(tag: str, colour: str, summary: str, body: object = None) -> None:
    stamp = datetime.now(timezone.utc).strftime("%H:%M:%S.%f")[:-3]
    print(f"{paint(stamp, DIM)} {paint(tag, colour)} {summary}", flush=True)
    if body is not None and not OPTIONS.quiet:
        rendered = json.dumps(body, indent=2, sort_keys=True)
        print("\n".join(f"    {line}" for line in rendered.splitlines()), flush=True)


def describe_credential(credential: dict) -> str:
    """Shows the credential as sent, and decoded when it is readable text.

    Rushls base64-encodes it because a publisher may present bytes that are not
    UTF-8 at all; most of the time it is a key someone typed, so show both.
    """
    value = credential.get("value", "")
    if credential.get("encoding") != "base64":
        return repr(value)
    try:
        raw = base64.b64decode(value, validate=True)
    except Exception:
        return f"{value} (undecodable)"
    try:
        return f"{raw.decode()!r}"
    except UnicodeDecodeError:
        return f"{len(raw)} bytes, not text: {raw.hex()}"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self) -> None:  # noqa: N802 - the name is BaseHTTPRequestHandler's
        body = self._read_body()
        if body is None:
            return

        if OPTIONS.token:
            presented = self.headers.get("Authorization", "")
            if presented != f"Bearer {OPTIONS.token}":
                log("AUTH", RED, "rejected: wrong or missing bearer token")
                self._respond(401, {"error": "unauthorized"})
                return

        try:
            payload = json.loads(body)
        except json.JSONDecodeError as error:
            log("BAD", RED, f"body was not JSON: {error}")
            self._respond(400, {"error": "malformed json"})
            return

        if self.path.rstrip("/") == "/admit":
            self._admit(payload)
        elif self.path.rstrip("/") == "/events":
            self._event(payload)
        else:
            log("404", YELLOW, f"nothing serves {self.path}")
            self._respond(404, {"error": "unknown path"})

    def _admit(self, request: dict) -> None:
        resource = request.get("resource", {})
        namespace, name = resource.get("namespace"), resource.get("name", "")
        requested = f"{namespace}/{name}" if namespace else name
        client = request.get("client", {})
        credential = describe_credential(request.get("credential", {}))

        log(
            "ADMIT",
            BLUE,
            f"{request.get('protocol', '?')} {paint(requested, BOLD)} "
            f"from {client.get('remote_address', '?')} with {credential}",
            request,
        )

        if OPTIONS.deny:
            log("DENY", RED, f"refusing {requested}")
            self._respond(200, {"decision": "deny", "reason": "echo_sidecar_denies"})
            return

        # Echoing the requested resource is what makes this a useful default:
        # the publisher lands on the stream it asked for, as the open provider
        # would, while still exercising the whole external-admission path.
        decision = {
            "decision": "allow",
            "stream_id": OPTIONS.stream or requested,
            "principal": "echo-sidecar",
        }
        if OPTIONS.profile:
            decision["profile"] = OPTIONS.profile
        log("ALLOW", GREEN, f"{decision['stream_id']} as {decision['principal']}")
        self._respond(200, decision)

    def _event(self, event: dict) -> None:
        kind = event.get("type", "?")
        data = event.get("data", {})
        detail = ""
        if kind.endswith("session.ended.v1"):
            detail = (
                f" after {data.get('duration_ms', '?')}ms"
                f", {data.get('outcome', '?')}"
                f"{'' if data.get('was_available') else ', never playable'}"
            )
        log(
            "EVENT",
            GREEN,
            f"{paint(kind, BOLD)} {event.get('subject', '?')}{detail} "
            f"{paint(event.get('id', ''), DIM)}",
            event,
        )
        # 204 rather than 200: there is nothing to say back, and answering with
        # a body a consumer does not read is just bytes on the wire.
        self._respond(204, None)

    def _read_body(self) -> bytes | None:
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self._respond(400, {"error": "bad content-length"})
            return None
        if length > MAXIMUM_BODY:
            self._respond(413, {"error": "body too large"})
            return None
        return self.rfile.read(length)

    def _respond(self, status: int, payload: dict | None) -> None:
        body = b"" if payload is None else json.dumps(payload).encode()
        self.send_response(status)
        if body:
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)

    def log_message(self, *_args) -> None:
        """Silences the default access log; this tool prints its own."""


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Echo sidecar for Rushls admission and lifecycle hooks.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8081)
    parser.add_argument(
        "--deny", action="store_true", help="refuse every publisher, to test failure"
    )
    parser.add_argument(
        "--stream", help="admit every publisher to this stream instead of the one asked for"
    )
    parser.add_argument("--profile", help="name a publish profile in the allow response")
    parser.add_argument(
        "--token", help="require this bearer token, matching `token` in the config"
    )
    parser.add_argument("--quiet", action="store_true", help="one line per request")
    parser.add_argument("--no-colour", action="store_true")
    args = parser.parse_args()

    global OPTIONS
    OPTIONS = Options(args)

    # Threaded so a hook delivery is not held up behind an admission, which is
    # what Rushls itself does and what the ordering guarantees assume.
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    server.daemon_threads = True

    where = f"http://{args.host}:{args.port}"
    print(f"{paint('admission', BLUE)}  {where}/admit", flush=True)
    print(f"{paint('lifecycle', GREEN)}  {where}/events", flush=True)
    if args.deny:
        print(paint("denying every publisher", RED), flush=True)
    print(paint("ctrl-c to stop", DIM), flush=True)

    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        thread.join()
    except KeyboardInterrupt:
        print("\n" + paint("stopped", DIM), flush=True)
        server.shutdown()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
