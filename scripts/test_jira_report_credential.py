#!/usr/bin/env python3
"""Exercise rotating operator credentials with synthetic grants and loopback HTTP only."""

from contextlib import redirect_stdout
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import jira_privacy_report as privacy
import test_jira_privacy_report as fixtures

NOW = fixtures.NOW
endpoint = fixtures.endpoint


class Credentials(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="mddiff-jira-credential-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.path = self.root / "credential.json"
        self.credential = {"version": 1, "client_id": "synthetic-client", "client_secret": "synthetic-client-secret",
                           "access_token": "synthetic-old-access", "refresh_token": "synthetic-old-refresh",
                           "expires_at": NOW - 1, "refresh_started_at": None, "retry_after": 0, "reauthorize": False}
        privacy.write_state(self.path, self.credential)
        self.calls = []

    def success(self, payload):
        self.calls.append(payload)
        stored = privacy.private_read(self.path)
        self.assertEqual(stored["refresh_started_at"], NOW)
        return 200, {}, json.dumps({"access_token": "synthetic-new-access", "refresh_token": "synthetic-new-refresh",
                                   "expires_in": 3600}).encode()

    def token(self, now=NOW, send=None, **kwargs):
        return privacy.reporting_token(self.path, now, send=send or self.success, **kwargs)

    def test_rotation_is_saved_before_the_next_consumer_and_reused_after_restart(self):
        self.assertEqual(self.token(), "synthetic-new-access")
        stored = privacy.private_read(self.path)
        self.assertEqual(stored["refresh_token"], "synthetic-new-refresh")
        self.assertEqual(stored["expires_at"], NOW + 3600)
        self.assertIsNone(stored["refresh_started_at"])
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.token(now=NOW + 1), "synthetic-new-access")
        self.assertEqual(len(self.calls), 1)
        self.assertEqual(self.calls[0], {"grant_type": "refresh_token", "client_id": "synthetic-client",
                                       "client_secret": "synthetic-client-secret", "refresh_token": "synthetic-old-refresh"})

    def test_unknown_delivery_only_replays_within_the_original_safe_window(self):
        def interrupted(payload):
            self.calls.append(payload)
            self.assertEqual(privacy.private_read(self.path)["refresh_started_at"], NOW)
            raise TimeoutError("synthetic private provider diagnostic")
        with self.assertRaises(RuntimeError):
            self.token(send=interrupted)
        self.assertEqual(privacy.private_read(self.path)["refresh_started_at"], NOW)
        self.assertNotIn("provider diagnostic", self.path.read_text())
        self.assertEqual(self.token(now=NOW + 1), "synthetic-new-access")
        self.assertEqual(self.calls[1]["refresh_token"], "synthetic-old-refresh")
        self.credential.update(refresh_started_at=NOW, expires_at=NOW + 3600)
        privacy.write_state(self.path, self.credential)
        count = len(self.calls)
        with self.assertRaises(RuntimeError):
            self.token(now=NOW + privacy.REFRESH_REPLAY_WINDOW)
        self.assertEqual(len(self.calls), count)
        self.assertEqual(privacy.private_read(self.path)["refresh_started_at"], NOW)

    def test_explicit_rejection_requires_reauthorization_without_reusing_the_grant(self):
        for status in (400, 401, 403):
            with self.subTest(status=status):
                privacy.write_state(self.path, self.credential)
                with self.assertRaises(RuntimeError):
                    self.token(send=lambda _: (status, {}, b'{"error_description":"private diagnostic"}'))
                stored = privacy.private_read(self.path)
                self.assertTrue(stored["reauthorize"])
                self.assertEqual(stored["refresh_token"], "synthetic-old-refresh")
                with self.assertRaises(RuntimeError):
                    self.token(now=NOW + 1)
                self.assertEqual(self.calls, [])
                self.assertNotIn("private diagnostic", self.path.read_text())

    def test_rate_limit_uses_seconds_and_does_not_spend_the_rotation_window_on_rejection(self):
        with self.assertRaises(RuntimeError):
            self.token(send=lambda _: (429, {"Retry-After": "31"}, b""))
        stored = privacy.private_read(self.path)
        self.assertEqual(stored["retry_after"], NOW + 31)
        self.assertIsNone(stored["refresh_started_at"])
        with self.assertRaises(RuntimeError):
            self.token(now=NOW + 30)
        self.assertEqual(self.calls, [])
        def accepted(payload):
            self.assertEqual(privacy.private_read(self.path)["refresh_started_at"], NOW + 31)
            return 200, {}, b'{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}'
        self.assertEqual(self.token(now=NOW + 31, send=accepted), "new-access")

    def test_uncertain_retries_keep_the_original_deadline_even_after_a_rate_limit(self):
        def unavailable(payload):
            self.calls.append(payload)
            return 503, {}, b"private diagnostic"
        for at in (NOW, NOW + 299):
            with self.assertRaises(RuntimeError):
                self.token(now=at, send=unavailable)
            self.assertEqual(privacy.private_read(self.path)["refresh_started_at"], NOW)
        with self.assertRaises(RuntimeError):
            self.token(now=NOW + 300, send=unavailable)
        self.assertEqual(len(self.calls), 2)
        with self.assertRaises(RuntimeError):
            self.token(now=NOW + 299, send=lambda _: (429, {"Retry-After": "31"}, b""))
        stored = privacy.private_read(self.path)
        self.assertEqual(stored["refresh_started_at"], NOW)
        self.assertEqual(stored["retry_after"], NOW + 330)
        with self.assertRaises(RuntimeError):
            self.token(now=NOW + 330)
        self.assertEqual(len(self.calls), 2)

    def test_invalid_or_missing_rotated_values_preserve_the_attempt(self):
        bodies = [b"not JSON", b"[]", b"{}",
                  b'{"access_token":"new","refresh_token":"new-refresh","expires_in":true}',
                  b'{"access_token":"new","refresh_token":"new-refresh","expires_in":3600,"token_type":null}',
                  b'{"access_token":"new","refresh_token":"new-refresh","expires_in":3600,"token_type":"Basic"}']
        for body in bodies:
            with self.subTest(body=body):
                privacy.write_state(self.path, self.credential)
                with self.assertRaises(RuntimeError):
                    self.token(send=lambda _: (200, {}, body))
                stored = privacy.private_read(self.path)
                self.assertEqual(stored["refresh_started_at"], NOW)
                self.assertEqual(stored["refresh_token"], "synthetic-old-refresh")

    def test_a_new_refresh_token_is_saved_even_if_access_expired_in_transit(self):
        with self.assertRaises(RuntimeError):
            self.token(send=lambda _: (200, {}, b'{"access_token":"short-access","refresh_token":"valid-new-refresh","expires_in":1}'),
                       clock=lambda: NOW + 2)
        stored = privacy.private_read(self.path)
        self.assertEqual(stored["refresh_token"], "valid-new-refresh")
        self.assertIsNone(stored["refresh_started_at"])

    def test_reservation_failure_sends_nothing_and_rotation_failure_keeps_recovery_marker(self):
        original_write = privacy.write_state
        with patch.object(privacy, "write_state", side_effect=OSError("synthetic disk full")):
            with self.assertRaises(OSError):
                self.token()
        self.assertEqual(self.calls, [])
        def fail_new_token(path, value, **kwargs):
            if value["access_token"] == "synthetic-new-access":
                raise OSError("synthetic disk full")
            return original_write(path, value, **kwargs)
        with patch.object(privacy, "write_state", side_effect=fail_new_token):
            with self.assertRaises(OSError):
                self.token()
        self.assertEqual(privacy.private_read(self.path)["refresh_started_at"], NOW)
        self.assertEqual(privacy.private_read(self.path)["refresh_token"], "synthetic-old-refresh")

    def test_private_ownership_repository_boundary_and_single_consumer(self):
        with privacy.reporting_lock(self.path):
            with self.assertRaises(RuntimeError):
                self.token()
        with patch.object(privacy.os, "geteuid", return_value=self.path.stat().st_uid + 1):
            with self.assertRaises(RuntimeError):
                self.token()
        (self.root / ".git").mkdir()
        with self.assertRaises(RuntimeError):
            self.token()
        (self.root / ".git").rmdir()
        self.path.chmod(0o644)
        with self.assertRaises(RuntimeError):
            self.token()
        self.path.chmod(0o600)
        self.assertEqual(self.calls, [])

    def test_legacy_static_files_keep_their_contract_and_cannot_claim_renewal(self):
        privacy.write_state(self.path, {"access_token": "synthetic-static"})
        self.root.chmod(0o755)
        self.assertEqual(self.token(), "synthetic-static")
        with self.assertRaises(RuntimeError):
            self.token(rotating_only=True)
        self.assertEqual(self.calls, [])
        self.root.chmod(0o700)

    def test_http_transport_uses_only_the_auth_endpoint_and_never_follows_redirects(self):
        body = b'{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"token_type":"Bearer"}'
        with endpoint(200, body=body) as calls:
            origin = privacy.ENDPOINT.removesuffix("/app/report-accounts/")
            with patch.object(privacy, "TOKEN_ENDPOINT", origin + "/oauth/token"):
                self.assertEqual(privacy.reporting_token(self.path, NOW), "new-access")
            self.assertEqual(calls[0]["path"], "/oauth/token")
            self.assertIsNone(calls[0]["auth"])
        privacy.write_state(self.path, self.credential)
        with endpoint(302, {"Location": "/never-follow"}, b"private diagnostic") as calls:
            with patch.object(privacy, "TOKEN_ENDPOINT", privacy.ENDPOINT):
                with self.assertRaises(RuntimeError):
                    privacy.reporting_token(self.path, NOW)
            self.assertEqual(len(calls), 1)
        self.assertEqual(privacy.private_read(self.path)["refresh_started_at"], NOW)

    def test_cli_renews_without_a_database_and_does_not_print_credentials(self):
        output = io.StringIO()
        with patch("sys.argv", ["reporter", "renew-credential", "--credential", str(self.path)]), \
                patch.object(privacy.time, "time", return_value=NOW), \
                patch.object(privacy, "post_json", side_effect=lambda url, payload: self.success(payload)), redirect_stdout(output):
            privacy.main()
        self.assertEqual(json.loads(output.getvalue()), {"credential_ready": True})
        self.assertNotIn("synthetic", output.getvalue())

    def test_report_cli_persists_rotation_before_reporting(self):
        database = fixtures.Reporting.database(self, "current.db", {"synthetic:account": NOW - 1})
        state = self.root / "report.json"
        calls = []
        def provider(url, payload, token=None):
            calls.append(url)
            if url == privacy.TOKEN_ENDPOINT:
                return self.success(payload)
            self.assertEqual(url, privacy.ENDPOINT)
            self.assertEqual(token, "synthetic-new-access")
            self.assertEqual(privacy.private_read(self.path)["refresh_token"], "synthetic-new-refresh")
            self.assertIsNone(privacy.private_read(self.path)["refresh_started_at"])
            return 204, {}, b""
        output = io.StringIO()
        with patch("sys.argv", ["reporter", "report", "--database", str(database), "--state", str(state), "--credential", str(self.path)]), \
                patch.object(privacy.time, "time", return_value=NOW), patch.object(privacy, "post_json", side_effect=provider), redirect_stdout(output):
            privacy.main()
        self.assertEqual(calls, [privacy.TOKEN_ENDPOINT, privacy.ENDPOINT])
        self.assertEqual(json.loads(output.getvalue())["reported"], 1)
        self.assertNotIn("synthetic", output.getvalue())


if __name__ == "__main__":
    unittest.main()
