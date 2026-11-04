#!/usr/bin/env python3
"""Apply provider decisions with the real binary and replay them over an older private checkpoint."""

import argparse
from contextlib import closing
import json
from pathlib import Path
import select
import shutil
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.request import ProxyHandler, build_opener

import jira_privacy_report as privacy
from test_jira_privacy_report import endpoint

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/release/mddiff"


def check(condition, message):
    if not condition:
        raise RuntimeError(message)


def bootstrap(data):
    process = subprocess.Popen([str(BINARY), "--mode", "hosted", "--data-dir", str(data), "--port", "0"],
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        check(bool(select.select([process.stdout], [], [], 10)[0]), "Isolated bootstrap did not start.")
        announced = process.stdout.readline().strip()
        check(announced.startswith("MDdiff: http://127.0.0.1:"), "Isolated bootstrap did not announce loopback.")
        base = announced.removeprefix("MDdiff: ")
        opener = build_opener(ProxyHandler({}))
        deadline = time.monotonic() + 5
        ready = False
        while time.monotonic() < deadline:
            try:
                response = opener.open(base + "/api/work", timeout=1)
            except HTTPError as error:
                response = error
            except (URLError, TimeoutError):
                continue
            with response:
                ready = response.status == 401
            break
        check(ready, "Isolated bootstrap did not close anonymous work access.")
    finally:
        if process.poll() is None:
            process.send_signal(signal.SIGINT)
        try:
            process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=5)
            raise RuntimeError("Isolated bootstrap did not stop.") from None
        check(process.returncode == 0, f"Isolated bootstrap failed (exit {process.returncode}).")


def apply(data, journal, previous=None):
    with socket.socket() as occupied:
        occupied.bind(("127.0.0.1", 0))
        arguments = ["--mode", "hosted", "--data-dir", str(data), "--port", str(occupied.getsockname()[1])]
        result = subprocess.run([str(BINARY), *arguments, "--jira-privacy-file", str(journal), "--apply-jira-privacy"],
                                capture_output=True, text=True, timeout=15)
        check(result.returncode == 0 and result.stdout.startswith("JIRA_PRIVACY_APPLIED:"),
              "Offline application failed or tried to bind a server.")
        missing = subprocess.run([str(BINARY), *arguments], capture_output=True, text=True, timeout=5)
        check(missing.returncode != 0 and "registro externo" in missing.stderr,
              "A database requiring erasure protection opened without its journal.")
        if previous is not None:
            old = subprocess.run([str(previous), *arguments], capture_output=True, text=True, timeout=5)
            check(old.returncode != 0 and "esquema más reciente" in old.stderr,
                  "The pre-erasure binary did not reject the protected schema.")


def records(data):
    with closing(sqlite3.connect(data / "work.sqlite3")) as db:
        return db.execute("SELECT subject,revision,base_markdown,candidate_markdown,base_source,jira_privacy FROM current_work ORDER BY subject").fetchall()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--previous-binary", type=Path)
    args = parser.parse_args()
    if args.previous_binary is not None:
        args.previous_binary = args.previous_binary.resolve(strict=True)
    now = int(time.time())
    with tempfile.TemporaryDirectory(prefix="mddiff-erasure-") as directory:
        root = Path(directory)
        data = root / "data"
        data.mkdir(mode=0o700)
        state, journal = root / "report.json", root / "journal.json"
        bootstrap(data)
        database = data / "work.sqlite3"
        privacy.stage_erasures(database, state, journal, now, create=True)
        inventory = lambda key: {"accounts": {key: now - 100}, "unclassified": False}
        empty = {"accounts": {}, "unclassified": False}
        with closing(sqlite3.connect(database)) as db, db:
            for subject, base_id in (("alice", "synthetic:mentioned"), ("bob", "synthetic:unaffected")):
                ledger = {"base": inventory(base_id), "candidate": inventory("synthetic:mentioned")}
                db.execute("INSERT INTO current_work(issuer,subject,revision,base_label,base_markdown,candidate_label,candidate_markdown,jira_privacy) VALUES ('synthetic-issuer',?,1,'base.md',?,'proposal.md',?,?)",
                           (subject, "Original " + subject + "\r\n", "Private proposal " + subject, json.dumps(ledger)))
        apply(data, journal)
        checkpoint = root / "restored"
        checkpoint.mkdir(mode=0o700)
        shutil.copy2(database, checkpoint / "work.sqlite3")
        original = records(data)
        body = json.dumps({"accounts": [{"accountId": "synthetic:mentioned", "status": "closed"}]}).encode()
        with endpoint(200, body=body) as calls:
            try:
                privacy.report({"synthetic:mentioned": now - 100}, state, "synthetic-token", now)
            except RuntimeError:
                pass
            check(len(calls) == 1 and privacy.private_read(state)["pending"]["synthetic:mentioned"]["status"] == "closed",
                  "Provider erasure was not saved as pending.")
        staged = privacy.stage_erasures(database, state, journal, now)
        check(staged["generation"] == 1 and staged["pending"] == 1 and records(data) == original,
              "Staging falsely acknowledged or immediately deleted document content.")
        apply(data, journal, args.previous_binary)
        applied = records(data)
        check(applied[0][0:4] == ("alice", staged["revision_floor"], None, None), "The affected original/proposal were not erased.")
        check(applied[1][0:4] == ("bob", staged["revision_floor"], "Original bob\r\n", None), "Candidate-only erasure changed an unrelated original.")
        check(json.loads(applied[0][-1]) == {"base": empty, "candidate": empty}, "Erased content retained its inventory.")
        check(records(checkpoint) == original, "The source checkpoint unexpectedly changed.")
        apply(checkpoint, journal, args.previous_binary)
        check(records(checkpoint) == applied, "Restoring the old checkpoint reintroduced erased content.")
        apply(checkpoint, journal)
        check(records(checkpoint) == applied, "Reapplying the barrier changed revision or content again.")
        check(privacy.private_read(state)["pending"], "Applying a database falsely acknowledged all retained backups.")
    print("Jira erasure: real CLI, external decisions, owner-independent removal, candidate-only retention, stale-checkpoint replay and idempotent restart passed.")
    if args.previous_binary is not None:
        print("Jira erasure: pre-erasure binary rejected SQLite 6.")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        reason = str(error) if type(error) is RuntimeError else type(error).__name__
        raise SystemExit("Jira erasure test failed: " + reason + "; private content and credentials were not logged.") from None
