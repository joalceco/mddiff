"""Ten synthetic owners; reports aggregate timings without documents or credentials."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import closing, contextmanager
import json
import math
import socket
import sqlite3
import threading
import time
from urllib.parse import urlencode

from container_client import DATA, Scenario
from hosted_mcp_fixture import TlsBrowser, http, token
from test_hosted_auth import check, fixture
from test_hosted_mcp import current, tool
from test_hosted_persistence import Events
from test_memos_source import connect, request, work


@contextmanager
def long_lived_clients(runtime):
    changed = []
    try:
        for name in ("mddiff-test", "mddiff-hosted-web"):
            endpoint = fixture.BASE + "/admin/realms/" + fixture.REALM + "/clients"
            matches = fixture.request(endpoint + "?" + urlencode({"clientId": name}), token=fixture.admin_token(runtime))
            check(len(matches) == 1, "Capacity OAuth client is missing.")
            url = endpoint + "/" + matches[0]["id"]
            original = fixture.request(url, token=fixture.admin_token(runtime))
            changed.append((url, original))
            updated = json.loads(json.dumps(original))
            updated.setdefault("attributes", {})["access.token.lifespan"] = "600"
            fixture.request(url, updated, fixture.admin_token(runtime), "PUT")
        yield
    finally:
        for url, original in reversed(changed):
            fixture.request(url, original, fixture.admin_token(runtime), "PUT")


def summary(samples):
    ordered = sorted(samples)
    check(bool(ordered), "Capacity measurement has no samples.")
    return {"samples": len(ordered), "p50_ms": round(ordered[math.ceil(len(ordered) * .5) - 1], 2),
            "p95_ms": round(ordered[math.ceil(len(ordered) * .95) - 1], 2), "max_ms": round(ordered[-1], 2)}


def markdown(user, size):
    marker = "CAPACITY-OWNER-" + user
    unit = marker + " original " + "x" * 4000 + "\n\n"
    return (unit * (size // len(unit) + 1))[:size]


def close_event(event):
    event.response.fp.raw._sock.shutdown(socket.SHUT_RDWR)
    event.thread.join(timeout=3)
    check(event.closed.is_set(), "Closing an SSE did not release its stream.")


class Capacity(Scenario):
    def __init__(self, runtime, provider, base):
        super().__init__(runtime, provider, base)
        self.users = [name for name in runtime["users"] if name != "mallory"]
        check(len(self.users) == 10 and set(self.users) == set(provider.memos), "Capacity needs ten distinct admitted synthetic owners.")
        self.access, self.csrf = {}, {}
        self.content_markers = {"CAPACITY-OWNER-" + user for user in self.users}
        self.capture_ms = []
        provider.delay_seconds = .05
        self.bootstrap_metrics = {}

    def safe(self, raw):
        super().safe(raw)
        check(all(value.encode() not in raw for value in self.content_markers), "Logs or denied responses exposed document text.")

    def sessions(self, first=False, update=False):
        self.ready()
        if first:
            self.boundaries()
        self.streams = []
        for name in self.users:
            browser = TlsBrowser(self.base, self.runtime["issuer"], self.env.trust)
            browser.login(self.runtime["users"][name])
            self.csrf[name] = browser.session()["csrf_token"]
            self.browsers[name] = browser
            self.access[name] = token(self.env, name); self.private.add(self.access[name])
            if first:
                connect(browser, self.provider, name, 0)
                started = time.perf_counter()
                code, _ = request(browser, self.provider, "/api/sources/memos", "POST", {
                    "destination": "notes", "reference": "shared", "expected_revision": 0})
                self.capture_ms.append((time.perf_counter() - started) * 1000)
                check(code == 200, "A capacity owner could not capture its private original.")
                self.expected[name] = work(browser, self.provider)
            else:
                check(work(browser, self.provider) == self.expected[name], "Ten-user recovery changed an owner's work or provenance.")
                code, _ = request(browser, self.provider, "/api/connections/notes/check", "POST", {"expected_revision": 1})
                check(code == 200 and self.provider.audit[-1]["user"] == name, "Ten-user recovery decrypted another owner's credential.")
            result = current(self.env, self.access[name])
            check(all(result[key] == self.expected[name][key] for key in ("revision", "base", "candidate")), "MCP read a different owner's work.")
            self.streams.append(Events(browser, self.expected[name]["revision"]))
        self.bootstrap_metrics = {"memos_capture_end_to_end": summary(self.capture_ms), "synthetic_provider_delay_per_get_ms": 50}

    def load(self, profile):
        check(profile in {"small", "near-limit"}, "Unknown capacity profile.")
        size, rounds = (4096, 3) if profile == "small" else (2 * 1024 * 1024, 1)
        barrier = threading.Barrier(10, timeout=15)

        def run(index, name):
            metrics = {key: [] for key in ("api_put", "mcp_proposal", "api_read", "api_state", "rust_processing")}
            original = {"label": "synthetic.md", "markdown": markdown(name, size)}
            proposed = {"label": "proposal.md", "markdown": original["markdown"].replace("original", "proposal", 1)}
            browser, access = self.browsers[name], self.access[name]
            headers = {"Origin": self.base, "X-CSRF-Token": self.csrf[name]}
            barrier.wait()
            for _ in range(rounds):
                before = self.expected[name]["revision"]
                start = time.perf_counter()
                status, raw, _ = http(self.env, "/api/work", {"expected_revision": before, "base": original, "candidate": None},
                                      method="PUT", headers=headers, opener=browser.opener)
                metrics["api_put"].append((time.perf_counter() - start) * 1000)
                check(status == 200, "A ten-user write failed inside its declared quotas.")
                snapshot = json.loads(raw); metrics["rust_processing"].append(snapshot["processing_ms"])
                self.streams[index].revision(before + 1)
                start = time.perf_counter()
                result = tool(self.env, access, "submit_proposal", {"expected_revision": before + 1, "candidate": proposed})
                metrics["mcp_proposal"].append((time.perf_counter() - start) * 1000)
                check(not result.get("isError") and result["structuredContent"]["revision"] == before + 2, "A ten-user MCP proposal failed.")
                metrics["rust_processing"].append(result["structuredContent"]["processing_ms"])
                self.streams[index].revision(before + 2)
                start = time.perf_counter()
                after = work(browser, self.provider)
                metrics["api_read"].append((time.perf_counter() - start) * 1000)
                check(after["revision"] == before + 2 and after["base"] == original and after["candidate"] == proposed,
                      "Concurrent owners mixed or lost Markdown.")
                self.expected[name] = after
                start = time.perf_counter()
                status, raw, _ = http(self.env, "/api/state", opener=browser.opener)
                metrics["api_state"].append((time.perf_counter() - start) * 1000)
                check(status == 200 and json.loads(raw)["revision"] == before + 2
                      and all(("CAPACITY-OWNER-" + other).encode() not in raw for other in self.users if other != name),
                      "The viewer refresh leaked another owner or returned an incorrect revision.")
            return metrics

        start = time.perf_counter()
        with ThreadPoolExecutor(max_workers=10) as pool:
            results = list(pool.map(lambda pair: run(*pair), enumerate(self.users)))
        aggregate = {key: summary([sample for result in results for sample in result[key]]) for key in results[0]}
        threshold = 3000 if profile == "small" else 15000
        return {"profile": profile, "bytes_per_document": size, "rounds_per_user": rounds,
                "elapsed_ms": round((time.perf_counter() - start) * 1000, 2), "p95_threshold_ms": threshold,
                "timings": aggregate, "successful_writes": 10 * rounds * 2, "unexpected_failures": 0,
                "latency_threshold_passed": all(aggregate[key]["p95_ms"] <= threshold for key in ("api_put", "mcp_proposal", "api_read", "api_state"))}

    def limits(self):
        self.stage = "resident workspaces"
        name, other = self.users[:2]
        browser = self.browsers[name]
        before = self.expected[name]
        realm = fixture.BASE + "/admin/realms/" + fixture.REALM
        owner_url = realm + "/users/" + self.runtime["users"]["mallory"]["id"]
        role = fixture.request(realm + "/roles/mddiff-user", token=fixture.admin_token(self.runtime))
        fixture.request(owner_url + "/role-mappings/realm", [role], fixture.admin_token(self.runtime), "POST")
        try:
            overflow = token(self.env, "mallory"); self.private.add(overflow)
            status, raw, headers = http(self.env, "/api/work", access=overflow)
            check(status == 429 and headers.get("Retry-After") == "1", "An eleventh active owner bypassed the resident-workspace budget.")
            self.safe(raw)
            status, _, _ = http(self.env, "/auth/logout", {}, method="POST",
                headers={"Origin": self.base, "X-CSRF-Token": self.csrf[name]}, opener=browser.opener)
            check(status == 204 and self.streams[0].closed.wait(8), "Logout did not close the active workspace's SSE.")
            deadline = time.monotonic() + 3
            while True:
                status, raw, _ = http(self.env, "/api/work", access=overflow)
                if status != 429 or time.monotonic() >= deadline: break
                time.sleep(.05)
            check(status == 200 and json.loads(raw)["base"] is None, "Closing an owner did not make room for another private workspace.")
            browser = TlsBrowser(self.base, self.runtime["issuer"], self.env.trust)
            browser.login(self.runtime["users"][name]); self.csrf[name] = browser.session()["csrf_token"]
            self.browsers[name] = browser
            self.access[name] = token(self.env, name); self.private.add(self.access[name])
            check(work(browser, self.provider) == before, "Evicting an idle workspace lost its persisted work.")
            self.streams[0] = Events(browser, before["revision"])
        finally:
            fixture.request(owner_url + "/role-mappings/realm", [role], fixture.admin_token(self.runtime), "DELETE")
            fixture.request(owner_url + "/logout", token=fixture.admin_token(self.runtime), method="POST")
        def rejected(body, expected, method="PUT", path="/api/work", access=None):
            status, raw, _ = http(self.env, path, body, access=access, method=method,
                                  headers=None if access else {"Origin": self.base, "X-CSRF-Token": self.csrf[name]},
                                  opener=None if access else browser.opener)
            check(status == expected, "A boundary probe returned the wrong rejection status.")
            self.safe(raw)
            check(work(browser, self.provider) == before, "A rejected boundary probe changed work.")
        for body in [
            {"expected_revision": before["revision"], "base": {"label": "too-large.md", "markdown": "x" * (2 * 1024 * 1024 + 1)}},
            {"expected_revision": before["revision"], "base": {"label": "lines.md", "markdown": "x\n" * 10001}},
            {"expected_revision": before["revision"], "base": {"label": "events.md", "markdown": "*a* " * 12000}},
            {"expected_revision": before["revision"], "base": {"label": "depth.md", "markdown": "> " * 129 + "nested"}},
            {"expected_revision": before["revision"], "base": {"label": "html.md", "markdown": "<div>" * 257}},
            {"expected_revision": before["revision"], "base": {"label": "view.md", "markdown": "\x01" * (2 * 1024 * 1024)}},
        ]:
            rejected(body, 413)
        barrier = threading.Barrier(10, timeout=15)
        self.stage = "concurrent JSON expansion"
        def excessive_view(owner):
            browser = self.browsers[owner]
            expected = self.expected[owner]
            barrier.wait()
            status, raw, _ = http(self.env, "/api/work", {"expected_revision": expected["revision"],
                "base": {"label": "expanded.md", "markdown": "\x01" * (2 * 1024 * 1024)}}, method="PUT",
                headers={"Origin": self.base, "X-CSRF-Token": self.csrf[owner]}, opener=browser.opener)
            check(status == 413, "Concurrent JSON expansion did not fail within the rendering budget.")
            self.safe(raw)
            check(work(browser, self.provider) == expected, "Concurrent rejected expansion replaced the saved pair.")
        with ThreadPoolExecutor(max_workers=10) as pool:
            list(pool.map(excessive_view, self.users))
        rejected({"expected_revision": before["revision"], "base": before["base"], "owner_id": other}, 422)
        self.stage = "authorization and provider failures"
        rejected({"expected_revision": before["revision"], "owner_id": other}, 422, "DELETE")
        rejected({"expected_revision": before["revision"]}, 403, "DELETE", access=self.access[name])
        rejected({"base_path": "/never-open/original.md", "candidate_path": "/never-open/proposal.md"}, 404, "POST", "/api/compare")
        failed = tool(self.env, self.access[name], "submit_proposal", {"expected_revision": before["revision"] - 1,
                                                                      "candidate": {"label": "stale.md", "markdown": "stale"}})
        check(failed.get("isError") and failed["structuredContent"]["status"] == 409, "Stale MCP overwrote current work.")
        for mode in ("redirect", "provider-error"):
            self.provider.mode = mode
            rejected({"destination": "notes", "reference": "plain", "expected_revision": before["revision"]},
                     502, "POST", "/api/sources/memos")
        self.provider.mode = "normal"
        self.stage = "SSE and per-owner request quotas"
        extras = [Events(browser, before["revision"]) for _ in range(2)]
        status, raw, headers = http(self.env, "/api/events", opener=browser.opener)
        check(status == 429 and headers.get("Retry-After") == "1", "A fourth owner SSE bypassed its quota.")
        self.safe(raw)
        for event in extras:
            close_event(event)
        for event in self.streams:
            event.unchanged()
        with ThreadPoolExecutor(max_workers=4) as pool, self.provider.pause("plain") as gate:
            body = {"destination": "notes", "reference": "plain", "expected_revision": before["revision"]}
            one = pool.submit(request, browser, self.provider, "/api/sources/memos", "POST", body)
            check(gate.entered.wait(5), "The quota probe did not reach its controlled provider.")
            others = [pool.submit(request, browser, self.provider, "/api/sources/memos", "POST", body) for _ in range(3)]
            time.sleep(.5)
            status, raw, headers = http(self.env, "/api/work", opener=browser.opener)
            check(status == 429 and headers.get("Retry-After") == "1", "A fifth active request bypassed its owner's quota.")
            check(work(self.browsers[other], self.provider) == self.expected[other], "A slow provider blocked another owner's work read.")
            self.safe(raw); gate.release.set()
            check(sorted(future.result(timeout=15)[0] for future in [one, *others]) == [200, 409, 409, 409], "Concurrent capture did not preserve CAS under quotas.")
        self.expected[name] = work(browser, self.provider)
        self.streams[0].revision(before["revision"] + 1)
        for event in self.streams[1:]:
            event.unchanged()
        check(work(self.browsers[other], self.provider) == self.expected[other], "A quota probe changed another owner.")
        # A successful owner-only deletion must preserve all nine other users.
        revision = self.expected[name]["revision"]
        status, _, _ = http(self.env, "/api/work", {"expected_revision": revision}, method="DELETE",
                            headers={"Origin": self.base, "X-CSRF-Token": self.csrf[name]}, opener=browser.opener)
        check(status == 200, "The owner could not delete its own work.")
        self.expected[name] = work(browser, self.provider)
        self.streams[0].revision(revision + 1)
        for index, owner in enumerate(self.users[1:], 1):
            self.streams[index].unchanged()
            check(work(self.browsers[owner], self.provider) == self.expected[owner], "Deletion crossed the owner's boundary.")
        self.provider.assert_read_only()

    def stopped(self):
        for event in self.streams:
            check(event.closed.wait(8), "A capacity SSE survived container shutdown.")
        with closing(sqlite3.connect(DATA / "work.sqlite3")) as database:
            check(database.execute("SELECT count(*) FROM current_work").fetchone()[0] == 10,
                  "The capacity run created history or lost a tombstone.")
            check(database.execute("SELECT count(*) FROM encrypted_connections").fetchone()[0] == 10,
                  "The capacity run lost private credentials.")
        for path in DATA.iterdir():
            super().safe(path.read_bytes())
