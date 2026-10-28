#!/usr/bin/env python3
"""Capture synthetic Memos notes with private credentials, exact bytes and persistent provenance."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
from pathlib import Path
import socket
import sqlite3
import tempfile
import time
from types import SimpleNamespace
from urllib.error import HTTPError
from urllib.request import Request

from memos_fixture import memos
from test_connections import application, login
from test_hosted_auth import Browser, CheckFailed, ROOT, check, fixture, load_runtime, web_client
from hosted_mcp_fixture import audience_clients, token
from test_hosted_mcp import rpc, tool
from test_hosted_persistence import Events


def request(browser, provider, path="/api/work", method="GET", body=None, headers=None, protect=True):
    outgoing = {} if method == "GET" or not protect else {"Origin": browser.base, "X-CSRF-Token": browser.session()["csrf_token"]}
    outgoing.update(headers or {})
    if body is not None:
        outgoing["Content-Type"] = "application/json"
    query = Request(browser.base + path, data=None if body is None else json.dumps(body).encode(), headers=outgoing, method=method)
    try:
        response = browser.opener.open(query, timeout=20)
    except HTTPError as error:
        response = error
    with response:
        # A 2 MiB source made of JSON control escapes can produce a much larger rendered snapshot.
        raw = response.read(64 * 1024 * 1024)
        check(len(raw) < 64 * 1024 * 1024, "A capture response exceeded the test's bounded reader.")
        check(all(secret.encode() not in raw for secret in provider.secrets), "A response exposed a Memos credential.")
        return response.status, raw


def work(browser, provider):
    status, raw = request(browser, provider)
    check(status == 200, "The owner cannot read captured work.")
    return json.loads(raw)


def snapshot(browser, provider):
    status, raw = request(browser, provider, "/api/state")
    check(status == 200, "The owner cannot read the captured comparison.")
    return json.loads(raw)


def connect(browser, provider, user, revision):
    status, raw = request(browser, provider, "/api/connections/notes/memos", "POST", {
        "expected_revision": revision, "token": provider.memos[user], "acknowledge_full_access": True})
    check(status == 200 and json.loads(raw)["revision"] == revision + 1, "Could not connect the owner's synthetic Memos account.")


def capture(browser, provider, user, uid, reference, events=None, other=None):
    before = work(browser, provider)
    start = int(time.time())
    status, raw = request(browser, provider, "/api/sources/memos", "POST", {
        "destination": "notes", "reference": reference, "expected_revision": before["revision"]})
    check(status == 200, f"Memos capture returned HTTP {status}.")
    visible = json.loads(raw)
    after = work(browser, provider)
    expected = provider.documents[user, uid]
    check(after["revision"] == before["revision"] + 1 and visible["revision"] == after["revision"], "Capture did not publish exactly one owner revision.")
    check(after["base"] == {"label": "Memos · " + uid + ".md", "markdown": expected["content"]}, "Capture changed the memo's raw Markdown or label.")
    check(after["candidate"] == before["candidate"], "Capturing an original changed its current proposal.")
    source = after.get("base_source", {})
    check(source.get("provider") == "memos" and source.get("destination") == "notes"
          and source.get("resource") == "memos/" + uid and source.get("url") == provider.base + "/memos/" + uid,
          "Captured provenance does not identify the approved memo destination.")
    check(isinstance(source.get("captured_at"), int) and start <= source["captured_at"] <= int(time.time()), "Capture omitted a valid timestamp.")
    check(source.get("updated_at") == expected.get("updateTime"), "The optional provider timestamp was changed or invented.")
    check(set(source) <= {"provider", "destination", "resource", "url", "captured_at", "updated_at"}, "Captured provenance exposed unexpected private metadata.")
    check(visible.get("base_source") == source and visible["base_path"] == after["base"]["label"], "Snapshot and saved work disagree about their original source.")
    check(provider.audit[-1]["user"] == user and provider.audit[-1]["path"] == "/api/v1/memos/" + uid, "Capture sent another owner's token or fetched another memo.")
    if events is not None:
        events.revision(after["revision"])
    if other is not None:
        other.unchanged()
    return after


def failed(browser, provider, body, expected, events, before_network=False, headers=None, protect=True):
    before = work(browser, provider)
    visible = snapshot(browser, provider)
    count = len(provider.audit)
    status, _ = request(browser, provider, "/api/sources/memos", "POST", body, headers, protect)
    check(status in expected, f"Rejected memo capture returned unexpected HTTP {status}.")
    check(work(browser, provider) == before and snapshot(browser, provider) == visible, "A rejected capture changed the previous work or comparison.")
    events.unchanged()
    if before_network:
        check(len(provider.audit) == count, "A rejected reference or schema reached the provider network.")


def inspect(data, provider, runtime, expected):
    with closing(sqlite3.connect(data / "work.sqlite3")) as connection:
        check(connection.execute("PRAGMA user_version").fetchone()[0] == 3, "Memos provenance did not migrate storage to schema 3.")
        rows = connection.execute("SELECT subject, revision, base_markdown, candidate_markdown, base_source FROM current_work").fetchall()
    check(len(rows) == 2, "Captures created history rows or lost a private owner.")
    owners = {user["id"]: name for name, user in runtime["users"].items()}
    for subject, revision, original, proposal, source in rows:
        name = owners.get(subject)
        check(name in expected, "Persisted source has an unknown identity.")
        item = expected[name]
        check(revision == item["revision"] and original == (item["base"]["markdown"] if item["base"] else None)
              and proposal == (item["candidate"]["markdown"] if item["candidate"] else None)
              and (json.loads(source) if source is not None else None) == item.get("base_source"), "SQLite changed the captured source, raw text or proposal.")
    for path in data.iterdir():
        if path.is_file():
            check(all(secret.encode() not in path.read_bytes() for secret in provider.secrets), "A captured source leaked its provider credential into storage.")


def main():
    phase = "setup"
    streams = []
    try:
        runtime = load_runtime(fixture.RUNTIME)
        check(runtime["issuer"] == "http://localhost:8848/realms/mddiff-spike", "Only the isolated synthetic identity fixture is allowed.")
        with tempfile.TemporaryDirectory(prefix="mddiff-memos-source-") as temporary, memos() as provider:
            directory = Path(temporary)
            data = directory / "data"
            data.mkdir(mode=0o700)
            key = directory / "external.key"
            key.write_bytes(os.urandom(32)); key.chmod(0o600)
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0))
                base = f"http://127.0.0.1:{listener.getsockname()[1]}"
            oidc, config = directory / "oidc.json", directory / "connections.json"
            fixture.private_write(oidc, json.dumps({"issuer": runtime["issuer"], "public_origin": base, "client_id": "mddiff-hosted-web",
                "introspection_client_id": "mddiff-resource", "introspection_client_secret": runtime["resource_client"]["secret"],
                "audience": "mddiff-resource", "allow_insecure_loopback": True}))
            fixture.private_write(config, json.dumps({"key_file": str(key), "allow_insecure_loopback": True,
                "memos": [{"id": "notes", "label": "Memos sintético", "url": provider.base, "addresses": ["127.0.0.1"]}]}))
            with audience_clients(runtime, base + "/mcp"), web_client(runtime, base):
                with application(base, oidc, config, data) as process:
                    phase = "private sources, raw Markdown and reference formats"
                    alice, bob = login(runtime, base)
                    for name, browser in (("alice", alice), ("bob", bob)):
                        connect(browser, provider, name, 0)
                        status, _ = request(browser, provider, "/api/work", "PUT", {"expected_revision": 0,
                            "base": {"label": "Original.md", "markdown": "# Original\n"},
                            "candidate": {"label": "Propuesta.md", "markdown": "# Propuesta privada de " + name + "\n"}})
                        check(status == 200, "Could not seed the owner's original and proposal.")
                        streams.append(Events(browser, 1))
                    events, other = streams
                    capture(alice, provider, "alice", "shared", "shared", events, other)
                    capture(bob, provider, "bob", "shared", "memos/shared", other, events)
                    capture(alice, provider, "alice", "shared", provider.base + "/memos/shared", events, other)
                    capture(alice, provider, "alice", "plain", "plain", events, other)
                    capture(alice, provider, "alice", "empty", "empty", events, other)
                    capture(alice, provider, "alice", "shared", "shared", events, other)
                    before = work(alice, provider)
                    provider.note("alice", "shared", before["base"]["markdown"] + "\r\nCambio remoto sin sincronizar.\r\n", "2026-10-28T02:03:04Z")
                    check(work(alice, provider) == before, "Changing the provider silently replaced the captured original.")
                    events.unchanged()

                    phase = "MCP and PATCH proposals retain original provenance"
                    env = SimpleNamespace(base=base, trust=None, runtime=runtime)
                    access = token(env)
                    rpc(env, access, "initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "memos-source-test", "version": "1"}})
                    candidate = {"label": "Propuesta MCP.md", "markdown": "# Cambio de IA\n"}
                    result = tool(env, access, "submit_proposal", {"expected_revision": before["revision"], "candidate": candidate})
                    check(not result.get("isError"), "MCP could not propose against a captured original.")
                    events.revision(before["revision"] + 1)
                    after = work(alice, provider)
                    check(after["base"] == before["base"] and after.get("base_source") == before.get("base_source") and after["candidate"] == candidate, "MCP changed captured original provenance.")
                    status, _ = request(alice, provider, "/api/work", "PATCH", {"expected_revision": after["revision"], "candidate": {"label": "PATCH.md", "markdown": "# Segunda propuesta\n"}})
                    check(status == 200, "PATCH could not propose against a captured original.")
                    events.revision(after["revision"] + 1)
                    check(work(alice, provider).get("base_source") == before.get("base_source"), "PATCH discarded original provenance.")
                    other.unchanged()

                    phase = "reference validation, schema redaction and provider failures"
                    revision = work(alice, provider)["revision"]
                    valid = {"destination": "notes", "reference": "shared", "expected_revision": revision}
                    for reference in ["", "-", "../shared", "memos/../shared", "memos/shared?x=1", "shared#fragment", "shared\\other", "%73hared",
                                      "https://unapproved.example/memos/shared", provider.base + "/api/v1/memos/shared", provider.base + "/memos/../memos/shared",
                                      provider.base + "/memos/%73hared", provider.base + "/memos/shared?x=1", provider.base + "/memos/shared#x",
                                      provider.base.replace("http://", "http://user:" + provider.memos["alice"] + "@") + "/memos/shared"]:
                        failed(alice, provider, {**valid, "reference": reference}, {400}, events, before_network=True)
                    for body in [{**valid, "owner_id": "bob"}, {**valid, "destination": provider.base},
                                 {**valid, "expected_revision": provider.memos["alice"]}, {**valid, "reference": {"token": provider.memos["alice"]}}]:
                        failed(alice, provider, body, {400, 404, 422}, events, before_network=True)
                    failed(alice, provider, valid, {403}, events, before_network=True, protect=False)
                    failed(alice, provider, valid, {403}, events, before_network=True, headers={"Origin": "https://unapproved.example"})
                    failed(alice, provider, valid, {403}, events, before_network=True, headers={"Authorization": "Bearer synthetic-invalid"})
                    failed(alice, provider, {**valid, "expected_revision": revision - 1}, {409}, events)
                    failed(alice, provider, {**valid, "reference": "missing"}, {404, 502}, events)
                    for mode in ("wrong-name", "missing-content", "non-string-content", "long-update-time", "control-update-time", "non-string-update-time",
                                 "envelope", "provider-error", "redirect", "oversized-response"):
                        provider.mode = mode
                        failed(alice, provider, valid, {502}, events)
                        provider.mode = "normal"
                    start = len(provider.audit)
                    provider.revoked.add("alice")
                    failed(alice, provider, {**valid, "reference": "public"}, {502}, events)
                    provider.revoked.clear()
                    check([row["path"] for row in provider.audit[start:]] == ["/api/v1/auth/me"], "An invalid credential reached a public memo instead of failing identity validation.")
                    other.unchanged()

                    phase = "decoded UTF-8 and escaped JSON size boundaries"
                    provider.note("alice", "large", "é" * (1024 * 1024 + 1))
                    failed(alice, provider, {**valid, "reference": "large"}, {413}, events)
                    provider.note("alice", "wire", "\x01" * (2 * 1024 * 1024))
                    capture(alice, provider, "alice", "wire", "wire", events, other)
                    capture(alice, provider, "alice", "shared", "shared", events, other)

                    phase = "connection and work CAS during an in-flight provider read"
                    before = work(alice, provider)
                    with ThreadPoolExecutor(max_workers=1) as pool, provider.pause("plain") as gate:
                        pending = pool.submit(request, alice, provider, "/api/sources/memos", "POST", {"destination": "notes", "reference": "plain", "expected_revision": before["revision"]})
                        check(gate.entered.wait(5), "Capture did not reach its isolated provider read.")
                        status, _ = request(alice, provider, "/api/connections/notes", "DELETE", {"expected_revision": 1})
                        check(status == 200, "Disconnect waited behind an in-flight memo read.")
                        gate.release.set()
                        check(pending.result(timeout=5)[0] == 409, "A disconnected credential committed its in-flight capture.")
                    check(work(alice, provider) == before, "Disconnect race replaced the previous captured work.")
                    events.unchanged(); other.unchanged()
                    connect(alice, provider, "alice", 2)
                    with ThreadPoolExecutor(max_workers=1) as pool, provider.pause("plain") as gate:
                        pending = pool.submit(request, alice, provider, "/api/sources/memos", "POST", {"destination": "notes", "reference": "plain", "expected_revision": before["revision"]})
                        check(gate.entered.wait(5), "A concurrent capture did not reach its provider read.")
                        status, _ = request(alice, provider, "/api/work", "PUT", {"expected_revision": before["revision"],
                            "base": {"label": "Ganador.md", "markdown": "# Edición ganadora\n"}, "candidate": before["candidate"]})
                        check(status == 200, "A foreground document update failed during capture.")
                        winner = work(alice, provider)
                        events.revision(winner["revision"])
                        gate.release.set()
                        check(pending.result(timeout=5)[0] == 409, "A slow capture overwrote the newer work revision.")
                    check(work(alice, provider) == winner and winner.get("base_source") is None, "A rejected slow capture changed its winning manual original.")
                    events.unchanged(); other.unchanged()
                    capture(alice, provider, "alice", "shared", "shared", events, other)

                    phase = "logout during a provider read and durable captured provenance"
                    expected = {"alice": work(alice, provider), "bob": work(bob, provider)}
                    with ThreadPoolExecutor(max_workers=1) as pool, provider.pause("plain") as gate:
                        pending = pool.submit(request, alice, provider, "/api/sources/memos", "POST", {"destination": "notes", "reference": "plain", "expected_revision": expected["alice"]["revision"]})
                        check(gate.entered.wait(5), "Logout race did not reach the provider read.")
                        check(request(alice, provider, "/auth/logout", "POST", {})[0] == 204, "Could not revoke the web session during capture.")
                        gate.release.set()
                        check(pending.result(timeout=5)[0] == 401, "A logged-out web session committed an in-flight memo capture.")
                    check(events.closed.wait(8), "Logout left its captured-work event stream open.")
                    other.unchanged()
                    provider.assert_read_only()
                    process.kill(); process.wait(timeout=5)
                inspect(data, provider, runtime, expected)
                with application(base, oidc, config, data):
                    alice, bob = login(runtime, base)
                    check(work(alice, provider) == expected["alice"] and work(bob, provider) == expected["bob"], "Restart changed captured provenance, raw Markdown or private proposals.")
                    status, _ = request(alice, provider, "/api/work", "DELETE", {"expected_revision": expected["alice"]["revision"]})
                    check(status == 200, "Could not delete the captured current work.")
                    expected["alice"] = work(alice, provider)
                    check(expected["alice"]["base"] is None and expected["alice"]["candidate"] is None and expected["alice"].get("base_source") is None, "Deleting work retained its captured source.")
                inspect(data, provider, runtime, expected)
                with application(base, oidc, config, data):
                    alice, bob = login(runtime, base)
                    check(work(alice, provider) == expected["alice"] and work(bob, provider) == expected["bob"], "Restart resurrected a deleted source or changed the other owner.")
                provider.assert_read_only()
        print("Memos source: private audited GETs, raw text/provenance, references, limits, MCP/PATCH preservation, disconnect/work/logout races and crash recovery passed.")
    except CheckFailed as error:
        raise SystemExit(f"Memos source failed during {phase}: {error}") from None
    except AssertionError:
        raise SystemExit(f"Memos source failed during {phase}: provider audit rejected an unexpected request.") from None
    except Exception:
        raise SystemExit(f"Memos source failed during {phase}; credential-bearing diagnostics were not logged.") from None
    finally:
        for stream in streams:
            stream.closed.wait(5)
            stream.thread.join(timeout=1)


if __name__ == "__main__":
    main()
