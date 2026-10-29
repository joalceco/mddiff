#!/usr/bin/env python3
"""Start an isolated loopback-only Keycloak fixture. Never use this in production."""

import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[2]
DIRECTORY = ROOT / "target/identity-spike"
RUNTIME = DIRECTORY / "runtime.json"
NAME = "mddiff-keycloak-spike"
IMAGE = "quay.io/keycloak/keycloak:26.8.1"
BASE = "http://localhost:8848"
REALM = "mddiff-spike"
RESOURCE = "http://127.0.0.1:3850/mcp"


def private_write(path, content):
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "w") as output:
        output.write(content)
    path.chmod(0o600)


def request(url, data=None, token=None, method=None, form=False):
    headers = {}
    if token:
        headers["Authorization"] = "Bearer " + token
    if data is not None:
        headers["Content-Type"] = "application/x-www-form-urlencoded" if form else "application/json"
        data = (urllib.parse.urlencode(data) if form else json.dumps(data)).encode()
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=10) as response:
        raw = response.read()
        return json.loads(raw) if raw else None


def mapper(name, key, value):
    return {
        "name": name, "protocol": "openid-connect", "protocolMapper": "oidc-audience-mapper",
        "config": {key: value, "access.token.claim": "true", "introspection.token.claim": "true", "id.token.claim": "false"},
    }


def public_client(name, redirects=(), password=False, resource_audience=True, lifespan=None):
    mappers = [mapper("introspection-audience", "included.client.audience", "mddiff-resource")]
    if resource_audience:
        mappers.append(mapper("mcp-audience", "included.custom.audience", RESOURCE))
    attributes = {"pkce.code.challenge.method": "S256"}
    if lifespan:
        attributes["access.token.lifespan"] = str(lifespan)
    return {
        "clientId": name, "protocol": "openid-connect", "enabled": True,
        "publicClient": True, "standardFlowEnabled": bool(redirects),
        "directAccessGrantsEnabled": password, "redirectUris": list(redirects),
        "attributes": attributes, "protocolMappers": mappers,
        "defaultClientScopes": ["basic", "profile", "roles"],
        "optionalClientScopes": ["mddiff:compare"], "fullScopeAllowed": True,
    }


def representation(runtime, foreign=False):
    users = []
    for username, user in runtime["users"].items():
        users.append({
            "username": username, "enabled": True,
            "firstName": username.capitalize(), "lastName": "Synthetic",
            "email": username + "@users.invalid", "emailVerified": True,
            "credentials": [{"type": "password", "value": user["password"], "temporary": False}],
            "realmRoles": [] if username == "mallory" else ["mddiff-user"],
        })
    return {
        "realm": REALM + ("-foreign" if foreign else ""), "enabled": True,
        "registrationAllowed": False, "resetPasswordAllowed": False,
        "loginWithEmailAllowed": False, "duplicateEmailsAllowed": False,
        "accessTokenLifespan": 120, "sslRequired": "none",
        "roles": {"realm": [{"name": "mddiff-user"}]}, "users": users,
        "clientScopes": [
            {"name": "basic", "protocol": "openid-connect", "protocolMappers": [{
                "name": "subject", "protocol": "openid-connect", "protocolMapper": "oidc-sub-mapper",
                "config": {"access.token.claim": "true", "introspection.token.claim": "true"},
            }]},
            {"name": "profile", "protocol": "openid-connect", "attributes": {"include.in.token.scope": "true"}},
            {"name": "roles", "protocol": "openid-connect", "protocolMappers": [{
                "name": "realm roles", "protocol": "openid-connect", "protocolMapper": "oidc-usermodel-realm-role-mapper",
                "config": {"multivalued": "true", "claim.name": "realm_access.roles", "jsonType.label": "String",
                           "access.token.claim": "true", "introspection.token.claim": "true", "id.token.claim": "false"},
            }]},
            {"name": "mddiff:compare", "protocol": "openid-connect", "attributes": {"include.in.token.scope": "true"}},
        ],
        "clients": [
            {"clientId": "mddiff-resource", "secret": runtime["resource_client"]["secret"], "publicClient": False,
             "standardFlowEnabled": False, "directAccessGrantsEnabled": False, "enabled": True, "protocol": "openid-connect"},
            public_client("mddiff-web", ["http://127.0.0.1:3850/callback"]),
            public_client("mddiff-codex", ["http://127.0.0.1:43210/callback"]),
            # Password grants exist only on synthetic test clients, never web/Codex clients.
            public_client("mddiff-test", password=True),
            public_client("mddiff-wrong-audience", password=True, resource_audience=False),
            public_client("mddiff-expiry-test", password=True, lifespan=2),
        ],
    }


