#!/usr/bin/env python3
"""Integration checks against the real, synthetic Keycloak and Rust experiment."""

import json
import time
from types import SimpleNamespace
import unittest
import urllib.error
import urllib.parse
import urllib.request

from codex_probe import Forms, load_runtime, synthetic_cookie_jar
import keycloak as fixture

RUNTIME = load_runtime(fixture.RUNTIME)
WEB = RUNTIME["web_url"]
ISSUER = RUNTIME["issuer"]


def token(user="alice", client="mddiff-test", scope="openid profile mddiff:compare", foreign=False):
    return fixture.request(ISSUER + ("-foreign" if foreign else "") + "/protocol/openid-connect/token", {
        "grant_type": "password", "client_id": client, "username": user,
        "password": RUNTIME["users"][user]["password"], "scope": scope,
    }, form=True)


def http(path, access=None, body=None, headers=None, opener=None):
    request = urllib.request.Request(WEB + path, data=None if body is None else json.dumps(body).encode(), headers={
        **({"Authorization": "Bearer " + access} if access else {}),
        **({"Content-Type": "application/json"} if body is not None else {}), **(headers or {}),
    })
    try:
        with (opener.open(request, timeout=10) if opener else urllib.request.urlopen(request, timeout=10)) as response:
            raw = response.read()
            return response.status, json.loads(raw), response.headers
    except urllib.error.HTTPError as error:
        raw = error.read()
        try:
            data = json.loads(raw)
        except json.JSONDecodeError:
            data = {}
        error.close()
        return error.code, data, error.headers


