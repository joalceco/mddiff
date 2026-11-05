#!/usr/bin/env python3
"""Exercise real Codex OAuth and MCP calls with synthetic Keycloak users, without a model turn."""

import argparse
from contextlib import ExitStack
from html.parser import HTMLParser
from http.cookiejar import CookieJar, DefaultCookiePolicy
import json
import os
from pathlib import Path
import queue
import stat
import subprocess
import tempfile
import threading
import time
from urllib.parse import parse_qs, urlencode, urljoin, urlsplit
from urllib.request import HTTPCookieProcessor, HTTPRedirectHandler, Request, build_opener


SERVER = "identity_spike"
SCOPES = ["openid", "profile", "mddiff:compare"]
CALLBACK = "http://127.0.0.1:43210/callback"


class ProbeError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise ProbeError(message)


def load_runtime(path):
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    with os.fdopen(descriptor, "r", encoding="utf-8") as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode), "Runtime must be a regular file.")
        require(info.st_uid == os.getuid(), "Runtime must belong to the current user.")
        require(info.st_mode & 0o077 == 0, "Runtime must not grant group or other access; use mode 0600.")
        require(info.st_size <= 1024 * 1024, "Runtime file is unexpectedly large.")
        return json.load(stream)


def origin(url):
    parsed = urlsplit(url)
    return parsed.scheme, parsed.hostname, parsed.port


class RestrictedRedirects(HTTPRedirectHandler):
    def __init__(self, issuer):
        self.allowed = {origin(issuer), origin(CALLBACK)}

    def redirect_request(self, request, response, code, message, headers, new_url):
        require(origin(new_url) in self.allowed, "OAuth redirected outside the synthetic issuer and callback.")
        return super().redirect_request(request, response, code, message, headers, new_url)


class Forms(HTMLParser):
    def __init__(self, html):
        super().__init__(convert_charrefs=True)
        self.forms = []
        self.current = None
        self.feed(html)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "form":
            self.current = {"action": attrs.get("action", ""), "fields": {}, "password": False}
        elif tag == "input" and self.current is not None:
            self.current["password"] |= attrs.get("type") == "password"
            if attrs.get("type") == "hidden" and attrs.get("name"):
                self.current["fields"][attrs["name"]] = attrs.get("value", "")

    def handle_endtag(self, tag):
        if tag == "form" and self.current is not None:
            self.forms.append(self.current)
            self.current = None


def synthetic_cookie_jar():
    class LocalhostPolicy(DefaultCookiePolicy):
        def return_ok_secure(self, cookie, request):
            # Browsers allow Secure cookies on localhost HTTP; urllib does not.
            if (cookie.secure and origin(request.get_full_url()) == ("http", "localhost", 8848)
                and cookie.domain == "localhost.local"
                and cookie.name in {"AUTH_SESSION_ID", "KC_AUTH_SESSION_HASH", "KC_RESTART", "KEYCLOAK_IDENTITY", "KEYCLOAK_SESSION"}):
                return True
            return super().return_ok_secure(cookie, request)
    return CookieJar(LocalhostPolicy())


def complete_synthetic_login(url, issuer, mcp_url, username, password):
    query = parse_qs(urlsplit(url).query)
    require(origin(url) == origin(issuer), "Authorization URL uses an unexpected issuer.")
    require(query.get("client_id") == ["mddiff-codex"], "Codex did not use the pre-registered client.")
    require(query.get("redirect_uri") == [CALLBACK], "Codex did not use the fixed callback.")
    require(query.get("code_challenge_method") == ["S256"], "Codex did not request PKCE S256.")
    require(bool(query.get("code_challenge")), "Codex did not send a PKCE challenge.")
    resources = query.get("resource", [])
    require(resources and set(resources) == {mcp_url}, "Codex did not send only the expected resource.")
    require(set(SCOPES) <= set(query.get("scope", [""])[0].split()), "Codex omitted a required scope.")
    opener = build_opener(HTTPCookieProcessor(synthetic_cookie_jar()), RestrictedRedirects(issuer))
    with opener.open(url, timeout=20) as response:
        page_url = response.url
        html = response.read(1024 * 1024).decode("utf-8")
    forms = [form for form in Forms(html).forms if form["password"]]
    require(len(forms) == 1, "Expected one synthetic Keycloak username/password form.")
    form = forms[0]
    action = urljoin(page_url, form["action"])
    require(origin(action) == origin(issuer), "Refusing to submit credentials outside Keycloak.")
    require(urlsplit(action).path.startswith(urlsplit(issuer).path + "/"), "Unexpected Keycloak form path.")
    fields = {**form["fields"], "username": username, "password": password}
    request = Request(action, data=urlencode(fields).encode(), headers={"Content-Type": "application/x-www-form-urlencoded"})
    with opener.open(request, timeout=20) as response:
        # Consume the local callback response without logging codes, cookies, or token data.
        response.read(1024 * 1024)
        require(origin(response.url) == origin(CALLBACK), "Synthetic login did not reach the Codex callback.")


