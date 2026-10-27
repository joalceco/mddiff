#!/usr/bin/env python3
"""Check the main hosted API and stateless MCP over verified local HTTPS."""

from http.client import HTTPSConnection
import json
import ssl
import time
from urllib.error import URLError
from urllib.parse import urlsplit
from urllib.request import ProxyHandler, build_opener

from hosted_mcp_fixture import CheckFailed, TlsBrowser, check, hosted_fixture, http, idp, token
from test_hosted_persistence import Events


HEADERS = {"Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-11-25"}


def rpc(env, access, method, params=None, extra=None):
    status, raw, headers = http(env, "/mcp", {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}},
                                access, headers={**HEADERS, **(extra or {})})
    check(status == 200, "Authenticated MCP returned a non-protocol HTTP error.")
    check(headers.get("Mcp-Session-Id") is None, "Hosted MCP created a transport session.")
    return json.loads(raw)


def tool(env, access, name, arguments=None):
    result = rpc(env, access, "tools/call", {"name": name, "arguments": arguments or {}})
    check("result" in result, "MCP rejected the tool invocation at the protocol layer.")
    return result["result"]


def current(env, access):
    result = tool(env, access, "get_current_work")
    check(not result.get("isError"), "MCP cannot read the admitted user's current work.")
    return result["structuredContent"]


def seed(env, browser, markdown):
    csrf = browser.session()["csrf_token"]
    document = {"label": "Original sintético.md", "markdown": markdown}
    status, _, _ = http(env, "/api/work", {"expected_revision": 0, "base": document, "candidate": None}, method="PUT",
                         headers={"Origin": env.base, "X-CSRF-Token": csrf}, opener=browser.opener)
    check(status == 200, "The HTTPS web session could not seed its original.")
    return document


