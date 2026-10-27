#!/usr/bin/env python3
"""Exercise the main hosted server against the isolated synthetic Keycloak fixture."""

from contextlib import contextmanager
import copy
import json
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import threading
import time
from urllib.parse import parse_qs, urlencode, urljoin, urlsplit
from urllib.request import HTTPCookieProcessor, HTTPRedirectHandler, Request, build_opener
from urllib.error import HTTPError


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "spikes/identity"))
from codex_probe import Forms, load_runtime, synthetic_cookie_jar
import keycloak as fixture

NAVIGATION = {"Sec-Fetch-Site": "cross-site", "Sec-Fetch-Mode": "navigate", "Sec-Fetch-Dest": "document"}


class CheckFailed(Exception):
    pass


def check(condition, message):
    if not condition:
        raise CheckFailed(message)


def origin(url):
    parsed = urlsplit(url)
    return parsed.scheme, parsed.hostname, parsed.port


class Redirects(HTTPRedirectHandler):
    def __init__(self, allowed=()):
        self.allowed = set(allowed)
        self.stop_callback = False

    def redirect_request(self, request, response, code, message, headers, new_url):
        if not self.allowed:
            return None
        check(origin(new_url) in self.allowed, "Unexpected redirect outside the local hosted fixture.")
        if self.stop_callback and urlsplit(new_url).path == "/auth/callback":
            return None
        redirected = super().redirect_request(request, response, code, message, headers, new_url)
        if redirected is not None and urlsplit(new_url).path == "/auth/callback":
            # Fetch Metadata remains cross-site throughout the callback's redirect chain.
            for name, value in NAVIGATION.items():
                redirected.add_header(name, value)
        return redirected


def http(base, path, body=None, headers=None, opener=None):
    request = Request(base + path, data=None if body is None else json.dumps(body).encode(), headers={
        **({"Content-Type": "application/json"} if body is not None else {}), **(headers or {}),
    })
    try:
        response = (opener or build_opener(Redirects())).open(request, timeout=10)
    except HTTPError as error:
        response = error
    with response:
        return response.status, response.read(1024 * 1024), response.headers


def json_body(raw):
    value = json.loads(raw)
    check(isinstance(value, dict), "Expected a JSON object from the hosted server.")
    return value