class AppServer:
    def __init__(self, home, mcp_url):
        # Discovery supplies resource; Codex 0.163 duplicates it if oauth_resource is also configured.
        config = f'''mcp_oauth_credentials_store = "file"
[analytics]
enabled = false
[mcp_servers.{SERVER}]
url = {json.dumps(mcp_url)}
scopes = {json.dumps(SCOPES)}
required = true
[mcp_servers.{SERVER}.oauth]
client_id = "mddiff-codex"
callback_port = 43210
callback_url = {json.dumps(CALLBACK)}
'''
        config_path = home / "config.toml"
        config_path.write_text(config, encoding="utf-8")
        config_path.chmod(0o600)
        environment = {**os.environ, "CODEX_HOME": str(home)}
        for name in ("OPENAI_API_KEY", "CODEX_API_KEY"):
            environment.pop(name, None)
        self.process = subprocess.Popen(
            ["codex", "app-server", "--stdio", "--strict-config"], cwd=home, env=environment,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, encoding="utf-8", bufsize=1,
        )
        self.queue = queue.Queue()
        self.pending = []
        self.request_id = 0
        threading.Thread(target=self._read, daemon=True).start()
        try:
            self.request("initialize", {"clientInfo": {"name": "mddiff_identity_spike", "version": "0.1.0"}, "capabilities": {"experimentalApi": True}})
            self.send({"method": "initialized", "params": {}})
        except Exception:
            self.close()
            raise

    def _read(self):
        for line in self.process.stdout:
            try:
                self.queue.put(json.loads(line))
            except json.JSONDecodeError:
                self.queue.put({"invalid_json": True})
        self.queue.put(None)

    def send(self, message):
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def receive(self, predicate, timeout=40):
        for index, message in enumerate(self.pending):
            if predicate(message):
                return self.pending.pop(index)
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                message = self.queue.get(timeout=max(0.01, deadline - time.monotonic()))
            except queue.Empty:
                break
            require(message is not None, "Codex app-server exited before responding.")
            require("invalid_json" not in message, "Codex emitted invalid JSON on its protocol stream.")
            if predicate(message):
                return message
            require(not ("method" in message and "id" in message), "Unexpected server request; this probe does not approve interactive actions.")
            self.pending.append(message)
        raise ProbeError("Timed out waiting for Codex app-server.")

    def request(self, method, params):
        self.request_id += 1
        request_id = self.request_id
        self.send({"id": request_id, "method": method, "params": params})
        response = self.receive(lambda message: message.get("id") == request_id)
        require("error" not in response, f"Codex rejected {method}; RPC error code {response.get('error', {}).get('code')}.")
        return response["result"]

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                try:
                    self.process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=5)


def open_comparison(client, thread_id, label, expected_revision=None):
    arguments = {"original": f"# Documento sintético\n\nOriginal de {label}.\n", "proposal": f"# Documento sintético\n\nPropuesta de {label}.\n"}
    if expected_revision is not None:
        arguments["expected_revision"] = expected_revision
    return client.request("mcpServer/tool/call", {"server": SERVER, "threadId": thread_id, "tool": "open_comparison", "arguments": arguments})


