#!/usr/bin/env python3
"""Test private, encrypted connections with audited HTTP providers and synthetic OIDC users."""

from contextlib import closing, contextmanager
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import select
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
from urllib.error import HTTPError
from urllib.parse import parse_qs, urlencode, urlsplit, urlunsplit
from urllib.request import HTTPCookieProcessor, ProxyHandler, Request, build_opener

from connections_fixture import providers
from test_hosted_auth import Browser, CheckFailed, NAVIGATION, Redirects, ROOT, check, fixture, load_runtime, web_client


@contextmanager
def application(base, oidc, connections, data):
    process = subprocess.Popen([str(ROOT / "target/release/mddiff"), "--mode", "hosted", "--port", str(urlsplit(base).port),
                                "--oidc-config", str(oidc), "--connections-config", str(connections), "--data-dir", str(data)],
                               cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        check(bool(select.select([process.stdout], [], [], 10)[0]), "Connections server did not announce its URL.")
        check(process.stdout.readline().strip() == "MDdiff: " + base, "Connections server did not start at its configured origin.")
        yield process
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        process.stdout.close()
        process.stderr.close()


def request(browser, provider, path="/api/connections", method="GET", body=None, headers=None, protect=True):
    outgoing = {} if method == "GET" or not protect else {"Origin": browser.base, "X-CSRF-Token": browser.session()["csrf_token"]}
    outgoing.update(headers or {})
    if body is not None:
        outgoing["Content-Type"] = "application/json"
    query = Request(browser.base + path, data=None if body is None else json.dumps(body).encode(), headers=outgoing, method=method)
    try:
        response = browser.opener.open(query, timeout=15)
    except HTTPError as error:
        response = error
    with response:
        raw = response.read(4 * 1024 * 1024)
        check(all(secret.encode() not in raw for secret in provider.secrets), "A public response exposed a provider credential or OAuth code.")
        return response.status, raw


def destinations(browser, provider):
    status, raw = request(browser, provider)
    check(status == 200, "The admitted owner cannot list connections.")
    value = json.loads(raw)
    check(value["enabled"] is True and set(value) == {"enabled", "destinations"}, "Connections listing has unexpected public fields.")
    entries = {item["id"]: item for item in value["destinations"]}
    check(set(entries) == {"notes", "jira-alpha", "jira-beta"}, "Connections listing omitted an approved destination.")
    check(all(set(item) == {"id", "provider", "label", "url", "connected", "revision"} for item in entries.values()), "Connections listing exposed private metadata.")
    return entries


def mutate(browser, provider, destination, action, revision, extra=None, expected=200):
    method = "DELETE" if action == "" else "POST"
    path = "/api/connections/" + destination + ("/" + action if action else "")
    status, raw = request(browser, provider, path, method, {"expected_revision": revision, **(extra or {})})
    check(status == expected, f"Connection mutation returned HTTP {status}, expected {expected}.")
    return json.loads(raw) if status == 200 else None


def connect_memos(browser, provider, name, revision):
    mutate(browser, provider, "notes", "memos", revision,
           {"token": provider.memos[name], "acknowledge_full_access": True})
    item = destinations(browser, provider)["notes"]
    check(item["connected"] and item["revision"] == revision + 1, "Memos did not persist exactly one owner generation.")
    return item["revision"]


def authorization(browser, provider, revision, destination="jira-alpha", user="alice"):
    result = mutate(browser, provider, destination, "authorize", revision)
    url = result["authorization_url"]
    parsed = urlsplit(url)
    check(urlsplit(provider.base).netloc == parsed.netloc and parsed.path == "/authorize", "Jira authorization escaped its isolated fixture.")
    query = parse_qs(parsed.query)
    check(set(query.get("scope", [""])[0].split()) == {"read:jira-work", "offline_access"}, "Jira requested excessive or insufficient OAuth scopes.")
    check(bool(query.get("state", [""])[0]), "Jira authorization omitted its user-bound state.")
    query["fixture_user"] = [user]
    query = Request(urlunsplit(parsed._replace(query=urlencode(query, doseq=True))))
    try:
        response = build_opener(ProxyHandler({}), Redirects()).open(query, timeout=10)
    except HTTPError as error:
        response = error
    with response:
        check(response.status == 302, "Synthetic Jira did not produce its authorization callback.")
        callback = response.headers.get("Location", "")
        check(callback.startswith(browser.base + "/connections/jira/callback?"), "Jira returned an unexpected application callback.")
        return callback.removeprefix(browser.base)


def callback(browser, provider, path, expected="connected"):
    opener = build_opener(ProxyHandler({}), HTTPCookieProcessor(browser.cookies), Redirects())
    try:
        response = opener.open(Request(browser.base + path, headers=NAVIGATION), timeout=15)
    except HTTPError as error:
        response = error
    with response:
        raw = response.read(1024 * 1024)
        check(all(secret.encode() not in raw for secret in provider.secrets), "A callback response exposed its OAuth credential.")
        check(response.status in (302, 303) and response.headers.get("Location") == "/?connection=" + expected,
              "Jira callback returned an unexpected completion result.")


def login(runtime, base):
    browsers = [Browser(base, runtime["issuer"]) for _ in range(2)]
    for name, browser in zip(("alice", "bob"), browsers):
        browser.login(runtime["users"][name])
    return browsers


def inspect_database(data, runtime, provider, expected):
    database = data / "work.sqlite3"
    for path in data.iterdir():
        if path.is_file():
            raw = path.read_bytes()
            check(all(secret.encode() not in raw for secret in provider.secrets), "SQLite or its journal stored a plaintext provider credential.")
    with closing(sqlite3.connect(database)) as connection:
        check(connection.execute("PRAGMA user_version").fetchone()[0] == 2, "Encrypted connection storage has an unexpected schema version.")
        rows = connection.execute("SELECT issuer, subject, provider, destination, generation, nonce, ciphertext FROM encrypted_connections").fetchall()
    by_owner = {user["id"]: name for name, user in runtime["users"].items()}
    observed = {}
    ciphertexts = {}
    for issuer, subject, kind, destination, revision, nonce, ciphertext in rows:
        check(issuer == runtime["issuer"] and subject in by_owner, "A connection has an unverified or foreign owner.")
        owner = by_owner[subject]
        observed[owner, kind, destination] = (revision, ciphertext is not None)
        if ciphertext is not None:
            check(len(nonce) == 12 and len(ciphertext) >= 16, "A connected credential lacks an authenticated encrypted envelope.")
            ciphertexts[owner, kind, destination] = (nonce, ciphertext)
        else:
            check(nonce is None, "A disconnected tombstone retained an encryption nonce.")
    check(observed == expected, "Connection persistence lost an owner, CAS generation or disconnect tombstone.")
    return ciphertexts


def main():
    phase = "setup"
    try:
        runtime = load_runtime(fixture.RUNTIME)
        check(runtime["issuer"] == "http://localhost:8848/realms/mddiff-spike", "Only the isolated synthetic identity fixture is allowed.")
        with tempfile.TemporaryDirectory(prefix="mddiff-connections-") as directory, providers() as provider:
            directory = Path(directory)
            data = directory / "data"
            data.mkdir(mode=0o700)
            key = directory / "external.key"
            key.write_bytes(os.urandom(32))
            key.chmod(0o600)
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                base = f"http://127.0.0.1:{listener.getsockname()[1]}"
            oidc, config = directory / "oidc.json", directory / "connections.json"
            fixture.private_write(oidc, json.dumps({"issuer": runtime["issuer"], "public_origin": base,
                "client_id": "mddiff-hosted-web", "introspection_client_id": "mddiff-resource",
                "introspection_client_secret": runtime["resource_client"]["secret"], "audience": "mddiff-resource", "allow_insecure_loopback": True}))
            fixture.private_write(config, json.dumps({"key_file": str(key),
                "memos": [{"id": "notes", "label": "Memos sintético", "url": provider.base, "addresses": ["127.0.0.1"]}],
                "jira": {"client_id": provider.client_id, "client_secret": provider.client_secret, "sites": provider.sites},
                "allow_insecure_loopback": True, "jira_test_origin": provider.base}))
            with web_client(runtime, base):
                with application(base, oidc, config, data) as process:
                    phase = "owner isolation, cookie-only access and Memos consent"
                    alice, bob = login(runtime, base)
                    captured = {}
                    for name, browser in (("alice", alice), ("bob", bob)):
                        code, _ = request(browser, provider, "/api/work", "PUT", {"expected_revision": 0,
                            "base": {"label": "Capturado.md", "markdown": f"# Trabajo privado de {name}\n"}, "candidate": None})
                        check(code == 200, "Could not seed the owner's captured work.")
                        captured[name] = json.loads(request(browser, provider, "/api/work")[1])
                    for browser in (alice, bob):
                        check(all(not item["connected"] and item["revision"] == 0 for item in destinations(browser, provider).values()), "Fresh owner inherited another user's credentials.")
                    check(request(alice, provider, headers={"Authorization": "Bearer synthetic-invalid"})[0] == 403, "Connections admitted bearer credentials.")
                    for headers in ({}, {"Origin": base}, {"Origin": "https://unapproved.example", "X-CSRF-Token": alice.session()["csrf_token"]}):
                        code, _ = request(alice, provider, "/api/connections/notes/memos", "POST", {"expected_revision": 0,
                            "token": provider.memos["alice"], "acknowledge_full_access": True}, headers=headers, protect=False)
                        check(code == 403, "A connector mutation bypassed browser origin or CSRF protection.")
                    status, _ = request(alice, provider, "/api/connections/notes/memos", "POST", {"expected_revision": 0, "token": provider.memos["alice"], "acknowledge_full_access": False})
                    check(status in (400, 422), "Memos connected without acknowledging its full-access token.")
                    connect_memos(alice, provider, "alice", 0)
                    check(not destinations(bob, provider)["notes"]["connected"], "Another owner inherited the Memos token.")
                    connect_memos(bob, provider, "bob", 0)
                    mutate(alice, provider, "notes", "check", 1)
                    check(provider.audit[-1]["user"] == "alice", "Alice's Memos check sent another owner's credential.")
                    mutate(bob, provider, "notes", "check", 1)
                    check(provider.audit[-1]["user"] == "bob", "Bob's Memos check sent another owner's credential.")
                    mutate(alice, provider, "notes", "", 0, expected=409)
                    mutate(alice, provider, "notes", "memos", 1, {"token": provider.memos["bob"], "acknowledge_full_access": True, "owner_id": "bob"}, expected=422)
                    status, _ = request(alice, provider, "/api/connections/notes/check", "POST", {"expected_revision": provider.memos["alice"]})
                    check(status == 422, "Malformed revision was accepted or exposed a credential in parser diagnostics.")
                    for flag in ("redirect", "oversized"):
                        setattr(provider, flag, True)
                        code, _ = request(alice, provider, "/api/connections/notes/check", "POST", {"expected_revision": 1})
                        setattr(provider, flag, False)
                        check(code in (400, 502), "Memos accepted a provider redirect or oversized response.")
                    provider.revoked.add("alice")
                    code, _ = request(alice, provider, "/api/connections/notes/check", "POST", {"expected_revision": 1})
                    check(code in (400, 502), "A revoked Memos token still passed its provider check.")
                    provider.revoked.clear()
                    check(destinations(alice, provider)["notes"]["revision"] == 1, "A failed provider check changed the connection generation.")

                    phase = "Jira state, cross-session callback and disconnect CAS"
                    path = authorization(alice, provider, 0)
                    callback(bob, provider, path, "error")
                    another_alice = Browser(base, runtime["issuer"])
                    another_alice.login(runtime["users"]["alice"])
                    path = authorization(alice, provider, 0)
                    callback(another_alice, provider, path, "error")
                    check(not destinations(alice, provider)["jira-alpha"]["connected"], "A second session of the same user completed another session's callback.")
                    path = authorization(alice, provider, 0)
                    parsed = urlsplit(path)
                    query = parse_qs(parsed.query)
                    query["state"] = ["synthetic-invalid-state"]
                    callback(alice, provider, urlunsplit(parsed._replace(query=urlencode(query, doseq=True))), "error")
                    path = authorization(alice, provider, 0)
                    parsed = urlsplit(path)
                    query = parse_qs(parsed.query)
                    query.pop("code")
                    query["error"] = ["access_denied"]
                    callback(alice, provider, urlunsplit(parsed._replace(query=urlencode(query, doseq=True))), "error")
                    path = authorization(alice, provider, 0)
                    mutate(alice, provider, "jira-alpha", "", 0)
                    callback(alice, provider, path, "error")
                    check(destinations(alice, provider)["jira-alpha"]["revision"] == 1 and not destinations(alice, provider)["jira-alpha"]["connected"], "A late callback changed its disconnect tombstone.")
                    callback(alice, provider, authorization(alice, provider, 1))
                    callback(bob, provider, authorization(bob, provider, 0, user="bob"))
                    mutate(alice, provider, "jira-alpha", "check", 2)
                    check(provider.audit[-1]["user"] == "alice", "Alice's Jira check sent another owner's OAuth token.")
                    mutate(bob, provider, "jira-alpha", "check", 1)
                    check(provider.audit[-1]["user"] == "bob", "Bob's Jira check sent another owner's OAuth token.")
                    provider.resources_override = [{"id": provider.sites[0]["cloud_id"], "url": provider.sites[1]["url"], "scopes": ["read:jira-work"]}]
                    code, _ = request(alice, provider, "/api/connections/jira-alpha/check", "POST", {"expected_revision": 2})
                    provider.resources_override = None
                    check(code in (400, 502), "Jira accepted a cloud ID paired with another destination URL.")

                    phase = "rotating refresh tokens and captured-work preservation"
                    mutate(alice, provider, "jira-alpha", "", 2)
                    provider.token_lifespan = 1
                    callback(alice, provider, authorization(alice, provider, 3))
                    for revision in (4, 5):
                        time.sleep(2)
                        mutate(alice, provider, "jira-alpha", "check", revision)
                        check(destinations(alice, provider)["jira-alpha"]["revision"] == revision + 1, "Jira refresh did not atomically rotate its encrypted generation.")
                    provider.token_lifespan = 3600
                    check(len(provider.used_refresh) == 2, "Jira did not consume two distinct rotating refresh tokens.")
                    with ThreadPoolExecutor(max_workers=1) as pool, provider.pause("/oauth/token/accessible-resources") as gate:
                        pending_check = pool.submit(request, alice, provider, "/api/connections/jira-alpha/check", "POST", {"expected_revision": 6})
                        check(gate.entered.wait(5), "The refresh check did not reach its audited provider read.")
                        check(destinations(alice, provider)["jira-alpha"]["revision"] == 7, "The refreshed token was not committed before the provider read.")
                        mutate(alice, provider, "jira-alpha", "", 7)
                        gate.release.set()
                        check(pending_check.result(timeout=5)[0] == 409, "An in-flight refresh check returned stale success after disconnect.")
                    check(not destinations(alice, provider)["jira-alpha"]["connected"], "An in-flight refresh resurrected its disconnected credentials.")
                    callback(alice, provider, authorization(alice, provider, 8))
                    expected = {("alice", "memos", "notes"): (1, True), ("bob", "memos", "notes"): (1, True),
                                ("alice", "jira", "jira-alpha"): (9, True), ("bob", "jira", "jira-alpha"): (1, True)}
                    provider.assert_read_only()
                    process.kill()
                    process.wait(timeout=5)
                encrypted_before = inspect_database(data, runtime, provider, expected)

                phase = "crash restart, ciphertext rotation and private disconnects"
                with application(base, oidc, config, data):
                    alice, bob = login(runtime, base)
                    for name, browser in (("alice", alice), ("bob", bob)):
                        check(json.loads(request(browser, provider, "/api/work")[1]) == captured[name], "Restart or connector operations changed the owner's captured work.")
                        metadata = destinations(browser, provider)
                        for kind, destination in (("memos", "notes"), ("jira", "jira-alpha")):
                            revision, connected = expected[name, kind, destination]
                            check((metadata[destination]["revision"], metadata[destination]["connected"]) == (revision, connected), "Restart lost an owner's encrypted connection.")
                    mutate(bob, provider, "notes", "check", 1)
                    check(provider.audit[-1]["user"] == "bob", "Restart loaded another owner's Memos credential.")
                    connect_memos(bob, provider, "bob", 1)
                    mutate(alice, provider, "notes", "", 1)
                    mutate(alice, provider, "jira-alpha", "", 9)
                    mutate(alice, provider, "notes", "", 1, expected=409)
                    check(destinations(bob, provider)["notes"]["connected"] and destinations(bob, provider)["jira-alpha"]["connected"], "Alice's disconnect affected Bob's credentials.")
                expected["bob", "memos", "notes"] = (2, True)
                expected["alice", "memos", "notes"] = (2, False)
                expected["alice", "jira", "jira-alpha"] = (10, False)
                encrypted_after = inspect_database(data, runtime, provider, expected)
                check(encrypted_before["bob", "memos", "notes"] != encrypted_after["bob", "memos", "notes"], "Re-saving a token reused its ciphertext and nonce.")
                with application(base, oidc, config, data):
                    alice, bob = login(runtime, base)
                    for name, browser in (("alice", alice), ("bob", bob)):
                        check(json.loads(request(browser, provider, "/api/work")[1]) == captured[name], "Disconnect discarded the owner's captured document.")
                    check(not destinations(alice, provider)["notes"]["connected"] and not destinations(alice, provider)["jira-alpha"]["connected"], "Restart resurrected disconnected credentials.")
                    mutate(alice, provider, "notes", "", 1, expected=409)
                    mutate(bob, provider, "notes", "check", 2)
                provider.assert_read_only()
        print("Connections: personal Memos/Jira, exact read scopes, callback isolation, refresh rotation, CAS, encrypted crash recovery, private disconnects and audited reads passed.")
    except CheckFailed as error:
        raise SystemExit(f"Connections failed during {phase}: {error}") from None
    except AssertionError:
        raise SystemExit(f"Connections failed during {phase}: provider audit rejected an unexpected request.") from None
    except Exception:
        raise SystemExit(f"Connections failed during {phase}; credential-bearing diagnostics were not logged.") from None


if __name__ == "__main__":
    main()