class Browser:
    def __init__(self, base, issuer):
        self.base = base
        self.issuer = issuer
        self.cookies = synthetic_cookie_jar()
        self.redirects = Redirects([origin(base), origin(issuer)])
        self.opener = build_opener(HTTPCookieProcessor(self.cookies), self.redirects)

    def request(self, path, body=None, headers=None):
        return http(self.base, path, body, headers, self.opener)

    def login(self, credentials, expected=200, capture_callback=False):
        self.redirects.stop_callback = capture_callback
        with self.opener.open(self.base + "/auth/login", timeout=10) as response:
            page_url = response.url
            forms = [form for form in Forms(response.read(1024 * 1024).decode()).forms if form["password"]]
        query = parse_qs(urlsplit(page_url).query)
        check(origin(page_url) == origin(self.issuer), "Login did not reach the synthetic issuer.")
        check(query.get("client_id") == ["mddiff-hosted-web"], "Unexpected web OAuth client.")
        check(query.get("redirect_uri") == [self.base + "/auth/callback"], "Unexpected web OAuth callback.")
        check(query.get("code_challenge_method") == ["S256"], "Web login omitted PKCE S256.")
        check(bool(query.get("state")) and bool(query.get("code_challenge")), "Web login omitted state or PKCE.")
        check(len(forms) == 1, "Expected one synthetic Keycloak credential form.")
        form = forms[0]
        action = urljoin(page_url, form["action"])
        check(origin(action) == origin(self.issuer), "Refusing to send credentials outside the fixture.")
        check(urlsplit(action).path.startswith(urlsplit(self.issuer).path + "/login-actions/"), "Unexpected Keycloak form path.")
        fields = {**form["fields"], "username": credentials["username"], "password": credentials["password"]}
        request = Request(action, data=urlencode(fields).encode(), headers={"Content-Type": "application/x-www-form-urlencoded"})
        try:
            response = self.opener.open(request, timeout=10)
        except HTTPError as error:
            response = error
        with response:
            if capture_callback:
                callback = response.headers.get("Location", "")
                check(response.status in (302, 303), "The synthetic issuer did not return an authorization callback.")
                check(origin(callback) == origin(self.base) and urlsplit(callback).path == "/auth/callback", "The synthetic issuer returned an unexpected callback.")
                self.redirects.stop_callback = False
                return callback
            check(response.status == expected, "Synthetic browser admission returned unexpected HTTP status.")
            check(origin(response.url) == origin(self.base), "Login did not return to the main hosted server.")
            if expected == 200:
                check(urlsplit(response.url).path == "/", "Successful callback did not return to the viewer.")
            response.read(1024 * 1024)

    def cookie(self):
        cookies = [cookie for cookie in self.cookies if cookie.name == "mddiff-session"]
        check(len(cookies) == 1, "Expected one main hosted session cookie.")
        cookie = cookies[0]
        check(not cookie.secure and cookie.path == "/", "Unexpected loopback session cookie transport or path.")
        attributes = {name.lower(): value for name, value in cookie._rest.items()}
        check("httponly" in attributes, "Session cookie is not HttpOnly.")
        check(str(attributes.get("samesite", "")).lower() == "lax", "Session cookie is not SameSite=Lax.")
        return cookie.value

    def session(self, expected=200):
        status, raw, headers = self.request("/auth/session")
        check(status == expected, "Unexpected hosted session status.")
        check("application/json" in headers.get("Content-Type", ""), "Session endpoint exposed non-JSON content.")
        data = json_body(raw)
        check(not ({"access_token", "refresh_token", "id_token", "client_secret", "subject", "issuer"} & data.keys()), "Session response exposed provider credentials or identifiers.")
        if expected == 200:
            check(set(data) == {"user_label", "csrf_token"}, "Unexpected public session fields.")
            check(isinstance(data["csrf_token"], str) and bool(data["csrf_token"]), "Session omitted CSRF protection.")
            check(self.cookie().encode() not in raw, "Session response exposed its opaque cookie.")
        return data


