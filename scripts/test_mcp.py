#!/usr/bin/env python3
"""Exercise MCP discovery, comparison, errors and live viewer updates over HTTP."""

import json
from pathlib import Path
import signal
import subprocess
import tempfile
import urllib.error
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


def main():
    binary = Path(__file__).resolve().parents[1] / "target/release/mddiff"
    if not binary.is_file():
        raise SystemExit("Compila primero: cargo build --release")
    with tempfile.TemporaryDirectory(prefix="mddiff-mcp-") as directory:
        root = Path(directory) / "allowed"
        root.mkdir()
        original = "# Plan\n\nUsa **verde** y `uno` al salir.\n"
        proposed = "# Plan\n\nUsa **azul** y `dos` al salir.\n"
        (root / "base.md").write_bytes(original.encode())
        (root / "proposal.md").write_bytes(proposed.encode())
        (Path(directory) / "outside.md").write_text("Fuera de la raíz\n")
        server = subprocess.Popen(
            [str(binary), "--root", str(root), "--port", "0"],
            stdout=subprocess.PIPE, text=True,
        )
        try:
            first_line = server.stdout.readline().strip()
            assert first_line.startswith("MDdiff: http://127.0.0.1:"), first_line
            url = first_line.removeprefix("MDdiff: ")
            headers = {"Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-11-25"}

            def rpc(method, params=None):
                status, raw = http(url + "/mcp", {
                    "jsonrpc": "2.0", "id": 1, "method": method, "params": params or {},
                }, headers)
                assert status == 200, (status, raw)
                return json.loads(raw)

            initialized = rpc("initialize", {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "mddiff-test", "version": "1"},
            })
            assert initialized["result"]["serverInfo"]["name"] == "mddiff"
            tools = rpc("tools/list")["result"]["tools"]
            assert [tool["name"] for tool in tools] == ["open_comparison"]
            assert set(tools[0]["inputSchema"]["required"]) == {"base_path", "candidate_path"}
            assert tools[0]["inputSchema"]["additionalProperties"] is False
            assert tools[0]["annotations"]["destructiveHint"] is False

            def call(arguments):
                return rpc("tools/call", {"name": "open_comparison", "arguments": arguments})

            def revision(stream):
                for line in stream:
                    if line.startswith(b"data:"):
                        return int(line[5:].strip())
                raise AssertionError("SSE closed before its revision")

            with urllib.request.urlopen(url + "/api/events", timeout=5) as events:
                assert revision(events) == 0
                opened = call({"base_path": "base.md", "candidate_path": "proposal.md"})["result"]
                assert opened["isError"] is False
                summary = opened["structuredContent"]
                assert summary["viewer_url"] == url + "/"
                assert summary["added_blocks"] == summary["removed_blocks"] == 1
                assert revision(events) == summary["revision"] == 1

            for arguments in [
                {"base_path": "missing.md", "candidate_path": "proposal.md"},
                {"base_path": "../outside.md", "candidate_path": "proposal.md"},
            ]:
                assert call(arguments)["result"]["isError"] is True
            for arguments in [
                {"base_path": "base.md"},
                {"base_path": "base.md", "candidate_path": "proposal.md", "extra": True},
            ]:
                failed = call(arguments)
                assert "error" in failed or failed.get("result", {}).get("isError") is True
            state = json.loads(http(url + "/api/state")[1])
            assert state["revision"] == 1
            assert state["comparison"]["rows"][1]["right"]["highlighted_html"].count("diff-change") == 2
            assert http(url + "/mcp", {}, {"Origin": "https://evil.example"})[0] == 403
            assert http(url + "/mcp", {}, {"Host": "evil.example"})[0] == 403
            assert http(url + "/mcp", {"large": "x" * 17000}, headers)[0] == 413
            assert (root / "base.md").read_bytes() == original.encode()
            assert (root / "proposal.md").read_bytes() == proposed.encode()
            print("MCP: inicio, herramientas, comparación, SSE, errores, límites y archivos intactos correctos.")
        finally:
            server.send_signal(signal.SIGINT)
            server.wait(timeout=5)


if __name__ == "__main__":
    main()
