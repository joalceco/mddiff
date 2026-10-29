#!/usr/bin/env python3
"""Run real Codex OAuth and hosted MCP over verified HTTPS, with zero model turns."""

from contextlib import ExitStack
import json
import os
from pathlib import Path
import tempfile
from unittest.mock import patch

from hosted_mcp_fixture import CheckFailed, TlsBrowser, check, hosted_fixture, http, token
from test_hosted_mcp import seed
from test_hosted_persistence import Events
from codex_probe import AppServer, ProbeError, SCOPES, SERVER, complete_synthetic_login


def call(client, thread_id, name, arguments=None):
    return client.request("mcpServer/tool/call", {"server": SERVER, "threadId": thread_id,
                                               "tool": name, "arguments": arguments or {}})


def current(client, thread_id):
    result = call(client, thread_id, "get_current_work")
    check(not result.get("isError"), "Real Codex could not read its admitted user's work.")
    return result["structuredContent"]


def connect(resources, env, name):
    home = Path(resources.enter_context(tempfile.TemporaryDirectory(prefix="mddiff-codex-https-")))
    environment = dict(os.environ)
    for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
        environment.pop(key, None)
    environment.update({"CODEX_CA_CERTIFICATE": str(env.ca), "NO_PROXY": "localhost,127.0.0.1", "no_proxy": "localhost,127.0.0.1"})
    with patch.dict(os.environ, environment, clear=True):
        client = AppServer(home, env.resource)
    resources.callback(client.close)
    login = client.request("mcpServer/oauth/login", {"name": SERVER, "scopes": SCOPES, "timeoutSecs": 60})
    credentials = env.runtime["users"][name]
    complete_synthetic_login(login["authorizationUrl"], env.runtime["issuer"], env.resource,
                             credentials["username"], credentials["password"])
    done = client.receive(lambda message: message.get("method") == "mcpServer/oauthLogin/completed"
                          and message.get("params", {}).get("name") == SERVER)
    check(done["params"]["success"], "Real Codex OAuth did not complete.")
    thread = client.request("thread/start", {"cwd": str(home), "ephemeral": True,
                                            "approvalPolicy": "never", "sandbox": "read-only"})
    thread_id = thread["thread"]["id"]
    inventory = client.request("mcpServerStatus/list", {"threadId": thread_id, "serverName": SERVER, "detail": "toolsAndAuthOnly"})
    servers = [server for server in inventory["data"] if server["name"] == SERVER]
    check(len(servers) == 1 and servers[0]["authStatus"] == "oAuth", "Real Codex did not authenticate its hosted MCP server.")
    check(set(servers[0]["tools"]) == {"get_current_work", "submit_proposal"}, "Real Codex discovered an incorrect hosted tool inventory.")
    return client, thread_id


def main():
    phase = "setup"
    streams = []
    try:
        with hosted_fixture() as env, ExitStack() as resources:
            phase = "HTTPS web sessions and originals"
            sessions = []
            for name in ("alice", "bob"):
                browser = TlsBrowser(env.base, env.runtime["issuer"], env.trust)
                browser.login(env.runtime["users"][name])
                original = seed(env, browser, f"# Original privado de {name}\r\n\r\nConservar 🌳 y 日本語.\r\n")
                streams.append(Events(browser, 1))
                sessions.append({"name": name, "browser": browser, "original": original})
            for session in sessions:
                phase = "real Codex OAuth over HTTPS"
                client, thread_id = connect(resources, env, session["name"])
                session.update({"client": client, "thread_id": thread_id})
                work = current(client, thread_id)
                check(work == {"revision": 1, "base": session["original"], "candidate": None, "viewer_url": env.base + "/"},
                      "Real Codex read another account's work or returned an insecure viewer URL.")
            phase = "real Codex proposals, browser SSE and account isolation"
            for index, session in enumerate(sessions):
                proposal = {"label": "Propuesta de IA.md", "markdown": f"# Propuesta privada de {session['name']}\r\n\r\nCambio 🌲.\r\n"}
                session["proposal"] = proposal
                result = call(session["client"], session["thread_id"], "submit_proposal", {"expected_revision": 1, "candidate": proposal})
                summary = result.get("structuredContent", {})
                check(not result.get("isError") and summary.get("revision") == 2 and summary.get("viewer_url") == env.base + "/",
                      "Real Codex did not commit its proposal with an HTTPS viewer URL.")
                streams[index].revision(2)
                streams[1 - index].unchanged()
            for index, session in enumerate(sessions):
                expected = {"revision": 2, "base": session["original"], "candidate": session["proposal"]}
                codex_work = current(session["client"], session["thread_id"])
                check({key: codex_work[key] for key in expected} == expected, "Real Codex changed the original or crossed an account boundary.")
                status, raw, _ = http(env, "/api/work", opener=session["browser"].opener)
                check(status == 200 and json.loads(raw) == expected, "The browser cannot see the exact Codex proposal.")
                status, raw, _ = http(env, "/api/work", access=token(env, session["name"]))
                check(status == 200 and json.loads(raw) == expected, "The API cannot see the exact Codex proposal.")
                snapshot = json.loads(http(env, "/api/state", opener=session["browser"].opener)[1])
                check(snapshot["revision"] == 2 and not snapshot["comparison"]["source_equal"] and snapshot["comparison"]["rows"], "The viewer did not render the Codex comparison.")
                stale = call(session["client"], session["thread_id"], "submit_proposal", {"expected_revision": 1, "candidate": session["proposal"]})
                check(stale.get("isError") and stale["structuredContent"]["status"] == 409, "Real Codex overwrote a newer revision.")
                streams[index].unchanged()
        print("Real Codex MCP: verified resource HTTPS/custom CA, OAuth PKCE, two private accounts, proposal/SSE/viewer agreement and stale-write rejection passed; model turns: 0.")
    except (CheckFailed, ProbeError) as error:
        raise SystemExit(f"Real Codex MCP failed during {phase}: {error}") from None
    except Exception:
        raise SystemExit(f"Real Codex MCP failed during {phase}; credential-bearing diagnostics were not logged.") from None
    finally:
        for stream in streams:
            stream.closed.wait(5)
            stream.thread.join(timeout=1)


if __name__ == "__main__":
    main()