@contextmanager
def running(binary, base, config_path):
    port = urlsplit(base).port
    process = subprocess.Popen(
        [str(binary), "--mode", "hosted", "--port", str(port), "--oidc-config", str(config_path)],
        cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        check(bool(select.select([process.stdout], [], [], 10)[0]), "Hosted server did not announce its URL.")
        check(process.stdout.readline().strip() == "MDdiff: " + base, "Hosted server did not start at the configured loopback origin.")
        yield
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
            raise CheckFailed("Hosted server did not stop after SIGINT.") from None
        process.stdout.close()
        process.stderr.close()


@contextmanager
def web_client(runtime, base):
    endpoint = fixture.BASE + "/admin/realms/" + fixture.REALM + "/clients"
    token = fixture.admin_token(runtime)
    matches = fixture.request(endpoint + "?" + urlencode({"clientId": "mddiff-hosted-web"}), token=token)
    check(len(matches) <= 1, "Synthetic web client is ambiguous.")
    previous = fixture.request(endpoint + "/" + matches[0]["id"], token=token) if matches else None
    client = fixture.public_client("mddiff-hosted-web", [base + "/auth/callback"], resource_audience=False)
    client["protocolMappers"].append({
        "name": "synthetic-user-label", "protocol": "openid-connect", "protocolMapper": "oidc-usermodel-property-mapper",
        "config": {"user.attribute": "username", "claim.name": "preferred_username", "jsonType.label": "String",
                   "id.token.claim": "true", "access.token.claim": "true", "introspection.token.claim": "true"},
    })
    if previous:
        identifier = previous["id"]
        fixture.request(endpoint + "/" + identifier, client, token, "PUT")
    else:
        fixture.request(endpoint, client, token, "POST")
        matches = fixture.request(endpoint + "?" + urlencode({"clientId": client["clientId"]}), token=token)
        check(len(matches) == 1, "Could not locate the owned synthetic web client.")
        identifier = matches[0]["id"]
    client_url = endpoint + "/" + identifier
    try:
        yield client_url
    finally:
        token = fixture.admin_token(runtime)
        if previous:
            fixture.request(client_url, previous, token, "PUT")
        else:
            fixture.request(client_url, token=token, method="DELETE")


def test_anonymous(base):
    for path in ["/auth/session", "/api/state", "/api/events"]:
        for suffix, headers in [("", {}), ("", {"X-User-Id": "alice"}), ("", {"Authorization": "Bearer synthetic-invalid"}), ("?owner_id=alice", {})]:
            check(http(base, path + suffix, headers=headers)[0] == 401, "An unverified request crossed the hosted identity boundary.")
    status, _, headers = http(base, "/")
    check(status in (302, 303, 307, 308), "Anonymous viewer did not redirect to login.")
    check(urljoin(base, headers.get("Location", "")) == base + "/auth/login", "Viewer redirected to an unexpected login endpoint.")
    check(http(base, "/auth/callback?code=synthetic-unused&state=synthetic-forged")[0] == 400, "Forged callback state was accepted.")
    check(http(base, "/auth/callback?code=synthetic-unused&state=synthetic-forged", headers=NAVIGATION)[0] == 400, "Cross-site callback bypassed state verification or was blocked before it.")
    check(http(base, "/api/state", headers={"Sec-Fetch-Site": "cross-site"})[0] == 403, "Cross-site API request bypassed the HTTP guard.")
    check(http(base, "/api/events", headers=NAVIGATION)[0] == 403, "Navigation headers allowed cross-site API access.")


def test_browsers(runtime, base):
    alice, bob = Browser(base, runtime["issuer"]), Browser(base, runtime["issuer"])
    for name, browser in [("alice", alice), ("bob", bob)]:
        browser.login(runtime["users"][name])
        check(browser.session()["user_label"] == name, "Browser identity label differs from its verified user.")
        check(browser.request("/api/state")[0] == 200, "Admitted browser cannot read its workspace.")
    check(alice.cookie() != bob.cookie(), "Two browser identities received the same session identifier.")
    check(alice.request("/", headers=NAVIGATION)[0] == 200, "An admitted document navigation was blocked after OAuth.")
    check(alice.request("/api/state", headers={"Sec-Fetch-Site": "cross-site"})[0] == 403, "Authenticated cross-site API request was accepted.")
    status, raw, _ = alice.request("/auth/session?owner_id=bob", headers={"X-User-Id": "bob"})
    check(status == 200 and json_body(raw)["user_label"] == "alice", "A browser identity was replaced by untrusted identifiers.")
    mallory = Browser(base, runtime["issuer"])
    mallory.login(runtime["users"]["mallory"], expected=403)
    mallory.session(expected=401)
    for path, body in [("/api/open", {"path": "synthetic.md"}), ("/api/compare", {"base_path": "synthetic.md", "candidate_path": "other.md"}), ("/mcp", {})]:
        headers = {"Origin": base, "X-CSRF-Token": alice.session()["csrf_token"]}
        check(alice.request(path, body, headers)[0] == 404, "Hosted authentication enabled a local path or MCP endpoint.")
    return alice, bob


def test_callback_binding(runtime, base):
    source, target = Browser(base, runtime["issuer"]), Browser(base, runtime["issuer"])
    callback = source.login(runtime["users"]["alice"], capture_callback=True)
    with target.opener.open(base + "/auth/login", timeout=10) as response:
        response.read(1024 * 1024)
    path = callback[len(base):]
    check(target.request(path, headers=NAVIGATION)[0] == 400, "A real callback accepted another browser's flow cookie.")
    target.session(expected=401)
    status = source.request(path, headers=NAVIGATION)[0]
    check(status == 200, f"Legitimate callback did not survive rejected transplant and navigation: HTTP {status}.")
    check(source.session()["user_label"] == "alice", "The legitimate callback changed browser identity.")
    check(source.request(path, headers=NAVIGATION)[0] == 400, "A completed real callback could be replayed.")


def test_session_rotation(runtime, base):
    browser = Browser(base, runtime["issuer"])
    browser.login(runtime["users"]["alice"])
    before = browser.session()
    old_cookie = browser.cookie()
    # Forget only synthetic IdP SSO cookies, keeping the main app's old SID for rotation.
    for cookie in list(browser.cookies):
        if cookie.domain == "localhost.local":
            browser.cookies.clear(cookie.domain, cookie.path, cookie.name)
    browser.login(runtime["users"]["bob"])
    after = browser.session()
    check(after["user_label"] == "bob", "Signing in as another account retained the previous identity.")
    check(browser.cookie() != old_cookie and after["csrf_token"] != before["csrf_token"], "New login did not rotate its SID and CSRF token.")
    check(http(base, "/api/state", headers={"Cookie": "mddiff-session=" + old_cookie})[0] == 401, "The previous account's SID survived session rotation.")


@contextmanager
def event_stream(browser):
    stream = browser.opener.open(browser.base + "/api/events", timeout=10)
    check(stream.status == 200 and "text/event-stream" in stream.headers.get("Content-Type", ""), "Admitted browser cannot open its event stream.")
    check(stream.readline().startswith(b"event:"), "Event stream did not send its initial revision.")
    done, result = threading.Event(), {}

    def drain():
        try:
            while stream.read(8192):
                pass
            result["eof"] = True
        except (OSError, ValueError):
            result["eof"] = False
        finally:
            stream.close()
            done.set()

    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    try:
        yield done, result
    finally:
        if not done.wait(2):
            stream.close()
        reader.join(timeout=1)


def test_logout(alice, bob):
    session = alice.session()
    old_cookie = "mddiff-session=" + alice.cookie()
    for headers in [
        {}, {"Origin": alice.base}, {"X-CSRF-Token": session["csrf_token"]},
        {"Origin": "https://other.invalid", "X-CSRF-Token": session["csrf_token"]},
        {"Origin": alice.base, "X-CSRF-Token": "synthetic-invalid"},
    ]:
        check(alice.request("/auth/logout", {}, headers)[0] == 403, "Logout accepted missing or invalid Origin/CSRF protection.")
        check(alice.request("/api/state")[0] == 200, "Rejected logout destroyed a valid session.")
    with event_stream(alice) as (done, result):
        status, raw, _ = alice.request("/auth/logout", {}, {"Origin": alice.base, "X-CSRF-Token": session["csrf_token"]})
        check(status == 204 and not raw, "Protected logout did not return an empty 204.")
        check(done.wait(8) and result.get("eof"), "Logging out did not close an existing SSE within eight seconds.")
    check(not any(cookie.name == "mddiff-session" for cookie in alice.cookies), "Logout did not remove its browser cookie.")
    for path in ["/auth/session", "/api/state"]:
        check(http(alice.base, path, headers={"Cookie": old_cookie})[0] == 401, "Logged-out cookie can be replayed.")
    check(bob.session()["user_label"] == "bob", "Logging out one user affected another browser identity.")


def test_expiry(runtime, base, client_url):
    token = fixture.admin_token(runtime)
    original = fixture.request(client_url, token=token)
    short = copy.deepcopy(original)
    short.setdefault("attributes", {})["access.token.lifespan"] = "2"
    fixture.request(client_url, short, token, "PUT")
    try:
        browser = Browser(base, runtime["issuer"])
        browser.login(runtime["users"]["alice"])
        browser.session()
        old_cookie = "mddiff-session=" + browser.cookie()
        time.sleep(3)
        check(http(base, "/auth/session", headers={"Cookie": old_cookie})[0] == 401, "Expired web session remained authenticated.")
        check(http(base, "/api/state", headers={"Cookie": old_cookie})[0] == 401, "Expired cookie still exposes a workspace.")
    finally:
        fixture.request(client_url, original, fixture.admin_token(runtime), "PUT")


def test_role_removal(runtime, base):
    browser = Browser(base, runtime["issuer"])
    browser.login(runtime["users"]["alice"])
    browser.session()
    old_cookie = "mddiff-session=" + browser.cookie()
    admin = fixture.admin_token(runtime)
    realm = fixture.BASE + "/admin/realms/" + fixture.REALM
    user_url = realm + "/users/" + runtime["users"]["alice"]["id"]
    role = fixture.request(realm + "/roles/mddiff-user", token=admin)
    try:
        with event_stream(browser) as (done, result):
            fixture.request(user_url + "/role-mappings/realm", [role], admin, "DELETE")
            status = http(base, "/api/state", headers={"Cookie": old_cookie})[0]
            check(status in (401, 403), "Removing admission alone retained token privileges in the pinned fixture.")
            print("Hosted auth: removing the role alone was rejected before provider logout.")
            check(http(base, "/api/state", headers={"Cookie": old_cookie})[0] in (401, 403), "The documented revocation policy did not invalidate the original session.")
            check(http(base, "/auth/session", headers={"Cookie": old_cookie})[0] in (401, 403), "Revoked session metadata remains accessible.")
            check(done.wait(8) and result.get("eof"), "Revoked SSE did not reach EOF within eight seconds.")
    finally:
        fixture.request(user_url + "/role-mappings/realm", [role], fixture.admin_token(runtime), "POST")
    check(http(base, "/api/state", headers={"Cookie": old_cookie})[0] in (401, 403), "Restoring admission resurrected a revoked browser session.")


def test_provider_logout(runtime, base):
    browser = Browser(base, runtime["issuer"])
    browser.login(runtime["users"]["bob"])
    browser.session()
    old_cookie = "mddiff-session=" + browser.cookie()
    user_url = fixture.BASE + "/admin/realms/" + fixture.REALM + "/users/" + runtime["users"]["bob"]["id"]
    with event_stream(browser) as (done, result):
        fixture.request(user_url + "/logout", token=fixture.admin_token(runtime), method="POST")
        check(http(base, "/api/state", headers={"Cookie": old_cookie})[0] == 401, "Provider logout alone did not invalidate an admitted session.")
        check(done.wait(8) and result.get("eof"), "Provider logout did not close SSE within eight seconds.")


def main():
    phase = "setup"
    try:
        binary = ROOT / "target/release/mddiff"
        check(binary.is_file(), "Compile the main binary first: cargo build --release.")
        runtime = load_runtime(fixture.RUNTIME)
        check(runtime["issuer"] == "http://localhost:8848/realms/mddiff-spike", "This test only permits the isolated local Keycloak fixture.")
        check(runtime["resource_client"]["client_id"] == "mddiff-resource", "Unexpected introspection client.")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        base = f"http://127.0.0.1:{port}"
        config = {
            "issuer": runtime["issuer"], "public_origin": base, "client_id": "mddiff-hosted-web",
            "introspection_client_id": "mddiff-resource",
            "introspection_client_secret": runtime["resource_client"]["secret"],
            "audience": "mddiff-resource", "allow_insecure_loopback": True,
        }
        config_path = fixture.DIRECTORY / "hosted.json"
        fixture.private_write(config_path, json.dumps(config) + "\n")
        with web_client(runtime, base) as client_url, running(binary, base, config_path):
            phase = "anonymous boundaries"
            test_anonymous(base)
            print("Hosted auth: anonymous boundaries passed.")
            phase = "browser identities and admission"
            alice, bob = test_browsers(runtime, base)
            print("Hosted auth: separate browser identities, admission and local routes absent passed.")
            phase = "callback binding and replay"
            test_callback_binding(runtime, base)
            print("Hosted auth: real callback browser binding and replay rejection passed.")
            phase = "account switching and session rotation"
            test_session_rotation(runtime, base)
            print("Hosted auth: account switching rotated SID/CSRF and invalidated the old session.")
            phase = "logout protection and replay"
            test_logout(alice, bob)
            print("Hosted auth: logout CSRF, cookie removal and replay rejection passed.")
            phase = "web session expiry"
            test_expiry(runtime, base, client_url)
            print("Hosted auth: expired browser session rejected.")
            phase = "role removal policy and SSE"
            test_role_removal(runtime, base)
            print("Hosted auth: role removal policy, SSE EOF and no session resurrection passed.")
            phase = "provider logout and SSE"
            test_provider_logout(runtime, base)
            print("Hosted auth: provider logout with admission unchanged and SSE EOF passed.")
        print("Main hosted server + real synthetic Keycloak integration passed; social login remains deferred.")
    except CheckFailed as error:
        raise SystemExit(f"Hosted auth failed during {phase}: {error}") from None
    except Exception:
        # OAuth exceptions can contain codes or token data; never print raw errors or responses.
        raise SystemExit(f"Hosted auth failed during {phase}; provider response details were not logged.") from None


if __name__ == "__main__":
    main()
