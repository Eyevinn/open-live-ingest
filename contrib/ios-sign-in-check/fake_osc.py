#!/usr/bin/env python3
"""A fake Open Source Cloud for the simulator sign-in check (docs/specs/ios-app.md,
sections 6 and 14 Q4), run by the CI job `ios-sign-in` on the macOS runner.

The iOS Simulator shares the runner's network, so the app's core reaches this fake
at http://127.0.0.1:<port> (OLI_TEST_OSC_URL, read by debug builds of the core only),
and the authentication sheet's web view reaches the core's own loopback listener.

The authorize endpoint answers with a 302 straight to the redirect_uri, with a code
and the state: there is no login page, so the check covers exactly the question
asked, whether ASWebAuthenticationSession's out-of-process web view reaches
127.0.0.1:<port>. The other endpoints mirror mobile/src/testing.rs's FakeOsc.

Usage: fake_osc.py <port>. Standard library only; it logs paths, never a query.
"""

import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlsplit

CODE = "sign-in-check-code"
# {"tenantId":"sign-in-check"}, the shape osc::inspect reads a workspace from.
ACCESS = "h.eyJ0ZW5hbnRJZCI6InNpZ24taW4tY2hlY2sifQ.s"
REFRESH = "sign-in-check-refresh"
SERVICE_TOKEN = "sign-in-check-service"


class Fake(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("fake-osc: " + (fmt % args) + "\n")
        sys.stderr.flush()

    def log_request(self, code="-", size="-"):
        # The path only: a query holds the state and the code.
        self.log_message("%s %s %s", self.command, urlsplit(self.path).path, code)

    def reply(self, status, body=None, headers=None):
        data = b"" if body is None else json.dumps(body).encode()
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        if body is not None:
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def base(self):
        return "http://" + self.headers.get("Host", "127.0.0.1")

    def do_GET(self):
        url = urlsplit(self.path)
        query = parse_qs(url.query)
        if url.path == "/health":
            return self.reply(200, {"ok": True})
        if url.path == "/api/connect/authorize":
            redirect = (query.get("redirect_uri") or [""])[0]
            state = (query.get("state") or [""])[0]
            if not redirect.startswith("http://127.0.0.1:") or not state:
                return self.reply(400, {"error": "invalid_request"})
            sys.stderr.write("fake-osc: authorize -> 302 to the app's loopback listener\n")
            location = redirect + "?" + urlencode({"code": CODE, "state": state})
            return self.reply(302, None, {"Location": location, "Cache-Control": "no-store"})
        if url.path == "/mysubscriptions" and self.headers.get("x-pat-jwt") == "Bearer " + ACCESS:
            return self.reply(200, [{"serviceId": "eyevinn-open-live",
                                     "apiUrl": self.base() + "/instances"}])
        if url.path == "/instances" and self.headers.get("x-jwt") == "Bearer " + SERVICE_TOKEN:
            return self.reply(200, [])
        return self.reply(404, {})

    def do_POST(self):
        url = urlsplit(self.path)
        raw = self.body()
        if url.path == "/api/connect/register":
            return self.reply(201, {"client_id": "sign-in-check",
                                    "token_endpoint_auth_method": "none"})
        if url.path == "/api/connect/token":
            form = parse_qs(raw.decode("utf-8", "replace"))
            grant = (form.get("grant_type") or [""])[0]
            code = (form.get("code") or [""])[0]
            if grant == "authorization_code" and code == CODE:
                sys.stderr.write("fake-osc: token: the code came back and was exchanged\n")
                return self.reply(200, {"access_token": ACCESS, "refresh_token": REFRESH,
                                        "token_type": "Bearer", "expires_in": 3600})
            return self.reply(400, {"error": "invalid_grant"})
        if url.path == "/servicetoken" and self.headers.get("x-pat-jwt") == "Bearer " + ACCESS:
            return self.reply(200, {"token": SERVICE_TOKEN, "expiry": int(time.time()) + 3600})
        return self.reply(404, {})


def main():
    port = int(sys.argv[1])
    ThreadingHTTPServer(("127.0.0.1", port), Fake).serve_forever()


if __name__ == "__main__":
    main()
