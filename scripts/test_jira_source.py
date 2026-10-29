#!/usr/bin/env python3
"""Read Jira descriptions through personal OAuth, bounded ADF and owner-atomic captures."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
from pathlib import Path
import socket
import sqlite3
import tempfile
from types import SimpleNamespace

from jira_source_fixture import jira_sources, document
from test_connections import application, login, authorization, callback, mutate
from test_hosted_auth import Browser, CheckFailed, check, fixture, load_runtime, web_client
from test_memos_source import request, work, snapshot
from test_hosted_persistence import Events
from hosted_mcp_fixture import audience_clients, token
from test_hosted_mcp import tool


PATH = "/ex/jira/synthetic-alpha/rest/api/3/issue/PROJ-1"


def capture(browser, provider, reference="PROJ-1", expected=None):
    if expected is None:
        expected = work(browser, provider)["revision"]
    return request(browser, provider, "/api/sources/jira", "POST", {
        "destination": "jira-alpha", "reference": reference, "expected_revision": expected})


def rejected(browser, provider, reference, status, events, no_network=False):
    before, visible = work(browser, provider), snapshot(browser, provider)
    calls = len(provider.audit)
    code, raw = capture(browser, provider, reference)
    check(code == status, f"Rejected Jira capture returned HTTP {code}, expected {status}.")
    check(work(browser, provider) == before and snapshot(browser, provider) == visible, "Failed Jira capture overwrote current work.")
    events.unchanged()
    if no_network:
        check(len(provider.audit) == calls, "Rejected Jira reference reached the network.")


def main():
    phase = "setup"
    streams = []
    try:
        runtime = load_runtime(fixture.RUNTIME)
        check(runtime["issuer"] == "http://localhost:8848/realms/mddiff-spike", "Only the isolated synthetic issuer is allowed.")
        with tempfile.TemporaryDirectory(prefix="mddiff-jira-source-") as temporary, jira_sources() as provider:
            directory = Path(temporary); data = directory / "data"; data.mkdir(mode=0o700)
            key = directory / "external.key"; key.write_bytes(os.urandom(32)); key.chmod(0o600)
            with socket.socket() as listener:
                listener.bind(("127.0.0.1", 0)); base = f"http://127.0.0.1:{listener.getsockname()[1]}"
            oidc, config = directory / "oidc.json", directory / "connections.json"
            fixture.private_write(oidc, json.dumps({"issuer": runtime["issuer"], "public_origin": base, "client_id": "mddiff-hosted-web",
                "introspection_client_id": "mddiff-resource", "introspection_client_secret": runtime["resource_client"]["secret"], "audience": "mddiff-resource", "allow_insecure_loopback": True}))
            fixture.private_write(config, json.dumps({"key_file": str(key), "jira": {"client_id": provider.client_id,
                "client_secret": provider.client_secret, "sites": provider.sites}, "allow_insecure_loopback": True, "jira_test_origin": provider.base}))
            with audience_clients(runtime, base + "/mcp"), web_client(runtime, base):
                with application(base, oidc, config, data):
                    phase = "personal OAuth and private snapshots"
                    alice, bob = login(runtime, base)
                    for name, browser in (("alice", alice), ("bob", bob)):
                        callback(browser, provider, authorization(browser, provider, 0, user=name))
                        code, _ = request(browser, provider, "/api/work", "PUT", {"expected_revision": 0,
                            "base": {"label": "base.md", "markdown": "# Original\n"}, "candidate": {"label": "proposal.md", "markdown": "# Propuesta de " + name + "\n"}})
                        check(code == 200, "Could not seed the current proposal.")
                        streams.append(Events(browser, 1))
                    for i, (name, browser) in enumerate((("alice", alice), ("bob", bob))):
                        before = work(browser, provider)
                        code, raw = capture(browser, provider, "proj-1" if i == 0 else provider.sites[0]["url"] + "/browse/PROJ-1")
                        check(code == 200, f"Jira source returned HTTP {code}.")
                        captured = work(browser, provider); source = captured.get("base_source", {})
                        check(captured["candidate"] == before["candidate"] and captured["base"]["markdown"] == "Contenido privado de " + name + "\\.\n", "Jira import lost content or changed the proposal.")
                        check(source.get("provider") == "jira" and source.get("destination") == "jira-alpha" and source.get("resource") == "PROJ-1"
                              and source.get("url") == provider.sites[0]["url"] + "/browse/PROJ-1" and source.get("converter") == "adf-md-v1"
                              and source.get("title") == "Issue privado de " + name and not source.get("warnings"), "Jira source metadata is incomplete or crossed users.")
                        check(json.loads(raw)["base_source"] == source, "Saved and rendered Jira source disagree.")
                        streams[i].revision(captured["revision"]); streams[1-i].unchanged()
                    before = work(alice, provider)
                    provider.issues["alice", "PROJ-1"]["fields"]["description"] = document("Changed remotely")
                    check(work(alice, provider) == before, "Remote Jira change silently replaced the captured original.")
                    streams[0].unchanged()

                    phase = "adjacent ADF marks through the real Jira capture renderer"
                    provider.issues["alice", "PROJ-1"]["fields"]["description"] = {"type": "doc", "version": 1, "content": [{"type": "paragraph", "content": [
                        {"type": "text", "text": "a", "marks": [{"type": "strong"}]},
                        {"type": "text", "text": "b", "marks": [{"type": "strong"}]},
                        {"type": "text", "text": "c", "marks": [{"type": "em"}]},
                        {"type": "text", "text": "d", "marks": [{"type": "code"}]},
                        {"type": "text", "text": "e", "marks": [{"type": "code"}]},
                        {"type": "text", "text": "f", "marks": [{"type": "strong"}, {"type": "em"}]},
                    ]}]}
                    check(capture(alice, provider)[0] == 200, "Adjacent ADF marks failed capture.")
                    streams[0].revision(before["revision"] + 1)
                    rendered = "".join(row["left"]["html"] for row in snapshot(alice, provider)["comparison"]["rows"] if row.get("left"))
                    check("<strong>ab</strong><em>c</em><code>de</code><em><strong>f</strong></em>" in rendered and "<!--" not in rendered,
                          "Adjacent ADF marks inserted delimiter text or changed formatting in the viewer.")
                    before = work(alice, provider)

                    phase = "warnings and proposal-only agent writes"
                    provider.issues["alice", "PROJ-1"]["fields"]["description"] = {"type": "doc", "version": 1, "content": [
                        {"type": "paragraph", "content": [{"type": "text", "text": "Supported"}]},
                        {"type": "mediaSingle", "content": [{"type": "media", "attrs": {"id": "synthetic-no-download", "type": "file"}}]}]}
                    check(capture(alice, provider)[0] == 200, "Unsupported ADF content was not represented safely.")
                    streams[0].revision(before["revision"] + 1)
                    current = work(alice, provider)
                    check(current["base_source"].get("warnings") and "synthetic-no-download" in current["base"]["markdown"], "Unsupported ADF disappeared without a visible warning or fallback.")
                    env = SimpleNamespace(base=base, trust=None, runtime=runtime)
                    result = tool(env, token(env), "submit_proposal", {"expected_revision": current["revision"], "candidate": {"label": "agent.md", "markdown": "# Agent\n"}})
                    check(not result.get("isError"), "Agent could not propose against Jira source.")
                    streams[0].revision(current["revision"] + 1)
                    check(work(alice, provider)["base_source"] == current["base_source"], "Agent discarded Jira conversion warnings.")

                    phase = "expired Jira access tokens rotate and persist before source capture"
                    provider.token_lifespan = 1
                    callback(bob, provider, authorization(bob, provider, 1, user="bob"))
                    provider.token_lifespan = 3600
                    before_bob = work(bob, provider)
                    used = len(provider.used_refresh)
                    check(capture(bob, provider)[0] == 200, "Jira source could not refresh an expiring personal token.")
                    streams[1].revision(before_bob["revision"] + 1); streams[0].unchanged()
                    connections = json.loads(request(bob, provider, "/api/connections")[1])["destinations"]
                    jira = next(d for d in connections if d["id"] == "jira-alpha")
                    check(jira["revision"] == 3 and len(provider.used_refresh) == used + 1,
                          "Jira capture lost a rotating refresh token or reused a connection generation.")

                    phase = "references, fields, permissions and bounded failures"
                    for reference in ["https://evil.example/browse/PROJ-1", provider.sites[1]["url"] + "/browse/PROJ-1", "../PROJ-1", "PROJ-1?x=1", provider.sites[0]["url"] + "/browse/%50ROJ-1"]:
                        rejected(alice, provider, reference, 400, streams[0], no_network=True)
                    for upstream, status in [(403, 502), (404, 404), (429, 429), (500, 502)]:
                        provider.issue_errors["PROJ-1"] = upstream
                        rejected(alice, provider, "PROJ-1", status, streams[0])
                    provider.issue_errors.clear()
                    for description, status in [(None, 422), ({"type": "doc", "version": 2, "content": []}, 502), (document("x" * (2 * 1024 * 1024 + 1)), 413)]:
                        provider.issues["alice", "PROJ-1"]["fields"]["description"] = description
                        rejected(alice, provider, "PROJ-1", status, streams[0])
                    provider.issues["alice", "PROJ-8"] = {"key": "PROJ-8", "fields": {"summary": "Dense inline formatting", "description": {"type": "doc", "version": 1, "content": [{"type": "paragraph", "content": [
                        {"type": "text", "text": "x", "marks": [{"type": "strong" if i % 2 else "em"}]} for i in range(300)]}]}}}
                    rejected(alice, provider, "PROJ-8", 413, streams[0])
                    provider.issues.pop(("alice", "PROJ-8"))
                    provider.issues["alice", "PROJ-9"] = {"key": "PROJ-9", "fields": {"summary": "Oversized envelope", "description": document("small"), "unused": "x" * (6 * 2 * 1024 * 1024 + 64 * 1024)}}
                    rejected(alice, provider, "PROJ-9", 413, streams[0])
                    provider.issues.pop(("alice", "PROJ-9"))
                    provider.issue_redirect = True; rejected(alice, provider, "PROJ-1", 502, streams[0]); provider.issue_redirect = False
                    provider.revoked.add("alice"); rejected(alice, provider, "PROJ-1", 502, streams[0]); provider.revoked.clear()
                    before = work(alice, provider)
                    code, _ = request(alice, provider, "/api/sources/jira", "POST", {"destination": "jira-beta", "reference": "PROJ-1", "expected_revision": before["revision"]})
                    check(code == 409 and work(alice, provider) == before, "An unconnected Jira destination was read.")
                    check(request(alice, provider, "/api/sources/jira", "POST", {"destination": "jira-alpha", "reference": "PROJ-1", "expected_revision": before["revision"]}, headers={"Authorization": "Bearer invalid"})[0] == 403, "Bearer compare capability entered source capture.")
                    check(request(alice, provider, "/api/sources/jira", "POST", {"destination": "jira-alpha", "reference": "PROJ-1", "expected_revision": before["revision"]}, protect=False)[0] == 403, "Jira source bypassed CSRF/Origin.")

                    phase = "late reads, disconnect and session revocation"
                    provider.issues["alice", "PROJ-1"]["fields"]["description"] = document("Latest description")
                    with ThreadPoolExecutor(max_workers=2) as pool:
                        with provider.pause(PATH) as gate:
                            future = pool.submit(capture, alice, provider)
                            check(gate.entered.wait(5), "Jira read did not reach its controlled barrier.")
                            current = work(alice, provider)
                            result = tool(env, token(env), "submit_proposal", {"expected_revision": current["revision"], "candidate": {"label": "newer.md", "markdown": "# Newer agent proposal\n"}})
                            check(not result.get("isError"), "Agent could not write while Jira waited.")
                            streams[0].revision(current["revision"] + 1); after = work(alice, provider)
                            gate.release.set(); check(future.result()[0] == 409, "Late Jira capture overwrote a newer proposal.")
                            check(work(alice, provider) == after, "Late capture changed committed work.")
                        with provider.pause(PATH) as gate:
                            future = pool.submit(capture, alice, provider); check(gate.entered.wait(5), "Jira disconnect barrier missing.")
                            mutate(alice, provider, "jira-alpha", "", 1); gate.release.set()
                            check(future.result()[0] == 409 and work(alice, provider) == after, "Disconnected Jira credential committed an in-flight capture.")
                    streams[0].unchanged(); streams[1].unchanged()
                    before_bob = work(bob, provider)
                    with ThreadPoolExecutor(max_workers=1) as pool:
                        with provider.pause(PATH) as gate:
                            future = pool.submit(capture, bob, provider)
                            check(gate.entered.wait(5), "Jira logout barrier missing.")
                            check(request(bob, provider, "/auth/logout", "POST", {})[0] == 204, "Could not revoke the source reader session.")
                            gate.release.set()
                            check(future.result()[0] == 401, "A logged-out source reader committed its Jira capture.")
                    check(streams[1].closed.wait(5), "Revoked Jira source reader retained its event stream.")
                    bob = Browser(base, runtime["issuer"]); bob.login(runtime["users"]["bob"])
                    check(work(bob, provider) == before_bob, "Logout changed the previous Jira capture.")
                    expected = [work(alice, provider), work(bob, provider)]
                    check(bool(expected[0]["base_source"].get("warnings")), "Restart fixture must include conversion warnings.")
                phase = "persistent Jira conversion and provenance"
                with closing(sqlite3.connect(data / "work.sqlite3")) as connection:
                    rows = connection.execute("SELECT base_source FROM current_work").fetchall()
                    check(len(rows) == 2 and all(json.loads(row[0])["converter"] == "adf-md-v1" for row in rows), "Jira source did not persist by owner.")
                with application(base, oidc, config, data):
                    alice, bob = login(runtime, base)
                    check([work(alice, provider), work(bob, provider)] == expected, "Restart changed Jira conversion or private content.")
                provider.assert_read_only()
        print("Jira source: personal OAuth, site-scoped audited GETs, deterministic ADF/warnings, failures/limits, owner/CAS/disconnect races, agent preservation and restart passed.")
    except CheckFailed as error:
        raise SystemExit(f"Jira source failed during {phase}: {error}") from None
    except Exception:
        raise SystemExit(f"Jira source failed during {phase}; credential-bearing diagnostics were not logged.") from None
    finally:
        for stream in streams:
            stream.closed.wait(5); stream.thread.join(timeout=1)


if __name__ == "__main__":
    main()
