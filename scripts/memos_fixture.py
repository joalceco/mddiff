"""Small, audited Memos 0.31 HTTP fixture; contains only synthetic documents."""

from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import secrets
import threading
from types import SimpleNamespace
from urllib.parse import urlsplit


class Memos:
    def __init__(self):
        self.memos = {user: secrets.token_urlsafe(32) for user in ("alice", "bob")}
        self.secrets = set(self.memos.values())
        self.documents = {}
        self.audit = []
        self.revoked = set()
        self.mode = "normal"
        self.gate = None
        for user in self.memos:
            self.note(user, "shared", f"\ufeff# Nota privada de {user}\r\n\r\nUnicode: 🌳 日本語.\r\nNUL: \0 fin.\r\n", "2026-10-28T01:02:03Z")
            self.note(user, "plain", f"# Sin fecha de {user}\n")
            self.note(user, "empty", "")
            self.note(user, "public", "# Una nota pública no valida una credencial\n")

    def note(self, user, uid, content, updated=None):
        document = {"name": "memos/" + uid, "content": content,
                    "attachments": [{"externalLink": getattr(self, "base", "http://127.0.0.1:1") + "/never-fetch-attachment"}],
                    "relations": [{"relatedMemo": "memos/never-fetch-related"}]}
        if updated is not None:
            document["updateTime"] = updated
        self.documents[user, uid] = document

    @contextmanager
    def pause(self, uid):
        gate = SimpleNamespace(path="/api/v1/memos/" + uid, entered=threading.Event(), release=threading.Event())
        self.gate = gate
        try:
            yield gate
        finally:
            gate.release.set()
            self.gate = None

    def assert_read_only(self):
        allowed = {"/api/v1/auth/me", "/api/v1/memos/missing"} | {"/api/v1/memos/" + uid for _, uid in self.documents}
        assert all(row["method"] == "GET" and row["path"] in allowed
                   and not row["query"] for row in self.audit), "Memos connector fetched an unsupported path or wrote to its provider."


@contextmanager
def memos():
    state = Memos()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def reply(self, status, value, headers=None):
            payload = json.dumps(value, ensure_ascii=True).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            for name, value in (headers or {}).items():
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(payload)

        def do_GET(self):
            parsed = urlsplit(self.path)
            credential = self.headers.get("Authorization", "").removeprefix("Bearer ")
            user = next((name for name, value in state.memos.items() if value == credential), None)
            state.audit.append({"method": "GET", "path": parsed.path, "query": bool(parsed.query), "user": user})
            gate = state.gate
            if gate is not None and gate.path == parsed.path:
                gate.entered.set()
                gate.release.wait(10)
            if parsed.path == "/api/v1/auth/me":
                if user is None or user in state.revoked:
                    self.reply(401, {"message": "Synthetic credential rejected: " + credential})
                else:
                    self.reply(200, {"user": {"name": "users/" + user, "username": user}})
                return
            if not parsed.path.startswith("/api/v1/memos/"):
                self.reply(404, {"error": "unexpected attachment or resource read"})
                return
            uid = parsed.path.removeprefix("/api/v1/memos/")
            if uid == "public" and (user is None or user in state.revoked):
                self.reply(200, state.documents["alice", "public"])
                return
            if user is None or user in state.revoked:
                self.reply(401, {"message": credential})
                return
            document = state.documents.get((user, uid))
            if document is None:
                self.reply(404, {"message": "Synthetic missing memo: " + credential})
            elif state.mode == "redirect":
                self.reply(302, {}, {"Location": state.base + "/never-fetch-attachment"})
            elif state.mode == "oversized-response":
                self.reply(200, {"content": "x" * (12 * 1024 * 1024 + 64 * 1024 + 1)})
            elif state.mode == "wrong-name":
                self.reply(200, {**document, "name": "memos/another-owner"})
            elif state.mode == "missing-content":
                self.reply(200, {"name": document["name"], "secret": credential})
            elif state.mode == "non-string-content":
                self.reply(200, {**document, "content": 123})
            elif state.mode == "long-update-time":
                self.reply(200, {**document, "updateTime": "x" * 65})
            elif state.mode == "control-update-time":
                self.reply(200, {**document, "updateTime": "2026-10-28\n"})
            elif state.mode == "non-string-update-time":
                self.reply(200, {**document, "updateTime": 123})
            elif state.mode == "envelope":
                self.reply(200, {"memo": document})
            elif state.mode == "provider-error":
                self.reply(403, {"message": "Synthetic provider secret: " + credential})
            else:
                self.reply(200, document)

        def rejected_write(self):
            parsed = urlsplit(self.path)
            state.audit.append({"method": self.command, "path": parsed.path, "query": bool(parsed.query), "user": None})
            self.reply(405, {"error": "business writes forbidden"})

        do_POST = do_PUT = do_PATCH = do_DELETE = rejected_write

    class Server(ThreadingHTTPServer):
        daemon_threads = True

        def handle_error(self, *args):
            pass

    server = Server(("127.0.0.1", 0), Handler)
    state.base = f"http://127.0.0.1:{server.server_port}"
    for document in state.documents.values():
        document["attachments"][0]["externalLink"] = state.base + "/never-fetch-attachment"
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield state
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