class IdentityExperiment(unittest.TestCase):
    def test_anonymous_and_resource_discovery(self):
        self.assertEqual(http("/api/state")[0], 401)
        status, _, headers = http("/mcp", body={})
        self.assertEqual(status, 401)
        self.assertIn("oauth-protected-resource", headers["WWW-Authenticate"])
        status, metadata, _ = http("/.well-known/oauth-protected-resource/mcp")
        self.assertEqual(status, 200)
        self.assertEqual(metadata["resource"], RUNTIME["mcp_url"])
        self.assertEqual(metadata["authorization_servers"], [ISSUER])

    def test_authentication_and_admission_are_distinct(self):
        admitted = token()["access_token"]
        self.assertEqual(http("/api/state", admitted)[0], 200)
        self.assertEqual(http("/api/state", token("mallory")["access_token"])[0], 403)
        self.assertEqual(http("/api/state", token(scope="openid profile")["access_token"])[0], 403)
        self.assertEqual(http("/api/state", token(client="mddiff-wrong-audience")["access_token"])[0], 401)
        self.assertEqual(http("/api/state", token(foreign=True)["access_token"])[0], 401)
        self.assertEqual(http("/api/state", "invalid-synthetic-token")[0], 401)
        self.assertEqual(http("/mcp", token("mallory")["access_token"], {})[0], 403)

    def test_cookie_exception_is_limited_to_the_synthetic_issuer(self):
        policy = synthetic_cookie_jar()._policy
        cookie = SimpleNamespace(secure=True, domain="localhost.local", name="KC_RESTART")
        self.assertTrue(policy.return_ok_secure(cookie, urllib.request.Request("http://localhost:8848/")))
        for url in ("http://localhost:8849/", "http://127.0.0.1:8848/", "http://other.invalid/"):
            self.assertFalse(policy.return_ok_secure(cookie, urllib.request.Request(url)))
        self.assertTrue(policy.return_ok_secure(cookie, urllib.request.Request("https://other.invalid/")))
        for change in ({"domain":"other.invalid"}, {"name":"unrelated"}):
            changed = SimpleNamespace(**{**vars(cookie), **change})
            self.assertFalse(policy.return_ok_secure(changed, urllib.request.Request("http://localhost:8848/")))

    def test_expired_access_token_is_rejected(self):
        expired = token(client="mddiff-expiry-test")["access_token"]
        self.assertEqual(http("/api/state", expired)[0], 200)
        time.sleep(3)
        self.assertEqual(http("/api/state", expired)[0], 401)

    def test_owner_isolation_conflicts_and_identifier_injection(self):
        alice, bob = (token(name)["access_token"] for name in ("alice", "bob"))
        alice_state = http("/api/state", alice)[1]
        bob_state = http("/api/state", bob)[1]
        body = {"original": "# Synthetic original\n", "proposal": "# Synthetic proposal\n", "expected_revision": alice_state["revision"]}
        status, updated, _ = http("/api/compare", alice, body)
        self.assertEqual(status, 200)
        self.assertEqual(updated["subject"], RUNTIME["users"]["alice"]["id"])
        self.assertEqual(http("/api/state", bob)[1], bob_state)
        self.assertEqual(http("/api/compare", alice, body)[0], 409)
        self.assertEqual(http("/api/compare", alice, {**body, "owner_id": RUNTIME["users"]["bob"]["id"]})[0], 422)
        self.assertEqual(http("/api/state", alice)[1]["revision"], updated["revision"])

    def test_pkce_is_required_by_keycloak(self):
        url = ISSUER + "/protocol/openid-connect/auth?" + urllib.parse.urlencode({
            "client_id": "mddiff-codex", "response_type": "code", "redirect_uri": "http://127.0.0.1:43210/callback",
            "scope": "openid profile mddiff:compare", "state": "synthetic-no-pkce",
        })
        class NoRedirect(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, *args):
                return None
        try:
            urllib.request.build_opener(NoRedirect()).open(url, timeout=10)
            self.fail("Keycloak accepted authorization without PKCE")
        except urllib.error.HTTPError as error:
            location = error.headers.get("Location", "")
            description = urllib.parse.parse_qs(urllib.parse.urlsplit(location).query).get("error_description", [""])[0]
            self.assertIn("code_challenge", description)
            error.close()

    def test_revocation_requires_session_invalidation(self):
        response = token()
        access = response["access_token"]
        self.assertEqual(http("/api/state", access)[0], 200)
        admin = fixture.admin_token(RUNTIME)
        endpoint = fixture.BASE + "/admin/realms/" + fixture.REALM
        user = endpoint + "/users/" + RUNTIME["users"]["alice"]["id"]
        role = fixture.request(endpoint + "/roles/mddiff-user", token=admin)
        fixture.request(user + "/role-mappings/realm", [role], admin, "DELETE")
        try:
            role_only_status = http("/api/state", access)[0]
            self.assertIn(role_only_status, (200, 401, 403))
            fixture.request(user + "/logout", token=admin, method="POST")
            self.assertEqual(http("/api/state", access)[0], 401)
            with self.assertRaises(urllib.error.HTTPError) as rejected:
                fixture.request(ISSUER + "/protocol/openid-connect/token", {
                    "grant_type": "refresh_token", "client_id": "mddiff-test", "refresh_token": response["refresh_token"],
                }, form=True)
            self.assertEqual(rejected.exception.code, 400)
            rejected.exception.close()
            print(json.dumps({"removing_role_only_http_status": role_only_status, "after_admin_logout_http_status": 401, "refresh_rejected": True}))
        finally:
            fixture.request(user + "/role-mappings/realm", [role], fixture.admin_token(RUNTIME), "POST")

    def test_web_uses_verified_oidc_subject_and_browser_state(self):
        cookies = synthetic_cookie_jar()
        opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cookies))
        with opener.open(WEB + "/login", timeout=10) as response:
            page_url = response.url
            forms = [f for f in Forms(response.read().decode()).forms if f["password"]]
        self.assertEqual(len(forms), 1)
        action = urllib.parse.urljoin(page_url, forms[0]["action"])
        self.assertEqual(urllib.parse.urlsplit(action).netloc, "localhost:8848")
        fields = {**forms[0]["fields"], "username": "alice", "password": RUNTIME["users"]["alice"]["password"]}
        with opener.open(urllib.request.Request(action, data=urllib.parse.urlencode(fields).encode()), timeout=10) as response:
            self.assertEqual(response.url, WEB + "/api/state")
            state = json.load(response)
        self.assertEqual(state["issuer"], ISSUER)
        self.assertEqual(state["subject"], RUNTIME["users"]["alice"]["id"])
        self.assertTrue(any(c.name == "mddiff-spike-session" and "HttpOnly" in c._rest for c in cookies))
        self.assertEqual(http("/api/compare", body={"original":"x","proposal":"y"}, opener=opener)[0], 403)
        self.assertEqual(http("/callback?code=unused&state=forged")[0], 400)


if __name__ == "__main__":
    unittest.main(verbosity=2)
