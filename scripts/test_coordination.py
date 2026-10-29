#!/usr/bin/env python3
"""Exercise simultaneous proposals, private SSE catch-up and two real browser tabs."""

from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
from types import SimpleNamespace

from hosted_mcp_fixture import audience_clients, http, token
from test_hosted_auth import CheckFailed, ROOT, check, fixture, load_runtime, web_client
from test_hosted_mcp import current, tool
from test_hosted_persistence import Events, commit, delete, instance, login, request, verify, work


def disconnect(stream):
    stream.response.fp.raw._sock.shutdown(socket.SHUT_RDWR)
    stream.thread.join(timeout=3)
    check(stream.closed.is_set(), "A disconnected SSE did not close its local reader.")


def same_work(remote, browser):
    local = work(browser)
    check({key: remote[key] for key in ("revision", "base", "candidate")}
          == {key: local[key] for key in ("revision", "base", "candidate")},
          "MCP and the owner's browser disagree about current work.")


def binding(browser, revision):
    stream = browser.opener.open(browser.base + "/api/events", timeout=5)
    nonce, initial = None, None
    try:
        for _ in range(10):
            line = stream.readline()
            if line.startswith(b"id:"):
                nonce = line[3:].strip().decode()
            if line.startswith(b"data:"):
                initial = int(line[5:].strip())
            if not line.strip() and initial is not None:
                check(initial == revision, "A new SSE did not start at its owner's current revision.")
                return nonce
        raise CheckFailed("A new SSE omitted its initial revision.")
    finally:
        stream.fp.raw._sock.shutdown(socket.SHUT_RDWR)
        stream.close()