def admin_token(runtime):
    return request(BASE + "/realms/master/protocol/openid-connect/token", {
        "grant_type": "password", "client_id": "admin-cli", "username": "spike-admin",
        "password": runtime["admin_password"],
    }, form=True)["access_token"]


def main():
    command = sys.argv[1] if len(sys.argv) == 2 else ""
    if command not in ("up", "down"):
        raise SystemExit("Usage: python3 spikes/identity/keycloak.py up|down")
    existing = subprocess.run(["docker", "inspect", NAME], capture_output=True, text=True)
    if existing.returncode == 0:
        container = json.loads(existing.stdout)[0]
        if container["Config"]["Labels"].get("mddiff.identity-spike") != "true":
            raise SystemExit("Container name belongs to another service; refusing to touch it.")
        if command == "up":
            raise SystemExit("Fixture already exists. Use down before a fresh up; this resets synthetic state.")
        subprocess.run(["docker", "rm", "-f", NAME], check=True, stdout=subprocess.DEVNULL)
        print("Synthetic Keycloak fixture removed. Private runtime files remain ignored in target/.")
        return
    if command == "down":
        print("No synthetic fixture to remove.")
        return
    DIRECTORY.mkdir(parents=True, exist_ok=True)
    if DIRECTORY.is_symlink():
        raise SystemExit("Refusing symlink runtime directory.")
    DIRECTORY.chmod(0o700)
    runtime = {
        "issuer": BASE + "/realms/" + REALM, "mcp_url": RESOURCE, "web_url": "http://127.0.0.1:3850",
        "admin_password": secrets.token_urlsafe(32),
        "resource_client": {"client_id": "mddiff-resource", "secret": secrets.token_urlsafe(32)},
        "users": {name: {"username": name, "password": secrets.token_urlsafe(24)}
                  for name in ("alice", "bob", "mallory", *(f"load{n:02}" for n in range(3, 11)))},
    }
    private_write(RUNTIME, json.dumps(runtime, indent=2) + "\n")
    env_file = DIRECTORY / "keycloak.env"
    private_write(env_file, "KC_BOOTSTRAP_ADMIN_USERNAME=spike-admin\nKC_BOOTSTRAP_ADMIN_PASSWORD=" + runtime["admin_password"] + "\n")
    if not (DIRECTORY / "social.env").exists():
        private_write(DIRECTORY / "social.env", "# Reserved for a later social-login test; not consumed by this synthetic fixture.\nGOOGLE_CLIENT_ID=\nGOOGLE_CLIENT_SECRET=\nGITHUB_CLIENT_ID=\nGITHUB_CLIENT_SECRET=\n")
    subprocess.run([
        "docker", "run", "--detach", "--name", NAME, "--label", "mddiff.identity-spike=true",
        "--publish", "127.0.0.1:8848:8080", "--memory", "768m", "--cpus", "1",
        "--env", "JAVA_OPTS_KC_HEAP=-Xms128m -Xmx384m",
        "--env-file", str(env_file), IMAGE, "start-dev", "--hostname=" + BASE,
    ], check=True, stdout=subprocess.DEVNULL)
    deadline = time.monotonic() + 120
    while True:
        try:
            token = admin_token(runtime)
            break
        except (OSError, urllib.error.URLError):
            if time.monotonic() > deadline:
                raise SystemExit("Keycloak did not become ready; inspect the owned fixture logs locally.") from None
            time.sleep(1)
    for foreign in (False, True):
        request(BASE + "/admin/realms", representation(runtime, foreign), token, "POST")
    users = request(BASE + "/admin/realms/" + REALM + "/users", token=token)
    for user in users:
        runtime["users"][user["username"]]["id"] = user["id"]
    private_write(RUNTIME, json.dumps(runtime, indent=2) + "\n")
    print("Keycloak 26.8.1 ready on loopback :8848; alice/bob admitted, mallory denied.")
    print("Private credentials generated in ignored target/identity-spike/runtime.json (mode 0600).")


if __name__ == "__main__":
    main()
