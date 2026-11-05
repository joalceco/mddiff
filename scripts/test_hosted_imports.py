#!/usr/bin/env python3
"""Run browser imports against a private main-server instance and synthetic Keycloak."""

import json
import os
import socket
import subprocess

from test_hosted_auth import CheckFailed, ROOT, check, fixture, load_runtime, running, web_client


def main():
    try:
        runtime = load_runtime(fixture.RUNTIME)
        check(runtime["issuer"] == "http://localhost:8848/realms/mddiff-spike", "Only the isolated synthetic issuer is allowed.")
        binary = ROOT / "target/release/mddiff"
        check(binary.is_file(), "Compile the main release binary first.")
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        base = f"http://127.0.0.1:{port}"
        config_path = fixture.DIRECTORY / "hosted-imports.json"
        fixture.private_write(config_path, json.dumps({
            "issuer": runtime["issuer"], "public_origin": base, "client_id": "mddiff-hosted-web",
            "introspection_client_id": "mddiff-resource", "introspection_client_secret": runtime["resource_client"]["secret"],
            "audience": "mddiff-resource", "allow_insecure_loopback": True,
        }) + "\n")
        with web_client(runtime, base) as client_url:
            # This owned browser-test client needs enough time for the complete UI suite.
            token = fixture.admin_token(runtime)
            client = fixture.request(client_url, token=token)
            client.setdefault("attributes", {})["access.token.lifespan"] = "600"
            fixture.request(client_url, client, token, "PUT")
            with running(binary, base, config_path):
                environment = {**os.environ, "MDDIFF_HOSTED_URL": base, "MDDIFF_RUNTIME_JSON": str(fixture.RUNTIME)}
                for name in ("DEBUG", "PWDEBUG"):
                    environment.pop(name, None)
                completed = subprocess.run(["node", str(ROOT / "tests/browser/hosted-imports.cjs")], env=environment, cwd=ROOT)
                check(completed.returncode == 0, "Browser imports integration failed.")
        print("Main hosted browser imports passed with synthetic data; Google/GitHub remain deferred.")
    except CheckFailed as error:
        raise SystemExit(str(error)) from None
    except Exception:
        raise SystemExit("Browser imports setup failed; credential-bearing provider details were not logged.") from None


if __name__ == "__main__":
    main()