def main():
    phase = "setup"
    try:
        runtime = load_runtime(fixture.RUNTIME)
        check(runtime["issuer"] == "http://localhost:8848/realms/mddiff-spike",
              "Only the isolated synthetic identity fixture is allowed.")
        binary = ROOT / "target/release/mddiff"
        check(binary.is_file(), "Compile the main release binary first.")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            base = f"http://127.0.0.1:{listener.getsockname()[1]}"
        env = SimpleNamespace(base=base, resource=base + "/mcp", trust=None, runtime=runtime)
        with tempfile.TemporaryDirectory(prefix="mddiff-coordination-") as temporary:
            directory = Path(temporary)
            data = directory / "data"
            data.mkdir(mode=0o700)
            config = directory / "oidc.json"
            fixture.private_write(config, json.dumps({
                "issuer": runtime["issuer"], "public_origin": base, "client_id": "mddiff-hosted-web",
                "introspection_client_id": "mddiff-resource",
                "introspection_client_secret": runtime["resource_client"]["secret"],
                "audience": "mddiff-resource", "allow_insecure_loopback": True,
            }))
            with audience_clients(runtime, env.resource), web_client(runtime, base):
                with instance(binary, base, config, data) as (_, streams):
                    alice, bob = login(runtime, base)
                    originals = [{"label": name + ".md", "markdown": "# COORDINATION-PRIVATE-" + name + "\n"}
                                 for name in ("alice", "bob")]
                    for browser, original in zip((alice, bob), originals):
                        commit(browser, 0, original, None)
                    first, second, foreign = Events(alice, 1), Events(alice, 1), Events(bob, 1)
                    streams.extend((first, second, foreign))
                    access = token(env)
                    bob_access = token(env, "bob")
                    phase = "session-bound reads and SSE identity"
                    alice_nonce, bob_nonce = alice.session()["csrf_token"], bob.session()["csrf_token"]
                    check(alice_nonce != bob_nonce, "Independent web sessions share a binding nonce.")
                    status, raw, _ = http(env, "/api/state", opener=bob.opener,
                                          headers={"X-CSRF-Token": alice_nonce})
                    check(status == 403 and b"COORDINATION-PRIVATE" not in raw,
                          "An old tab's binding selected the cookie's different account.")
                    check(http(env, "/api/state", opener=alice.opener,
                               headers={"X-CSRF-Token": alice_nonce})[0] == 200,
                          "A correctly bound owner read was rejected.")
                    check(binding(alice, 1) == alice_nonce and binding(bob, 1) == bob_nonce,
                          "SSE did not bind its events to the admitted browser session.")

                    phase = "simultaneous MCP proposals for one revision"
                    barrier = threading.Barrier(3, timeout=10)
                    proposals = [{"label": f"agent-{index}.md", "markdown": f"# Proposal from agent {index}\n"}
                                 for index in range(2)]

                    def propose(candidate):
                        barrier.wait()
                        return tool(env, access, "submit_proposal", {"expected_revision": 1, "candidate": candidate})

                    with ThreadPoolExecutor(max_workers=2) as pool:
                        pending = [pool.submit(propose, candidate) for candidate in proposals]
                        barrier.wait()
                        results = [future.result(timeout=20) for future in pending]
                    winners = [index for index, result in enumerate(results) if not result.get("isError")]
                    check(len(winners) == 1, "Concurrent agents did not produce exactly one successful commit.")
                    loser = results[1 - winners[0]]
                    check(loser.get("isError") and loser.get("structuredContent", {}).get("status") == 409,
                          "A concurrent stale agent did not receive a revision conflict.")
                    expected_alice, expected_bob = work(alice), work(bob)
                    check(expected_alice["revision"] == 2 and expected_alice["base"] == originals[0]
                          and expected_alice["candidate"] == proposals[winners[0]],
                          "A concurrent proposal changed the original or committed the losing proposal.")
                    check(expected_bob["revision"] == 1 and expected_bob["base"] == originals[1]
                          and expected_bob["candidate"] is None, "A concurrent agent changed another owner.")
                    first.revision(2); second.revision(2); foreign.unchanged()
                    first.unchanged(); second.unchanged()
                    verify(alice, expected_alice); verify(bob, expected_bob)
                    snapshot = json.loads(http(env, "/api/state", opener=alice.opener)[1])
                    check(snapshot.get("candidate_input", {}).get("kind") == "agent", "The viewer did not persist MCP proposal provenance.")
                    same_work(current(env, access), alice); same_work(current(env, bob_access), bob)

                    phase = "same-owner SSE catch-up after a missed revision"
                    disconnect(second)
                    replacement = {"label": "replacement.md", "markdown": "# Current owner proposal\n"}
                    status, _ = request(alice, "PATCH", {"expected_revision": 2, "candidate": replacement})
                    check(status == 200, "The owner could not update while one tab was disconnected.")
                    first.revision(3); foreign.unchanged()
                    recovered = Events(alice, 3)
                    streams.append(recovered)
                    expected_alice = work(alice)
                    check(expected_alice["revision"] == 3 and expected_alice["base"] == originals[0]
                          and expected_alice["candidate"] == replacement, "Catch-up changed the current pair.")
                    verify(alice, expected_alice); verify(bob, expected_bob)
                    same_work(current(env, access), alice)

                    phase = "same-owner deletion on two subscriptions"
                    delete(alice, 3)
                    first.revision(4); recovered.revision(4); foreign.unchanged()
                    expected_alice = {"revision": 4, "base": None, "candidate": None}
                    verify(alice, expected_alice); verify(bob, expected_bob)

                phase = "restart and identity-bound catch-up"
                with instance(binary, base, config, data):
                    alice, bob = login(runtime, base)
                    verify(alice, expected_alice); verify(bob, expected_bob)
                    same_work(current(env, access), alice); same_work(current(env, bob_access), bob)
                    phase = "two real tabs, reconnect, replacement, deletion and logout"
                    environment = {**os.environ, "MDDIFF_HOSTED_URL": base, "MDDIFF_RUNTIME_JSON": str(fixture.RUNTIME)}
                    for name in ("DEBUG", "PWDEBUG"):
                        environment.pop(name, None)
                    completed = subprocess.run(["node", str(ROOT / "tests/browser/coordination.mjs")],
                                               cwd=ROOT, env=environment, timeout=120)
                    check(completed.returncode == 0, "The real two-tab coordination scenario failed.")
                    same_work(current(env, token(env, "bob")), bob)
        print("Coordination: simultaneous MCP CAS, two owner SSE streams, missed-revision catch-up, restart and real two-tab privacy passed.")
    except CheckFailed as error:
        raise SystemExit(f"Coordination failed during {phase}: {error}") from None
    except Exception:
        raise SystemExit(f"Coordination failed during {phase}; credential-bearing diagnostics were not logged.") from None


if __name__ == "__main__":
    main()
