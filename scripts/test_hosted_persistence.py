#!/usr/bin/env python3
"""Check one durable hosted work per synthetic identity, including crashes and failed writes."""

from contextlib import closing, contextmanager
import json
from pathlib import Path
import queue
import select
import signal
import socket
import sqlite3
import subprocess
import tempfile
import threading
from urllib.error import HTTPError
from urllib.parse import urlencode, urlsplit
from urllib.request import Request

from test_hosted_auth import Browser, CheckFailed, ROOT, check, fixture, http, load_runtime, web_client


def request(browser, method="GET", body=None, headers=None, path="/api/work", content_type="application/json", raw=None):
    if headers is None:
        headers = {} if method == "GET" else {"Origin": browser.base, "X-CSRF-Token": browser.session()["csrf_token"]}
    payload = raw if raw is not None else (None if body is None else json.dumps(body, ensure_ascii=False).encode())
    headers = dict(headers)
    if payload is not None and content_type is not None:
        headers["Content-Type"] = content_type
    query = Request(browser.base + path, data=payload, method=method, headers=headers)
    try:
        response = browser.opener.open(query, timeout=10)
    except HTTPError as error:
        response = error
    with response:
        return response.status, response.read(16 * 1024 * 1024)


def work(browser):
    status, raw = request(browser)
    check(status == 200, "Admitted identity cannot read its current work.")
    return json.loads(raw)


def verify(browser, expected):
    check(work(browser) == expected, "Current work differs from the last committed document pair.")


def commit(browser, expected_revision, base, candidate, expected_status=200, headers=None, path="/api/work"):
    status, raw = request(browser, "PUT", {"expected_revision": expected_revision, "base": base, "candidate": candidate}, headers, path)
    check(status == expected_status, f"PUT work returned HTTP {status}, expected {expected_status}.")
    if status == 200:
        snapshot = json.loads(raw)
        check(snapshot == json.loads(browser.request("/api/state")[1]), "PUT returned an unpublished snapshot.")
        check(snapshot["revision"] == expected_revision + 1, "PUT did not increment the owner's revision once.")


def delete(browser, expected_revision, expected_status=200):
    status, raw = request(browser, "DELETE", {"expected_revision": expected_revision})
    check(status == expected_status, f"DELETE work returned HTTP {status}, expected {expected_status}.")
    if status == 200:
        snapshot = json.loads(raw)
        check(snapshot == json.loads(browser.request("/api/state")[1]), "DELETE returned an unpublished snapshot.")
        check(snapshot["revision"] == expected_revision + 1, "DELETE reset or reused the owner's revision.")


class Events:
    def __init__(self, browser, expected_revision):
        self.response = browser.opener.open(browser.base + "/api/events", timeout=30)
        self.queue = queue.Queue()
        self.closed = threading.Event()
        check(self.response.status == 200, "Cannot observe the owner's event stream.")
        initial = None
        for _ in range(10):
            line = self.response.readline()
            if line.startswith(b"data:"):
                initial = int(line[5:].strip())
                break
        check(initial == expected_revision, "SSE did not start at the owner's persisted revision.")
        self.thread = threading.Thread(target=self._read, daemon=True)
        self.thread.start()

    def _read(self):
        try:
            for line in self.response:
                if line.startswith(b"data:"):
                    self.queue.put(int(line[5:].strip()))
        except Exception:
            pass
        finally:
            self.response.close()
            self.closed.set()

    def revision(self, expected):
        try:
            observed = self.queue.get(timeout=5)
        except queue.Empty:
            raise CheckFailed("Committed work did not publish its revision through SSE.") from None
        check(observed == expected, "SSE published another owner's or an incorrect revision.")

    def unchanged(self):
        check(not self.closed.is_set(), "Owner's SSE closed during a rejected operation.")
        try:
            self.queue.get(timeout=0.2)
        except queue.Empty:
            return
        raise CheckFailed("Rejected or foreign-owner operation published an unexpected revision.")