def main():
    phase = "setup"
    streams = []
    try:
        with hosted_fixture() as env:
            phase = "TLS trust and OAuth discovery"
            try:
                build_opener(ProxyHandler({})).open(env.base + "/.well-known/oauth-protected-resource", timeout=5)
                check(False, "An untrusted temporary CA was silently accepted.")
            except URLError as error:
                check(isinstance(error.reason, ssl.SSLCertVerificationError), "TLS failed for a reason other than certificate validation.")
            for alias in ("/.well-known/oauth-protected-resource", "/.well-known/oauth-protected-resource/mcp"):
                status, raw, _ = http(env, alias)
                metadata = json.loads(raw)
                check(status == 200 and metadata["resource"] == env.resource, "Protected-resource metadata has the wrong canonical resource.")
                check(metadata["authorization_servers"] == [env.runtime["issuer"]] and "mddiff:compare" in metadata["scopes_supported"], "Protected-resource metadata omitted its issuer or scope.")
            status, _, headers = http(env, "/mcp", {})
            check(status == 401 and "resource_metadata=" in headers.get("WWW-Authenticate", ""), "Anonymous MCP did not advertise OAuth discovery.")

            alice, bob = token(env), token(env, "bob")
            rpc(env, alice, "initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "mddiff-main-test", "version": "1"}})
            absent = tool(env, alice, "submit_proposal", {"expected_revision": 0, "candidate": {"label": "proposal.md", "markdown": "# No original yet\n"}})
            check(absent.get("isError") and absent["structuredContent"]["status"] == 409 and current(env, alice)["revision"] == 0, "MCP created a proposal without an original.")

            phase = "web-owned originals and stateless MCP"
            browsers = [TlsBrowser(env.base, env.runtime["issuer"], env.trust) for _ in range(2)]
            originals = []
            for name, browser in zip(("alice", "bob"), browsers):
                browser.login(env.runtime["users"][name])
                originals.append(seed(env, browser, f"# Original privado de {name}\r\n\r\nUnicode: 🌳 y 日本語.\r\n"))
                streams.append(Events(browser, 1))
            initialized = rpc(env, alice, "initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "mddiff-main-test", "version": "1"}})
            check(initialized["result"]["serverInfo"]["name"] == "mddiff-hosted", "MCP initialization returned a different server.")
            tools = rpc(env, alice, "tools/list")["result"]["tools"]
            check({item["name"] for item in tools} == {"get_current_work", "submit_proposal"}, "Hosted tools include a path opener or omit a required tool.")
            check(all(not item["inputSchema"].get("additionalProperties", True) for item in tools), "A hosted tool accepts unspecified top-level arguments.")
            proposals = [{"label": "Propuesta sintética.md", "markdown": f"# Propuesta privada de {name}\r\n\r\nUnicode: 🌲.\r\n"} for name in ("alice", "bob")]
            for index, access in enumerate((alice, bob)):
                before = current(env, access)
                check(before["revision"] == 1 and before["base"] == originals[index] and before["candidate"] is None, "MCP read another account's web work.")
                check(before["viewer_url"] == env.base + "/", "MCP viewer URL did not use the public HTTPS origin.")
                result = tool(env, access, "submit_proposal", {"expected_revision": 1, "candidate": proposals[index]})
                check(not result.get("isError") and result["structuredContent"]["revision"] == 2, "MCP could not commit the owner's proposal.")
                streams[index].revision(2)
                streams[1 - index].unchanged()
            for index, access in enumerate((alice, bob)):
                work = current(env, access)
                check(work["base"] == originals[index] and work["candidate"] == proposals[index] and work["revision"] == 2, "Submitting a proposal changed its original or another owner.")
                browser_work = json.loads(http(env, "/api/work", opener=browsers[index].opener)[1])
                check({key: work[key] for key in ("revision", "base", "candidate")} == browser_work, "Web, API and MCP do not share exactly the same work.")
            check(http(env, "/api/work", {"expected_revision": 2, "base": originals[1], "candidate": proposals[1]}, bob, "PUT")[0] == 200, "A bearer client cannot submit a full pair through the API.")
            streams[1].revision(3)
            streams[0].unchanged()

            phase = "CAS, patch and schema boundaries"
            stale = tool(env, alice, "submit_proposal", {"expected_revision": 1, "candidate": proposals[0]})
            check(stale.get("isError") and stale["structuredContent"]["status"] == 409, "A stale tool call overwrote current work.")
            streams[0].unchanged()
            candidate = {"label": "PATCH.md", "markdown": "# Sólo cambia la propuesta\n"}
            status, _, _ = http(env, "/api/work", {"expected_revision": 2, "candidate": candidate}, alice, "PATCH")
            check(status == 200 and current(env, alice)["base"] == originals[0], "PATCH did not preserve the immutable original.")
            streams[0].revision(3)
            check(http(env, "/api/work", {"expected_revision": 2, "candidate": candidate}, alice, "PATCH")[0] == 409, "PATCH ignored its expected revision.")
            check(http(env, "/api/work", {"expected_revision": 3, "candidate": candidate, "base": originals[1]}, alice, "PATCH")[0] == 422, "PATCH allowed original replacement through extra fields.")
            for name, arguments in [
                ("get_current_work", {"owner_id": "bob"}),
                ("submit_proposal", {"expected_revision": 3, "candidate": candidate, "owner_id": "bob"}),
                ("submit_proposal", {"expected_revision": 3, "candidate": {**candidate, "path": "/never-open/private.md"}}),
            ]:
                result = rpc(env, alice, "tools/call", {"name": name, "arguments": arguments})
                check("error" in result or result.get("result", {}).get("isError"), "MCP accepted a host path or client-selected owner.")
            check(current(env, alice)["revision"] == 3 and current(env, bob)["revision"] == 3, "A rejected owner or path argument changed work.")
            check(http(env, "/mcp", {}, alice, headers={**HEADERS, "Mcp-Session-Id": "synthetic"})[0] == 400, "Stateless MCP accepted a client-chosen session.")
            large = tool(env, alice, "submit_proposal", {"expected_revision": 3, "candidate": {"label": "large.md", "markdown": "x" * (2 * 1024 * 1024 + 1)}})
            check(large.get("isError") and large["structuredContent"]["status"] == 413, "MCP accepted an oversized document.")
            check(http(env, "/api/work", {"expected_revision": 3}, alice, "DELETE")[0] == 403, "Compare-only bearer credentials could delete work.")
            check(http(env, "/api/events", access=alice)[0] == 403, "A bearer token entered the browser-only event stream.")
            streams[0].unchanged()

            phase = "credential, issuer, audience and scope rejection"
            for invalid, expected in [(None, 401), ("synthetic-invalid", 401), (token(env, "mallory"), 403),
                                      (token(env, scope="openid profile"), 403), (token(env, client="mddiff-wrong-audience"), 401),
                                      (token(env, foreign=True), 401)]:
                for path, body in [("/mcp", {}), ("/api/work", None)]:
                    code, raw, _ = http(env, path, body, invalid, headers=HEADERS)
                    check(code == expected, "A hosted read admitted invalid or unauthorized bearer credentials.")
                    check(b"Original privado" not in raw and b"Propuesta privada" not in raw, "A denied response leaked work contents.")
            check(http(env, "/mcp", {}, headers=HEADERS, opener=browsers[0].opener)[0] == 401, "MCP used a browser cookie as fallback authentication.")
            check(http(env, "/mcp", {}, alice, headers={**HEADERS, "Cookie": "synthetic=unrelated"})[0] in (400, 401, 403), "MCP accepted ambiguous cookie and bearer credentials.")
            connection = HTTPSConnection("127.0.0.1", urlsplit(env.base).port, context=env.trust, timeout=10)
            connection.putrequest("POST", "/mcp")
            connection.putheader("Authorization", "Bearer " + alice)
            connection.putheader("Authorization", "Bearer " + bob)
            connection.putheader("Content-Type", "application/json")
            connection.putheader("Content-Length", "2")
            connection.endheaders(b"{}")
            response = connection.getresponse()
            check(response.status in (400, 401, 403), "Duplicate Authorization headers were accepted.")
            response.read(); connection.close()
            for method in ("GET", "DELETE"):
                check(http(env, "/mcp", access=alice, method=method, headers=HEADERS)[0] == 405, "Stateless MCP exposed an unsupported transport method.")
            expired = token(env, client="mddiff-expiry-test")
            check(http(env, "/api/work", access=expired)[0] == 200, "Fresh short-lived token was rejected.")
            time.sleep(3)
            check(http(env, "/api/work", access=expired)[0] == 401, "Expired bearer token remained valid.")
            user_url = idp.BASE + "/admin/realms/" + idp.REALM + "/users/" + env.runtime["users"]["alice"]["id"]
            idp.request(user_url + "/logout", token=idp.admin_token(env.runtime), method="POST")
            check(http(env, "/api/work", access=alice)[0] == 401, "Provider logout did not revoke bearer access.")
            check(http(env, "/api/work", access=bob)[0] == 200, "Revoking one user affected another owner's access.")
        print("Hosted MCP/API: verified HTTPS, stateless bearer calls, web-owned work, two accounts, CAS, scope and revocation passed.")
    except CheckFailed as error:
        raise SystemExit(f"Hosted MCP failed during {phase}: {error}") from None
    except Exception:
        raise SystemExit(f"Hosted MCP failed during {phase}; credential-bearing diagnostics were not logged.") from None
    finally:
        for stream in streams:
            stream.closed.wait(5)
            stream.thread.join(timeout=1)


if __name__ == "__main__":
    main()
