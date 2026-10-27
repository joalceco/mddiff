#!/usr/bin/env python3
"""Measure the release server over HTTP using temporary Markdown files."""

import json
from pathlib import Path
import statistics
import subprocess
import tempfile
import time
import urllib.request


def document(minimum_bytes):
    chunks = []
    total = 0
    index = 0
    while total < minimum_bytes:
        chunk = (
            f"## Sección {index}\n\nDocumento de prueba {index}: revisar una propuesta "
            "de la IA con **Markdown**, enlaces y contexto. El contenido del original "
            "se conserva y todos los cambios permanecen visibles.\n\n"
        )
        chunks.append(chunk)
        total += len(chunk.encode())
        index += 1
    return "".join(chunks)


def main():
    binary = Path(__file__).resolve().parents[1] / "target/release/mddiff"
    if not binary.is_file():
        raise SystemExit("Compila primero: cargo build --release")
    with tempfile.TemporaryDirectory(prefix="mddiff-benchmark-") as directory:
        root = Path(directory)
        server = subprocess.Popen(
            [str(binary), "--root", directory, "--port", "0"],
            stdout=subprocess.PIPE,
            text=True,
        )
        try:
            first_line = server.stdout.readline().strip()
            if not first_line.startswith("MDdiff: http://127.0.0.1:"):
                raise RuntimeError(f"El servidor no inició: {first_line}")
            url = first_line.removeprefix("MDdiff: ")
            print("bytes_por_archivo,bloques,mediana_rust_ms,mediana_http_json_ms")
            for minimum_bytes in (100_000, 1_000_000, 2_000_000):
                original = document(minimum_bytes)
                proposed = original.replace("se conserva", "se mantiene", 1)
                (root / "base.md").write_bytes(original.encode())
                (root / "proposal.md").write_bytes(proposed.encode())
                processing = []
                total = []
                for trial in range(6):
                    request = urllib.request.Request(
                        url + "/api/compare",
                        data=json.dumps({
                            "base_path": "base.md", "candidate_path": "proposal.md"
                        }).encode(),
                        headers={"Content-Type": "application/json"},
                    )
                    started = time.perf_counter()
                    with urllib.request.urlopen(request, timeout=30) as response:
                        snapshot = json.load(response)
                    comparison = snapshot["comparison"]
                    assert comparison["added"] == comparison["removed"] == 1
                    if trial:
                        processing.append(snapshot["processing_ms"])
                        total.append((time.perf_counter() - started) * 1000)
                assert (root / "base.md").read_bytes() == original.encode()
                assert (root / "proposal.md").read_bytes() == proposed.encode()
                print(
                    f"{len(original.encode())},{len(comparison['rows'])},"
                    f"{statistics.median(processing):.2f},{statistics.median(total):.2f}",
                    flush=True,
                )
        finally:
            server.terminate()
            server.wait(timeout=5)


if __name__ == "__main__":
    main()
