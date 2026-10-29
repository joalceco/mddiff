"""Audited Jira issue descriptions layered over the existing isolated OAuth fixture."""

from contextlib import contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading
from urllib.parse import parse_qs, urlsplit

from connections_fixture import providers


def document(text):
    return {"type": "doc", "version": 1, "content": [{"type": "paragraph", "content": [{"type": "text", "text": text}]}]}


@contextmanager
def jira_sources():
    with providers() as state:
        oauth = urlsplit(state.base)
        state.issues = {(user, "PROJ-1"): {"key": "PROJ-1", "fields": {"summary": "Issue privado de " + user,
            "description": document("Contenido privado de " + user + "."), "updated": "2026-10-29T00:01:02.000+0000"}}
            for user in ("alice", "bob")}
        state.issue_errors = {}
        state.issue_redirect = False

        def read_only():
            allowed = {("GET", "/authorize"), ("GET", "/oauth/token/accessible-resources"), ("POST", "/oauth/token")}
            for row in state.audit:
                issue = row["path"].startswith("/ex/jira/synthetic-alpha/rest/api/3/issue/")
                assert (row["method"], row["path"]) in allowed or (row["method"] == "GET" and issue), "Unexpected Jira business request"
                if issue:
                    assert row["fields"] == ["description", "summary", "updated"], "Jira requested comments, attachments or another field"
            assert all(row["grant"] in (None, "authorization_code", "refresh_token") for row in state.audit)

        state.assert_read_only = read_only

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, status, value, headers=None):
                payload = json.dumps(value).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                for key, value in (headers or {}).items():
                    self.send_header(key, value)
                self.end_headers()
                self.wfile.write(payload)

            def forward(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", "0"))) if self.command == "POST" else None
                headers = {k: self.headers[k] for k in ("Authorization", "Content-Type") if k in self.headers}
                connection = HTTPConnection(oauth.hostname, oauth.port, timeout=12)
                try:
                    connection.request(self.command, self.path, body, headers)
                    response = connection.getresponse()
                    raw = response.read(4 * 1024 * 1024)
                    self.send_response(response.status)
                    for name, value in response.getheaders():
                        if name.lower() not in {"connection", "server", "date", "content-length"}:
                            self.send_header(name, value)
                    self.send_header("Content-Length", str(len(raw)))
                    self.end_headers()
                    self.wfile.write(raw)
                finally:
                    connection.close()

            def do_GET(self):
                parsed = urlsplit(self.path)
                prefix = "/ex/jira/synthetic-alpha/rest/api/3/issue/"
                if not parsed.path.startswith(prefix):
                    self.forward()
                    return
                state.wait(parsed.path)
                access = self.headers.get("Authorization", "").removeprefix("Bearer ")
                user = state.access.get(access)
                query = parse_qs(parsed.query)
                with state.lock:
                    state.record("GET", parsed.path, user, fields=query.get("fields", [""])[0].split(","))
                    if user is None or user in state.revoked:
                        self.reply(401, {"error": "synthetic denied credential: " + access})
                        return
                    if query != {"fields": ["summary,description,updated"]}:
                        self.reply(400, {"error": "overbroad issue fields"})
                        return
                    if state.issue_redirect:
                        self.reply(302, {}, {"Location": state.base + "/never-follow-credential"})
                        return
                    key = parsed.path.removeprefix(prefix)
                    if key in state.issue_errors:
                        self.reply(state.issue_errors[key], {"error": "private Jira detail " + access}, {"Retry-After": "1"})
                        return
                    item = state.issues.get((user, key))
                    self.reply(200 if item is not None else 404, item or {"error": "unknown issue"})

            def do_POST(self):
                self.forward()

            def rejected_write(self):
                state.record(self.command, urlsplit(self.path).path)
                self.reply(405, {"error": "business writes forbidden"})

            do_PUT = do_PATCH = do_DELETE = rejected_write

        class Server(ThreadingHTTPServer):
            daemon_threads = True

            def handle_error(self, *args):
                pass

        server = Server(("127.0.0.1", 0), Handler)
        state.base = f"http://127.0.0.1:{server.server_port}"
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield state
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)