@contextmanager
def instance(binary, base, config_path, data_dir):
    streams = []
    process = subprocess.Popen(
        [str(binary), "--mode", "hosted", "--port", str(urlsplit(base).port),
         "--oidc-config", str(config_path), "--data-dir", str(data_dir)],
        cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        check(bool(select.select([process.stdout], [], [], 10)[0]), "Persistent server did not announce its URL.")
        check(process.stdout.readline().strip() == "MDdiff: " + base, "Persistent server did not start at its configured origin.")
        yield process, streams
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
            raise CheckFailed("Persistent server did not stop after SIGINT.") from None
        for stream in streams:
            check(stream.closed.wait(5), "A stopped server left its SSE connection open.")
            stream.thread.join(timeout=1)
        process.stdout.close()
        process.stderr.close()


def login(runtime, base):
    browsers = [Browser(base, runtime["issuer"]) for _ in range(2)]
    for name, browser in zip(("alice", "bob"), browsers):
        browser.login(runtime["users"][name])
        check(browser.session()["user_label"] == name, "Restart or relogin changed the verified browser identity.")
    return browsers


def observe(browser, revision, streams):
    events = Events(browser, revision)
    streams.append(events)
    return events


def rejected(browser, events, method, body, expected, **options):
    before = work(browser)
    snapshot = json.loads(browser.request("/api/state")[1])
    status, _ = request(browser, method, body, **options)
    check(status == expected, f"Rejected {method} returned HTTP {status}, expected {expected}.")
    verify(browser, before)
    check(json.loads(browser.request("/api/state")[1]) == snapshot, "A failed request changed the visible snapshot.")
    events.unchanged()


def inspect_database(path, runtime, expected):
    # The app holds an exclusive SQLite lock; callers inspect only after its process exits.
    check(path.is_file(), "Hosted mode did not create the promised SQLite database.")
    with closing(sqlite3.connect(path)) as connection:
        check(connection.execute("PRAGMA user_version").fetchone()[0] == 1, "Unexpected work schema version.")
        tables = connection.execute("SELECT name, sql FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'").fetchall()
        check(len(tables) == 1 and tables[0][0] == "current_work", "Storage contains an unexpected document/history table.")
        check("WITHOUT ROWID" in tables[0][1].upper(), "Work table does not use the intended compound primary key.")
        rows = connection.execute("SELECT issuer, subject, revision, base_label, base_markdown, candidate_label, candidate_markdown FROM current_work").fetchall()
        check(len(rows) == len(expected), "Replacing or deleting work created extra rows or lost another owner.")
        records = {(row[0], row[1]): row[2:] for row in rows}
        for name, item in expected.items():
            base, candidate = item["base"], item["candidate"]
            wanted = (item["revision"], base["label"] if base else None, base["markdown"] if base else None,
                      candidate["label"] if candidate else None, candidate["markdown"] if candidate else None)
            check(records.get((runtime["issuer"], runtime["users"][name]["id"])) == wanted, "SQLite does not contain only the owner's latest committed work.")


def main():
    phase = "setup"
    try:
        runtime = load_runtime(fixture.RUNTIME)
        check(runtime["issuer"] == "http://localhost:8848/realms/mddiff-spike", "This runner only permits the isolated Keycloak fixture.")
        binary = ROOT / "target/release/mddiff"
        check(binary.is_file(), "Compile the main release binary first.")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        base = f"http://127.0.0.1:{port}"
        config_path = fixture.DIRECTORY / "hosted-persistence.json"
        fixture.private_write(config_path, json.dumps({
            "issuer": runtime["issuer"], "public_origin": base, "client_id": "mddiff-hosted-web",
            "introspection_client_id": "mddiff-resource", "introspection_client_secret": runtime["resource_client"]["secret"],
            "audience": "mddiff-resource", "allow_insecure_loopback": True,
        }) + "\n")
        with tempfile.TemporaryDirectory(prefix="mddiff-persistence-") as directory, web_client(runtime, base):
            data_dir = Path(directory) / "data"
            data_dir.mkdir(mode=0o700)
            database = data_dir / "work.sqlite3"
            witness = Path(directory) / "not-opened.md"
            witness_bytes = b"# SYNTHETIC_WITNESS_MUST_NOT_BE_OPENED_BY_LABEL\n"
            witness.write_bytes(witness_bytes)
            first = {"label": "Árbol original.md", "markdown": "# Árbol 🌳\r\n\r\nTexto con **énfasis** y `uno`.\r\n"}
            proposal = {"label": "Árbol propuesto.md", "markdown": "# Árbol 🌲\r\n\r\nTexto con **énfasis** y `dos`.\r\n"}
            replacement = {"label": "Trabajo actual.md", "markdown": "# Sólo el trabajo actual\r\n\r\nSe conserva Unicode: español, 日本語 y 🌍.\r\n"}
            bob_base = {"label": str(witness), "markdown": "# Documento explícito de Bob\r\n\r\nEl label no abre archivos. 🐈\r\n"}
            empty = {"revision": 0, "base": None, "candidate": None}
            alice_work = {"revision": 2, "base": replacement, "candidate": None}
            bob_work = {"revision": 1, "base": bob_base, "candidate": proposal}

            phase = "per-owner APIs, validation and relogin"
            with instance(binary, base, config_path, data_dir) as (process, streams):
                alice, bob = login(runtime, base)
                verify(alice, empty)
                verify(bob, empty)
                alice_events, bob_events = (observe(browser, 0, streams) for browser in (alice, bob))
                commit(alice, 0, first, proposal)
                alice_events.revision(1)
                bob_events.unchanged()
                verify(alice, {"revision": 1, "base": first, "candidate": proposal})
                verify(bob, empty)
                commit(bob, 0, bob_base, proposal)
                bob_events.revision(1)
                alice_events.unchanged()
                verify(bob, bob_work)

                valid = {"expected_revision": 1, "base": first, "candidate": proposal}
                rejected(alice, alice_events, "PUT", {**valid, "expected_revision": 0}, 409)
                for label in ("", "x" * 256, "bad\nlabel", "bad\x00label"):
                    rejected(alice, alice_events, "PUT", {**valid, "base": {**first, "label": label}}, 400)
                rejected(alice, alice_events, "PUT", {**valid, "base": {"label": "Too large", "markdown": "x" * (2 * 1024 * 1024 + 1)}}, 413)
                rejected(alice, alice_events, "PUT", {**valid, "owner_id": runtime["users"]["bob"]["id"]}, 422)
                rejected(alice, alice_events, "PUT", {**valid, "mode": "local", "root": str(directory)}, 422)
                rejected(alice, alice_events, "PUT", None, 400, raw=b"{")
                rejected(alice, alice_events, "DELETE", {"expected_revision": 1}, 415, content_type="text/plain")
                rejected(alice, alice_events, "DELETE", None, 400, raw=b"{")
                csrf = alice.session()["csrf_token"]
                for headers in ({}, {"Origin": base}, {"X-CSRF-Token": csrf},
                                {"Origin": "https://other.invalid", "X-CSRF-Token": csrf},
                                {"Origin": base, "X-CSRF-Token": bob.session()["csrf_token"]}):
                    rejected(alice, alice_events, "PUT", valid, 403, headers=headers)
                rejected(alice, alice_events, "POST", valid, 405)
                anonymous = Browser(base, runtime["issuer"])
                spoofed = {"X-User-Id": runtime["users"]["alice"]["id"], "Authorization": "Bearer synthetic-invalid"}
                check(request(anonymous, headers=spoofed)[0] == 401, "Untrusted headers gained access to persisted work.")
                status, raw = request(alice, headers=spoofed, path="/api/work?" + urlencode({"owner_id": runtime["users"]["bob"]["id"]}))
                check(status == 200 and json.loads(raw) == work(alice), "An untrusted owner identifier selected another person's work.")
                commit(alice, 1, replacement, None, headers={"Origin": base, "X-CSRF-Token": csrf, "X-User-Id": runtime["users"]["bob"]["id"]})
                alice_events.revision(2)
                bob_events.unchanged()
                verify(alice, alice_work)
                verify(bob, bob_work)
                check(alice.request("/auth/logout", {}, {"Origin": base, "X-CSRF-Token": csrf})[0] == 204, "Could not log out after persisting work.")
                alice = Browser(base, runtime["issuer"])
                alice.login(runtime["users"]["alice"])
                verify(alice, alice_work)
                process.kill()
                process.wait(timeout=5)
                check(process.returncode == -signal.SIGKILL, "The crash check did not use SIGKILL.")
            inspect_database(database, runtime, {"alice": alice_work, "bob": bob_work})
            print("Hosted persistence: private work, exact CRLF/Unicode, validation, events, replacement and relogin passed.")

            # The app is stopped before an external trigger deliberately makes updates fail.
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("CREATE TRIGGER synthetic_write_failure BEFORE UPDATE ON current_work BEGIN SELECT RAISE(ABORT, 'synthetic write failure'); END")
                connection.commit()
            phase = "restart recovery and failed storage rollback"
            with instance(binary, base, config_path, data_dir) as (_, streams):
                alice, bob = login(runtime, base)
                verify(alice, alice_work)
                verify(bob, bob_work)
                events = observe(alice, 2, streams)
                rejected(alice, events, "PUT", {"expected_revision": 2, "base": first, "candidate": proposal}, 503)
                rejected(alice, events, "DELETE", {"expected_revision": 2}, 503)
                verify(bob, bob_work)
            inspect_database(database, runtime, {"alice": alice_work, "bob": bob_work})
            with closing(sqlite3.connect(database)) as connection:
                connection.execute("DROP TRIGGER synthetic_write_failure")
                connection.commit()
            print("Hosted persistence: SIGKILL recovery and SQL failure rollback without state or event publication passed.")

            phase = "deletion and crash-safe tombstone"
            tombstone = {"revision": 3, "base": None, "candidate": None}
            with instance(binary, base, config_path, data_dir) as (process, streams):
                alice, bob = login(runtime, base)
                verify(alice, alice_work)
                alice_events = observe(alice, 2, streams)
                bob_events = observe(bob, 1, streams)
                delete(alice, 2)
                alice_events.revision(3)
                bob_events.unchanged()
                verify(alice, tombstone)
                verify(bob, bob_work)
                rejected(alice, alice_events, "PUT", {"expected_revision": 2, "base": first, "candidate": proposal}, 409)
                rejected(alice, alice_events, "DELETE", {"expected_revision": 2}, 409)
                process.kill()
                process.wait(timeout=5)
                check(process.returncode == -signal.SIGKILL, "The tombstone crash check did not use SIGKILL.")
            inspect_database(database, runtime, {"alice": tombstone, "bob": bob_work})

            phase = "restart after deletion and revision reuse prevention"
            final = {"revision": 4, "base": first, "candidate": proposal}
            with instance(binary, base, config_path, data_dir) as (_, streams):
                alice, bob = login(runtime, base)
                verify(alice, tombstone)
                verify(bob, bob_work)
                events = observe(alice, 3, streams)
                for revision in (0, 2):
                    rejected(alice, events, "PUT", {"expected_revision": revision, "base": first, "candidate": proposal}, 409)
                rejected(alice, events, "DELETE", {"expected_revision": 2}, 409)
                commit(alice, 3, first, proposal)
                events.revision(4)
                verify(alice, final)
                verify(bob, bob_work)
            inspect_database(database, runtime, {"alice": final, "bob": bob_work})
            check(witness.read_bytes() == witness_bytes, "A display label changed its namesake file.")
            print("Hosted persistence: isolated deletion, durable tombstone, stale revisions after restart and one row per owner passed.")
    except CheckFailed as error:
        raise SystemExit(f"Hosted persistence failed during {phase}: {error}") from None
    except Exception:
        # Provider errors can embed credentials or authorization URLs; report only the phase.
        raise SystemExit(f"Hosted persistence failed during {phase}; credential-bearing diagnostics were not logged.") from None


if __name__ == "__main__":
    main()
