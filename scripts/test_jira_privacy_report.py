#!/usr/bin/env python3
"""Exercise Jira privacy reporting without Atlassian accounts, tokens or production copies."""

from contextlib import closing, contextmanager, redirect_stdout
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import io
from pathlib import Path
import sqlite3
import tempfile
import threading
import unittest
from unittest.mock import patch

import jira_privacy_report as privacy

NOW = 1790000000


@contextmanager
def endpoint(status=204, headers=None, body=b""):
    calls = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            calls.append({"path": self.path, "auth": self.headers.get("Authorization"),
                          "body": json.loads(self.rfile.read(int(self.headers["Content-Length"])))})
            self.send_response(status)
            for key, value in (headers or {}).items():
                self.send_header(key, value)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            calls.append({"unexpected_get": self.path})
            self.send_response(500)
            self.end_headers()

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with patch.object(privacy, "ENDPOINT", f"http://127.0.0.1:{server.server_port}/app/report-accounts/"):
            yield calls
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


class Reporting(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="mddiff-jira-report-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.state = self.root / "report.json"

    def database(self, name, base, candidate=None, legacy=False):
        path = self.root / name
        with closing(sqlite3.connect(path)) as db, db:
            db.executescript("CREATE TABLE current_work (base_label TEXT,candidate_label TEXT,jira_privacy TEXT); PRAGMA user_version=5;")
            item = lambda values: {"accounts": values or {}, "unclassified": False}
            raw = None if legacy else json.dumps({"base": item(base), "candidate": item(candidate)})
            db.execute("INSERT INTO current_work VALUES ('base',?,?)", ("proposal" if candidate is not None else None, raw))
        path.chmod(0o600)
        return path

    def test_inventory_uses_oldest_copy_across_users_sides_and_backups(self):
        live = self.database("live.db", {"synthetic:alice": NOW - 10}, {"synthetic:bob": NOW - 20})
        with closing(sqlite3.connect(live)) as db, db:
            other_owner = {"base": {"accounts": {"synthetic:alice": NOW - 50, "synthetic:charlie": NOW - 200}, "unclassified": False},
                           "candidate": {"accounts": {}, "unclassified": False}}
            db.execute("INSERT INTO current_work VALUES ('another owner',NULL,?)", (json.dumps(other_owner),))
        backup = self.database("backup.db", {"synthetic:alice": NOW - 100})
        accounts, summary = privacy.inventory([live, backup], NOW)
        self.assertEqual(accounts, {"synthetic:alice": NOW - 100, "synthetic:bob": NOW - 20, "synthetic:charlie": NOW - 200})
        self.assertEqual(summary, {"checkpoints": 2, "documents": 4, "accounts": 3, "unclassified": 0})
        legacy = self.database("legacy.db", {}, legacy=True)
        self.assertEqual(privacy.inventory([legacy], NOW)[1]["unclassified"], 1)
        with closing(sqlite3.connect(legacy)) as db, db:
            db.execute("PRAGMA user_version=4")
        with self.assertRaises(RuntimeError):
            privacy.inventory([legacy], NOW)

    def test_private_inputs_and_invalid_times_fail_without_network(self):
        for key, at in [("unknown", NOW - 1), ("bad id", NOW - 1), ("synthetic:alice", NOW + 1), ("synthetic:alice", True)]:
            with self.subTest(key=key, at=at):
                path = self.database("invalid-" + str(len(list(self.root.iterdir()))) + ".db", {key: at})
                with self.assertRaises(RuntimeError):
                    privacy.inventory([path], NOW)
        path = self.database("public.db", {})
        path.chmod(0o644)
        with self.assertRaises(RuntimeError):
            privacy.inventory([path], NOW)

    def test_http_batch_limit_days_and_resume_without_duplicate_reports(self):
        accounts = {f"synthetic:{i:03}": NOW - 100 for i in range(181)}
        with endpoint(headers={"Cycle-Period": "3"}) as calls:
            result = privacy.report(accounts, self.state, "synthetic-token", NOW, clock=lambda: NOW + 20)
            self.assertEqual(result["reported"], 181)
            self.assertEqual([len(call["body"]["accounts"]) for call in calls], [90, 90, 1])
            self.assertTrue(all(call["path"] == "/app/report-accounts/" and call["auth"] == "Bearer synthetic-token" for call in calls))
            self.assertEqual(calls[0]["body"]["accounts"][0]["updatedAt"], "2026-09-21T14:11:40Z")
            self.assertEqual(privacy.report(accounts, self.state, "synthetic-token", NOW + 3 * privacy.DAY)["reported"], 0)
            self.assertEqual(len(calls), 3)
            privacy.report(accounts, self.state, "synthetic-token", NOW + 3 * privacy.DAY + 20)
            self.assertEqual(len(calls), 6)
        self.assertEqual(self.state.stat().st_mode & 0o777, 0o600)
        saved = privacy.private_read(self.state)
        self.assertEqual(saved["accounts"]["synthetic:000"]["cycle"], 3 * privacy.DAY)
        self.assertNotIn("synthetic-token", self.state.read_text())

    def test_429_retry_is_global_and_uses_seconds_without_sleeping(self):
        accounts = {"synthetic:alice": NOW - 1}
        with endpoint(429, {"Retry-After": "31"}) as calls:
            with self.assertRaises(RuntimeError):
                privacy.report(accounts, self.state, "synthetic-token", NOW)
            with self.assertRaises(RuntimeError):
                privacy.report(accounts, self.state, "synthetic-token", NOW + 30)
            self.assertEqual(len(calls), 1)
        with endpoint() as calls:
            privacy.report(accounts, self.state, "synthetic-token", NOW + 31)
            self.assertEqual(len(calls), 1)
        self.assertEqual(privacy.private_read(self.state)["retry_after"], 0)

    def test_actions_survive_restart_and_stop_remaining_batches(self):
        accounts = {f"synthetic:{i:03}": NOW - 100 for i in range(91)}
        body = json.dumps({"accounts": [{"accountId": "synthetic:000", "status": "closed"},
                                        {"accountId": "synthetic:001", "status": "updated"}]}).encode()
        with endpoint(200, body=body) as calls:
            with self.assertRaises(RuntimeError):
                privacy.report(accounts, self.state, "synthetic-token", NOW)
            self.assertEqual(len(calls), 1)
            saved = privacy.private_read(self.state)
            self.assertEqual(saved["pending"]["synthetic:000"], {"status": "closed", "received_at": NOW})
            self.assertEqual(saved["pending"]["synthetic:001"]["status"], "updated")
            with self.assertRaises(RuntimeError):
                privacy.report(accounts, self.state, "synthetic-token", NOW + privacy.DEFAULT_CYCLE)
            self.assertEqual(len(calls), 1)

    def test_uncertain_delivery_and_unexpected_accounts_never_retry_blindly(self):
        accounts = {"synthetic:alice": NOW - 1}
        for status, headers, body in [
                (500, {}, b"private diagnostic"), (200, {}, b"invalid JSON"),
                (200, {}, b"x" * 65537),
                (200, {}, b'{"accounts":[{"accountId":"someone-else","status":"closed"}]}'),
                (204, {"Cycle-Period": "P7D"}, b"")]:
            self.state.unlink(missing_ok=True)
            with endpoint(status, headers, body) as calls:
                with self.assertRaises((RuntimeError, ValueError)):
                    privacy.report(accounts, self.state, "synthetic-token", NOW)
                self.assertTrue(privacy.private_read(self.state)["accounts"]["synthetic:alice"]["inflight"])
                with self.assertRaises(RuntimeError):
                    privacy.report(accounts, self.state, "synthetic-token", NOW + privacy.DEFAULT_CYCLE)
                self.assertEqual(len(calls), 1)

    def test_lock_and_private_state_guards(self):
        lock_path = Path(str(self.state) + ".lock")
        lock_path.touch(mode=0o600)
        with lock_path.open("r+") as lock:
            privacy.fcntl.flock(lock, privacy.fcntl.LOCK_EX | privacy.fcntl.LOCK_NB)
            with self.assertRaises(RuntimeError):
                privacy.report({}, self.state, "synthetic-token", NOW)
        self.state.write_text("{}")
        self.state.chmod(0o644)
        with self.assertRaises(RuntimeError):
            privacy.report({}, self.state, "synthetic-token", NOW)
        self.state.unlink()
        self.state.symlink_to(self.root / "missing")
        with self.assertRaises(OSError):
            privacy.private_read(self.state)

    def test_empty_inventory_does_not_send_a_placeholder_report(self):
        with endpoint() as calls:
            self.assertEqual(privacy.report({}, self.state, "synthetic-token", NOW)["reported"], 0)
            self.assertEqual(calls, [])

    def test_redirects_are_not_followed_and_diagnostics_are_not_persisted(self):
        with endpoint(302, {"Location": "/never-follow"}, b"private diagnostic") as calls:
            with self.assertRaises(RuntimeError):
                privacy.report({"synthetic:alice": NOW - 1}, self.state, "synthetic-token", NOW)
            self.assertEqual(len(calls), 1)
        self.assertNotIn("private diagnostic", self.state.read_text())

    def test_partial_success_resumes_only_the_rejected_batch(self):
        accounts = {f"synthetic:{i:03}": NOW - 1 for i in range(91)}
        calls = []

        def send(payload, token):
            calls.append(payload)
            return (204 if len(calls) == 1 else 401), {}, b""

        with self.assertRaises(RuntimeError):
            privacy.report(accounts, self.state, "synthetic-token", NOW, send=send)
        self.assertEqual(len(calls), 2)
        calls.clear()
        privacy.report(accounts, self.state, "new-synthetic-token", NOW + 1, send=send)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["accounts"][0]["accountId"], "synthetic:090")

    def test_timeout_preserves_uncertainty_and_does_not_repeat(self):
        def timeout(payload, token):
            raise TimeoutError("synthetic private transport diagnostic")

        with self.assertRaises(TimeoutError):
            privacy.report({"synthetic:alice": NOW - 1}, self.state, "synthetic-token", NOW, send=timeout)
        with self.assertRaises(RuntimeError):
            privacy.report({"synthetic:alice": NOW - 1}, self.state, "synthetic-token", NOW + 1, send=timeout)
        self.assertNotIn("synthetic private transport diagnostic", self.state.read_text())

    def test_header_units_and_fractional_days_are_explicit(self):
        self.assertEqual(privacy.seconds_header({"Cycle-Period": "0.5"}, "Cycle-Period", privacy.DEFAULT_CYCLE, privacy.DAY), 43200)
        self.assertEqual(privacy.seconds_header({"Retry-After": "0"}, "Retry-After", 300), 0)
        self.assertEqual(privacy.seconds_header({"Cycle-Period": "0.00000001"}, "Cycle-Period", privacy.DEFAULT_CYCLE, privacy.DAY), 1)
        for value in ("-1", "NaN", "Infinity", "0", "999999999", "1e4"):
            with self.subTest(value=value), self.assertRaises(RuntimeError):
                privacy.seconds_header({"Cycle-Period": value}, "Cycle-Period", privacy.DEFAULT_CYCLE, privacy.DAY)

    def test_invalid_credentials_do_not_create_a_report_or_touch_the_network(self):
        with endpoint() as calls:
            for token in (None, "", "bad\nheader", "bad\x00header", "sí"):
                with self.subTest(token=token), self.assertRaises(RuntimeError):
                    privacy.report({"synthetic:alice": NOW - 1}, self.state, token, NOW)
            self.assertFalse(self.state.exists())
            self.assertEqual(calls, [])

    def test_cli_keeps_inventory_private_and_refuses_unclassified_reporting(self):
        path = self.database("valid.db", {"synthetic:alice": NOW - 1})
        output = io.StringIO()
        with patch("sys.argv", ["jira_privacy_report", "inventory", "--database", str(path)]), redirect_stdout(output):
            privacy.main()
        summary = json.loads(output.getvalue())
        self.assertEqual(summary["accounts"], 1)
        self.assertNotIn("synthetic:alice", output.getvalue())
        legacy = self.database("unclassified.db", {}, legacy=True)
        with endpoint() as calls, patch("sys.argv", ["jira_privacy_report", "report", "--database", str(legacy), "--state", str(self.state)]):
            with self.assertRaisesRegex(RuntimeError, "Unclassified"):
                privacy.main()
            self.assertEqual(calls, [])
            self.assertFalse(self.state.exists())


if __name__ == "__main__":
    unittest.main()