def probe(runtime, users, issuer, mcp_url):
    with ExitStack() as resources:
        sessions = []
        for name in users:
            credentials = runtime.get("users", {}).get(name)
            require(isinstance(credentials, dict), "Requested synthetic user is missing from runtime.")
            require(all(isinstance(credentials.get(key), str) and credentials[key] for key in ("username", "password")), "Synthetic username/password is missing.")
            home = Path(resources.enter_context(tempfile.TemporaryDirectory(prefix="mddiff-codex-probe-")))
            client = AppServer(home, mcp_url)
            resources.callback(client.close)
            login = client.request("mcpServer/oauth/login", {"name": SERVER, "scopes": SCOPES, "timeoutSecs": 60})
            complete_synthetic_login(login["authorizationUrl"], issuer, mcp_url, credentials["username"], credentials["password"])
            done = client.receive(lambda message: message.get("method") == "mcpServer/oauthLogin/completed" and message.get("params", {}).get("name") == SERVER)
            require(done["params"]["success"], "Codex OAuth login did not complete successfully.")
            thread = client.request("thread/start", {"cwd": str(home), "ephemeral": True, "approvalPolicy": "never", "sandbox": "read-only"})
            thread_id = thread["thread"]["id"]
            inventory = client.request("mcpServerStatus/list", {"threadId": thread_id, "serverName": SERVER, "detail": "toolsAndAuthOnly"})
            servers = [server for server in inventory["data"] if server["name"] == SERVER]
            require(len(servers) == 1 and servers[0]["authStatus"] == "oAuth", "Codex did not discover the server with OAuth credentials.")
            require("open_comparison" in servers[0]["tools"], "Codex did not discover open_comparison.")
            result = open_comparison(client, thread_id, name)
            require(not result.get("isError"), "MCP comparison failed.")
            state = result.get("structuredContent", {})
            require(state.get("issuer") == issuer, "MCP returned a different issuer.")
            require(bool(state.get("subject")) and isinstance(state.get("revision"), int), "MCP omitted its owner or revision.")
            if credentials.get("id"):
                require(state["subject"] == credentials["id"], "MCP owner differs from the authenticated synthetic user.")
            require(isinstance(state.get("viewer_url"), str), "MCP omitted viewer_url.")
            sessions.append((name, client, thread_id, state))
        require(len({state["subject"] for _, _, _, state in sessions}) == len(sessions), "Two synthetic users share an MCP owner.")
        # Revisit each live client after other users wrote; global revision/state cannot pass this check.
        for name, client, thread_id, before in sessions:
            result = open_comparison(client, thread_id, name, before["revision"])
            require(not result.get("isError"), "Another user's comparison changed this user's revision.")
            after = result.get("structuredContent", {})
            require(after.get("subject") == before["subject"] and after.get("revision") == before["revision"] + 1, "Owner/revision changed unexpectedly.")
            stale = open_comparison(client, thread_id, name, before["revision"])
            require(stale.get("isError") is True, "A stale comparison overwrote the current work.")
            print(json.dumps({"user": name, "issuer": issuer, "subject": after["subject"], "revision": after["revision"], "oauth": "passed", "real_codex_mcp_call": "passed", "stale_write": "rejected", "model_turns": 0}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", type=Path, default=Path("target/identity-spike/runtime.json"))
    parser.add_argument("--user", action="append", help="Synthetic runtime user; repeat for an ownership test (default: alice and bob).")
    parser.add_argument("--issuer")
    parser.add_argument("--mcp-url")
    arguments = parser.parse_args()
    try:
        runtime = load_runtime(arguments.runtime)
        issuer = arguments.issuer or runtime.get("issuer", "http://localhost:8848/realms/mddiff-spike")
        mcp_url = arguments.mcp_url or runtime.get("mcp_url", "http://127.0.0.1:3850/mcp")
        users = arguments.user or ["alice", "bob"]
        require(len(set(users)) == len(users), "Each synthetic user may be selected once.")
        require(origin(issuer) == ("http", "localhost", 8848), "This synthetic-login probe only permits the local Keycloak spike.")
        require(origin(mcp_url) == ("http", "127.0.0.1", 3850), "This probe only permits the local MCP spike.")
        probe(runtime, users, issuer, mcp_url)
    except ProbeError as error:
        raise SystemExit(f"Codex identity probe failed: {error}") from None
    except (OSError, ValueError, KeyError, TypeError, queue.Empty):
        # OAuth exceptions can embed callback URLs or token responses. Print no raw exception data.
        raise SystemExit("Codex identity probe failed; no credentials or OAuth response data were printed.") from None


if __name__ == "__main__":
    main()
