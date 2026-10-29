#!/usr/bin/env python3
"""Private stdin-controlled client for the isolated container integration test."""

from contextlib import ExitStack, closing, contextmanager
import json
import os
from pathlib import Path
import select
import signal
import socket
import socketserver
import sqlite3
import ssl
import stat
import sys
import threading
import time
from types import SimpleNamespace
from unittest.mock import patch
from urllib.error import URLError
from urllib.request import ProxyHandler, build_opener

from hosted_mcp_fixture import TlsBrowser, audience_clients, http, tls_proxy, token
from memos_fixture import memos
from test_hosted_auth import CheckFailed, check, fixture, web_client
from test_hosted_mcp import current, rpc, tool
from test_hosted_persistence import Events
from test_memos_source import connect, request, work


DATA = Path("/var/lib/mddiff")
SECRETS = Path("/run/secrets/mddiff")
BASE_PORT = 3847


def emit(event, **fields):
    print(json.dumps({"event": event, **fields}), flush=True)


@contextmanager
def issuer_forwarder():
    class Handler(socketserver.BaseRequestHandler):
        def handle(self):
            with socket.create_connection(("127.0.0.1", 8080), timeout=5) as upstream:
                peers = [self.request, upstream]
                while True:
                    ready, _, _ = select.select(peers, [], [], 30)
                    if not ready:
                        return
                    for source in ready:
                        data = source.recv(65536)
                        if not data:
                            return
                        (upstream if source is self.request else self.request).sendall(data)

    class Server(socketserver.ThreadingTCPServer):
        allow_reuse_address = True
        daemon_threads = True

        def handle_error(self, *args):
            pass

    server = Server(("127.0.0.1", 8848), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield
    finally:
        server.shutdown(); server.server_close(); thread.join(timeout=5)


def initialize(tls):
    for path in (DATA, SECRETS, Path("/negative-db"), Path("/negative-secrets"), Path("/negative-config-data")):
        path.mkdir(parents=True, exist_ok=True)
        path.chmod(0o700); os.chown(path, 10001, 10001)
    for path, owner, mode in [(Path("/negative-owner"), 0, 0o700), (Path("/negative-mode"), 10001, 0o755)]:
        path.mkdir(exist_ok=True); path.chmod(mode); os.chown(path, owner, owner)
    for name, value in tls.items():
        check(name in {"ca.pem", "server.pem", "server.key"}, "Unexpected TLS fixture filename.")
        path = SECRETS / name
        path.write_text(value); path.chmod(0o600); os.chown(path, 10001, 10001)
    os.setgroups([]); os.setgid(10001); os.setuid(10001)
    check(os.geteuid() == 10001 and os.getegid() == 10001, "The test client did not drop its bootstrap identity.")


def configure(runtime, provider, base):
    fixture.private_write(SECRETS / "oidc.json", json.dumps({"issuer": runtime["issuer"], "public_origin": base,
        "client_id": "mddiff-hosted-web", "introspection_client_id": "mddiff-resource",
        "introspection_client_secret": runtime["resource_client"]["secret"], "audience": "mddiff-resource", "allow_insecure_loopback": True}))
    key = SECRETS / "connections.key"
    key.write_bytes(os.urandom(32)); key.chmod(0o600)
    fixture.private_write(SECRETS / "connections.json", json.dumps({"key_file": str(key), "allow_insecure_loopback": True,
        "memos": [{"id": "notes", "label": "Memos sintético", "url": provider.base, "addresses": ["127.0.0.1"]}]}))
    for path in SECRETS.iterdir():
        target = Path("/negative-secrets") / path.name
        target.write_bytes(path.read_bytes()); target.chmod(0o600)
    (Path("/negative-secrets") / "connections.json").chmod(0o644)


class Scenario:
    def __init__(self, runtime, provider, base):
        self.runtime, self.provider, self.base = runtime, provider, base
        self.env = SimpleNamespace(base=base, trust=ssl.create_default_context(cafile=str(SECRETS / "ca.pem")), runtime=runtime)
        self.expected = {}
        self.streams = []
        self.browsers = {}
        self.ciphertexts = None
        self.private = {runtime["admin_password"], runtime["resource_client"]["secret"], *provider.secrets,
                        *(user["password"] for user in runtime["users"].values())}

    def safe(self, raw):
        check(all(value.encode() not in raw for value in self.private), "Container output exposed a private credential.")

    def ready(self):
        deadline = time.monotonic() + 30
        while True:
            try:
                status, raw, _ = http(self.env, "/healthz")
                if status == 204:
                    check(not raw, "Container health disclosed content.")
                    break
                check(time.monotonic() < deadline, "The container health endpoint did not reach HTTP 204.")
                time.sleep(0.2)
            except (OSError, URLError):
                check(time.monotonic() < deadline, "The container did not become healthy.")
                time.sleep(0.2)
        with build_opener(ProxyHandler({})).open("http://127.0.0.1:3847/healthz", timeout=5) as response:
            check(response.status == 204 and not response.read(), "Direct loopback health requires private authentication.")

    def boundaries(self):
        try:
            build_opener(ProxyHandler({})).open(self.base + "/healthz", timeout=5)
            check(False, "The resource accepted an untrusted temporary CA.")
        except URLError as error:
            check(isinstance(error.reason, ssl.SSLCertVerificationError), "The negative TLS check failed for an unrelated reason.")
        fake = {"Forwarded": 'for=127.0.0.1;proto=https;host=approved.example', "X-Forwarded-User": "alice",
                "X-Forwarded-For": "127.0.0.1", "X-Forwarded-Proto": "https", "X-User-Id": "alice"}
        for path in ("/api/state", "/api/work", "/api/events"):
            check(http(self.env, path, headers=fake)[0] == 401, "Forwarded headers bypassed private account authentication.")
        check(http(self.env, "/mcp", {}, headers=fake)[0] == 401, "Forwarded headers bypassed MCP bearer authentication.")
        check(http(self.env, "/api/work", headers={"Origin": "https://unapproved.example"})[0] == 403, "A foreign browser origin entered the container API.")
        status, raw, _ = http(self.env, "/.well-known/oauth-protected-resource")
        check(status == 200 and json.loads(raw)["resource"] == self.base + "/mcp", "Container discovery omitted its HTTPS resource.")

    def sessions(self, first=False, update=False):
        self.ready()
        if first:
            self.boundaries()
        active = []
        for name in ("alice", "bob"):
            browser = TlsBrowser(self.base, self.runtime["issuer"], self.env.trust)
            browser.login(self.runtime["users"][name]); browser.session()
            self.browsers[name] = browser
            if first:
                connect(browser, self.provider, name, 0)
                status, _ = request(browser, self.provider, "/api/sources/memos", "POST", {
                    "destination": "notes", "reference": "shared", "expected_revision": 0})
                check(status == 200, "Container could not capture its owner's original from Memos.")
                self.expected[name] = work(browser, self.provider)
                check(self.expected[name]["base"]["markdown"] == self.provider.documents[name, "shared"]["content"], "Container capture changed private Markdown bytes.")
            else:
                check(work(browser, self.provider) == self.expected[name], "Container restart lost work, provenance or owner isolation.")
                status, _ = request(browser, self.provider, "/api/connections/notes/check", "POST", {"expected_revision": 1})
                check(status == 200 and self.provider.audit[-1]["user"] == name, "Restart could not decrypt the correct owner's Memos credential.")
            active.append(Events(browser, self.expected[name]["revision"]))
        self.streams = active
        for index, name in enumerate(("alice", "bob")):
            access = token(self.env, name); self.private.add(access)
            rpc(self.env, access, "initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "container-test", "version": "1"}})
            actual = current(self.env, access)
            check(all(actual[key] == self.expected[name][key] for key in ("revision", "base", "candidate"))
                  and actual["viewer_url"] == self.base + "/", "Container MCP read another account or returned an insecure viewer.")
            if first or update:
                before = self.expected[name]
                proposal = {"label": "Propuesta de IA.md", "markdown": "# Propuesta privada de " + name + "\r\n\r\nRevisión " + str(before["revision"]) + ". 🌲\r\n"}
                result = tool(self.env, access, "submit_proposal", {"expected_revision": before["revision"], "candidate": proposal})
                check(not result.get("isError") and result["structuredContent"]["viewer_url"] == self.base + "/", "Container MCP could not commit an HTTPS-visible proposal.")
                active[index].revision(before["revision"] + 1); active[1 - index].unchanged()
                after = work(self.browsers[name], self.provider)
                check(after["base"] == before["base"] and after.get("base_source") == before.get("base_source")
                      and after["candidate"] == proposal and after["revision"] == before["revision"] + 1, "Container proposal changed its captured original or provenance.")
                self.expected[name] = after
        self.provider.assert_read_only()

    def stopped(self):
        for stream in self.streams:
            check(stream.closed.wait(8), "SIGTERM left an authenticated SSE connection open.")
            stream.thread.join(timeout=1)
        check(stat.S_IMODE(DATA.stat().st_mode) == 0o700 and DATA.stat().st_uid == 10001, "Data volume has incorrect ownership or permissions.")
        for path in SECRETS.iterdir():
            check(path.is_file() and stat.S_IMODE(path.stat().st_mode) == 0o600 and path.stat().st_uid == 10001, "External secrets are not private to the runtime identity.")
        for path in DATA.iterdir():
            check(path.is_file() and stat.S_IMODE(path.stat().st_mode) == 0o600 and path.stat().st_uid == 10001, "SQLite created a public or incorrectly owned file.")
            raw = path.read_bytes(); self.safe(raw)
            check((SECRETS / "connections.key").read_bytes() not in raw, "SQLite stored its external encryption key.")
        with closing(sqlite3.connect(DATA / "work.sqlite3")) as database:
            rows = database.execute("SELECT subject, revision, base_label, base_markdown, candidate_label, candidate_markdown, base_source FROM current_work").fetchall()
            check(len(rows) == 2, "Container storage lost private owners or created document history.")
            owners = {user["id"]: name for name, user in self.runtime["users"].items()}
            for subject, revision, label, original, proposed_label, proposal, source in rows:
                expected = self.expected[owners[subject]]
                check(revision == expected["revision"] and {"label": label, "markdown": original} == expected["base"]
                      and {"label": proposed_label, "markdown": proposal} == expected["candidate"]
                      and json.loads(source) == expected["base_source"], "Container persistence changed raw documents or provenance.")
            encrypted = database.execute("SELECT subject, provider, destination, generation, nonce, ciphertext FROM encrypted_connections").fetchall()
            check(len(encrypted) == 2 and all(kind == "memos" and destination == "notes" and revision == 1 and len(nonce) == 12 and len(ciphertext) >= 16
                                            for _, kind, destination, revision, nonce, ciphertext in encrypted), "Container credentials are not private authenticated ciphertexts.")
            current_ciphertexts = {subject: (nonce, ciphertext) for subject, _, _, _, nonce, ciphertext in encrypted}
            check(self.ciphertexts is None or self.ciphertexts == current_ciphertexts, "A restart or read unexpectedly replaced encrypted credentials.")
            self.ciphertexts = current_ciphertexts
            with closing(sqlite3.connect("/negative-db/work.sqlite3")) as negative:
                database.backup(negative)
        Path("/negative-db/work.sqlite3").chmod(0o644)


def main():
    phase = "bootstrap"
    try:
        payload = json.loads(sys.stdin.readline(1024 * 1024))
        runtime = payload["runtime"]
        check(runtime["issuer"] == "http://localhost:8848/realms/mddiff-spike", "Only the isolated synthetic issuer is permitted.")
        initialize(payload["tls"])
        with ExitStack() as resources:
            resources.enter_context(issuer_forwarder())
            users = [name for name in runtime["users"] if name != "mallory"] if payload.get("capacity") else ("alice", "bob")
            provider = resources.enter_context(memos(users))
            with patch("hosted_mcp_fixture.certificates", return_value=(SECRETS / "ca.pem", SECRETS / "server.pem", SECRETS / "server.key")):
                base, _ = resources.enter_context(tls_proxy(SECRETS, BASE_PORT))
            configure(runtime, provider, base)
            resources.enter_context(audience_clients(runtime, base + "/mcp"))
            resources.enter_context(web_client(runtime, base))
            if payload.get("capacity"):
                from capacity_scenario import Capacity, long_lived_clients
                resources.enter_context(long_lived_clients(runtime))
                scenario = Capacity(runtime, provider, base)
            else:
                scenario = Scenario(runtime, provider, base)
            emit("ready", uid=os.geteuid(), gid=os.getegid(), origin=base)
            while line := sys.stdin.readline(2 * 1024 * 1024):
                command = json.loads(line)
                phase = command["phase"]
                if phase == "quit":
                    break
                if phase == "first":
                    scenario.sessions(first=True)
                    if payload.get("capacity"):
                        emit("passed", phase=phase, metrics=scenario.bootstrap_metrics)
                        continue
                elif phase == "load":
                    emit("passed", phase=phase, metrics=scenario.load(command["profile"]))
                    continue
                elif phase == "limits":
                    scenario.limits()
                elif phase == "restart":
                    scenario.sessions(update=True)
                elif phase == "recreated":
                    scenario.sessions()
                elif phase == "stopped":
                    scenario.stopped()
                elif phase == "logs":
                    scenario.safe(command["content"].encode())
                elif phase == "negative-permissions":
                    owner, mode = Path("/negative-owner").stat(), Path("/negative-mode").stat()
                    check(owner.st_uid == 0 and stat.S_IMODE(owner.st_mode) == 0o700, "Docker replaced the negative volume's owner fixture.")
                    check(mode.st_uid == 10001 and stat.S_IMODE(mode.st_mode) == 0o755, "Docker replaced the negative directory mode fixture.")
                    check(stat.S_IMODE(Path("/negative-db/work.sqlite3").stat().st_mode) == 0o644
                          and stat.S_IMODE(Path("/negative-secrets/connections.json").stat().st_mode) == 0o644,
                          "The negative database or configuration fixture is not public as intended.")
                else:
                    raise CheckFailed("Unknown container-test phase.")
                emit("passed", phase=phase)
        emit("closed")
        return 0
    except CheckFailed as error:
        emit("error", phase=phase, message=str(error))
    except BaseException as error:
        stage = getattr(locals().get("scenario"), "stage", phase)
        emit("error", phase=phase, message="Check failed during " + stage + " (" + type(error).__name__ + "); private diagnostics were suppressed.")
    return 1


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(1))
    raise SystemExit(main())
