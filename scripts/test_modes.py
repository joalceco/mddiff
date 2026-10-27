#!/usr/bin/env python3
"""Check local compatibility and hosted mode boundaries with synthetic documents."""

from contextlib import contextmanager
import json
from pathlib import Path
import select
import signal
import subprocess
import tempfile
import urllib.error
import urllib.parse
import urllib.request


def http(url, body=None, headers=None):
    request = urllib.request.Request(
        url,
        data=None if body is None else json.dumps(body).encode(),
        headers={"Content-Type": "application/json", **(headers or {})},
    )
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return response.status, response.read()
    except urllib.error.HTTPError as error:
        return error.code, error.read()


@contextmanager
def running(binary, root, arguments):
    server = subprocess.Popen(
        [str(binary), "--port", "0", *arguments],
        cwd=root, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        assert select.select([server.stdout], [], [], 5)[0], "Server did not announce its URL."
        first_line = server.stdout.readline().strip()
        assert first_line.startswith("MDdiff: http://127.0.0.1:"), first_line
        url = first_line.removeprefix("MDdiff: ")
        parsed = urllib.parse.urlsplit(url)
        assert parsed.hostname == "127.0.0.1" and parsed.port > 0, url
        yield url
    finally:
        if server.poll() is None:
            server.send_signal(signal.SIGINT)
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait(timeout=5)
            raise AssertionError("Server did not stop after SIGINT.") from None
        server.stdout.close()
        server.stderr.close()


def main():
    binary = Path(__file__).resolve().parents[1] / "target/release/mddiff"
    if not binary.is_file():
        raise SystemExit("Compila primero: cargo build --release")
    with tempfile.TemporaryDirectory(prefix="mddiff-modes-") as directory:
        root = Path(directory)
        source = "# Documento sintético\n\nEl contenido se conserva.\n".encode()
        original = root / "base.md"
        proposal = root / "proposal.md"
        witness = root / "witness.md"
        original.write_bytes(source)
        proposal.write_bytes(source)
        witness_bytes = b"# TESTIGO_SINTETICO_NO_DEBE_SALIR_EN_RESPUESTAS\n"
        witness.write_bytes(witness_bytes)

        for arguments in [
            ["--mode", "invalid"],
            ["--mode", "hosted", "--root", "."],
            ["--mode", "hosted", "--root", str(root / "missing-root")],
            ["--mode", "hosted", "base.md", "proposal.md"],
        ]:
            rejected = subprocess.run(
                [str(binary), "--port", "0", *arguments],
                cwd=root, capture_output=True, text=True, timeout=5,
            )
            assert rejected.returncode != 0, ("CLI accepted forbidden arguments", arguments)
            assert "MDdiff: http://" not in rejected.stdout, "Invalid CLI started a server."

        # The default invocation remains compatible with the explicit local mode.
        for mode in [[], ["--mode", "local"]]:
            with running(binary, root, [*mode, "--root", ".", "base.md", "proposal.md"]) as url:
                status, raw = http(url + "/api/state")
                assert status == 200, ("local state", status)
                state = json.loads(raw)
                assert state["base_path"] == str(original.resolve())
                assert state["candidate_path"] == str(proposal.resolve())
                comparison = state["comparison"]
                assert comparison["source_equal"] is True
                assert comparison["added"] == comparison["removed"] == 0
                for side in ["left", "right"]:
                    text = "".join(row[side]["source"] for row in comparison["rows"] if row[side])
                    assert text.encode() == source, "Local CLI changed the source."

        # The working directory contains Markdown, but hosted mode must not expose its reader.
        with running(binary, root, ["--mode", "hosted"]) as url:
            for path in ["/api/state", "/api/events"]:
                for suffix, headers in [
                    ("", {}),
                    ("", {"X-User-Id": "alice"}),
                    ("", {"Authorization": "Bearer synthetic-invalid-token"}),
                    ("?owner_id=alice&mode=local&root=.", {"X-User-Id": "alice"}),
                ]:
                    status, raw = http(url + path + suffix, headers=headers)
                    assert status == 401, ("hosted identity boundary", path, status)
                    assert witness_bytes.strip() not in raw, "Unauthorized response exposed a file."

            path_requests = [
                ("/api/open", {"path": str(witness)}),
                ("/api/compare", {"base_path": str(original), "candidate_path": str(proposal)}),
                ("/mcp", {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                    "name": "open_comparison", "arguments": {
                        "base_path": str(original), "candidate_path": str(proposal),
                    },
                }}),
            ]
            for path, body in path_requests:
                for extra in [{}, {"mode": "local", "owner_id": "alice", "root": str(root)}]:
                    status, raw = http(url + path, {**body, **extra})
                    assert status == 404, ("hosted path endpoint was registered", path, status)
                    assert witness_bytes.strip() not in raw, "Path endpoint exposed a file."
            assert http(url + "/mcp")[0] == 404, "Hosted registered the local MCP transport."

            for headers in [
                {"Host": "evil.example"},
                {"Origin": "https://evil.example"},
                {"Sec-Fetch-Site": "cross-site"},
            ]:
                assert http(url + "/api/state", headers=headers)[0] == 403
                assert http(url + "/api/open", {"path": str(witness)}, headers)[0] == 403

        assert original.read_bytes() == proposal.read_bytes() == source
        assert witness.read_bytes() == witness_bytes, "The witness file changed."
        print("Modes: local CLI, hosted identity boundary, path endpoints absent, HTTP guards and files intact passed.")


if __name__ == "__main__":
    main()
