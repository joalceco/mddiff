"""Audited, isolated HTTP fixtures for Memos 0.31 and Jira Cloud OAuth 3LO."""

from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import secrets
import threading
from types import SimpleNamespace
from urllib.parse import parse_qs, urlencode, urlsplit


class Providers:
    def __init__(self):
        self.client_id = "synthetic-jira-client"
        self.client_secret = secrets.token_urlsafe(32)
        self.memos = {name: secrets.token_urlsafe(32) for name in ("alice", "bob")}
        self.secrets = {self.client_secret, *self.memos.values()}
        self.sites = [{"id": "jira-alpha", "label": "Jira sintético", "url": "https://team-alpha.atlassian.net", "cloud_id": "synthetic-alpha"},
                      {"id": "jira-beta", "label": "Jira sin consentimiento", "url": "https://team-beta.atlassian.net", "cloud_id": "synthetic-beta"}]
        self.codes = {}
        self.access = {}
        self.refresh = {}
        self.used_refresh = set()
        self.audit = []
        self.revoked = set()
        self.resources_override = None
        self.token_lifespan = 3600
        self.redirect = False
        self.oversized = False
        self.lock = threading.RLock()
        self.gate = None

    @contextmanager
    def pause(self, path):
        gate = SimpleNamespace(path=path, entered=threading.Event(), release=threading.Event())
        self.gate = gate
        try:
            yield gate
        finally:
            gate.release.set()
            self.gate = None

    def wait(self, path):
        gate = self.gate
        if gate is not None and gate.path == path:
            gate.entered.set()
            gate.release.wait(10)

    def credentials(self, user):
        access, refresh = secrets.token_urlsafe(32), secrets.token_urlsafe(32)
        self.access[access] = user
        self.refresh[refresh] = user
        self.secrets.update((access, refresh))
        return {"access_token": access, "refresh_token": refresh, "expires_in": self.token_lifespan,
                "token_type": "Bearer", "scope": "read:jira-work offline_access"}

    def record(self, method, path, user=None, fields=(), grant=None):
        self.audit.append({"method": method, "path": path, "user": user,
                           "fields": sorted(fields), "grant": grant})

    def assert_read_only(self):
        allowed = {("GET", "/api/v1/auth/me"), ("GET", "/authorize"),
                   ("GET", "/oauth/token/accessible-resources"), ("POST", "/oauth/token")}
        assert all((row["method"], row["path"]) in allowed for row in self.audit), "Connector made an unsupported provider request."
        assert all(row["grant"] in (None, "authorization_code", "refresh_token") for row in self.audit), "Connector used an unexpected OAuth grant."


@contextmanager
def providers():
    state = Providers()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass  # Provider paths and request errors must never reveal OAuth codes or secrets.

        def reply(self, status, value, headers=None):
            payload = json.dumps(value).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            for name, value in (headers or {}).items():
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(payload)

        def do_GET(self):
            parsed = urlsplit(self.path)
            access = self.headers.get("Authorization", "").removeprefix("Bearer ")
            state.wait(parsed.path)
            with state.lock:
                if parsed.path == "/authorize":
                    query = parse_qs(parsed.query)
                    user = query.get("fixture_user", ["alice"])[0]
                    state.record("GET", parsed.path, user)
                    if (user not in state.memos or query.get("client_id") != [state.client_id]
                            or set(query.get("scope", [""])[0].split()) != {"read:jira-work", "offline_access"}
                            or query.get("audience") != ["api.atlassian.com"]
                            or query.get("response_type") != ["code"] or query.get("prompt") != ["consent"]):
                        self.reply(400, {"error": "synthetic_invalid_authorization"})
                        return
                    callback = query.get("redirect_uri", [""])[0]
                    if urlsplit(callback).hostname != "127.0.0.1" or urlsplit(callback).path != "/connections/jira/callback":
                        self.reply(400, {"error": "synthetic_invalid_redirect"})
                        return
                    code = secrets.token_urlsafe(32)
                    state.secrets.add(code)
                    state.codes[code] = (user, callback)
                    self.reply(302, {}, {"Location": callback + "?" + urlencode({"code": code, "state": query.get("state", [""])[0]})})
                    return
                user = next((name for name, value in state.memos.items() if value == access), None) if parsed.path == "/api/v1/auth/me" else state.access.get(access)
                state.record("GET", parsed.path, user)
                if user is None or user in state.revoked:
                    self.reply(401, {"error": "synthetic denied credential: " + access})
                elif state.redirect:
                    self.reply(302, {}, {"Location": state.base + "/never-follow-credential"})
                elif state.oversized:
                    self.reply(200, {"oversized": "x" * (3 * 1024 * 1024)})
                elif parsed.path == "/api/v1/auth/me":
                    self.reply(200, {"user": {"name": "users/" + user, "username": user, "role": "USER"}})
                elif parsed.path == "/oauth/token/accessible-resources":
                    self.reply(200, state.resources_override if state.resources_override is not None else [{
                        "id": state.sites[0]["cloud_id"], "name": "Synthetic Jira", "url": state.sites[0]["url"], "scopes": ["read:jira-work"],
                    }])
                else:
                    self.reply(404, {"error": "unsupported synthetic read"})

        def do_POST(self):
            try:
                length = int(self.headers.get("Content-Length", "0"))
                if not 0 <= length <= 16 * 1024:
                    self.reply(413, {})
                    return
                body = json.loads(self.rfile.read(length))
            except (ValueError, json.JSONDecodeError):
                self.reply(400, {})
                return
            path = urlsplit(self.path).path
            with state.lock:
                grant = body.get("grant_type")
                state.record("POST", path, fields=body.keys(), grant=grant)
                if path != "/oauth/token" or body.get("client_id") != state.client_id or body.get("client_secret") != state.client_secret:
                    self.reply(400, {"error": "invalid_client"})
                    return
                if grant == "authorization_code":
                    authorization = state.codes.pop(body.get("code"), None)
                    if authorization is None or body.get("redirect_uri") != authorization[1]:
                        self.reply(400, {"error": "invalid_grant"})
                        return
                    user = authorization[0]
                elif grant == "refresh_token":
                    refresh = body.get("refresh_token")
                    user = state.refresh.pop(refresh, None)
                    if user is None:
                        self.reply(400, {"error": "invalid_grant"})
                        return
                    state.used_refresh.add(refresh)
                else:
                    self.reply(400, {"error": "unsupported_grant_type"})
                    return
                state.audit[-1]["user"] = user
                if user in state.revoked:
                    self.reply(400, {"error": "invalid_grant"})
                else:
                    self.reply(200, state.credentials(user))

        def rejected_write(self):
            with state.lock:
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
