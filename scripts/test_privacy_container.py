#!/usr/bin/env python3
"""Check the privacy CLI and live checkpoints as container UID 10001."""

import argparse
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time

from jira_privacy_recovery import MOUNT, docker_apply
import jira_privacy_report as privacy


def live_checkpoint(image, data, journal, root):
    directory = root / "checkpoint"
    directory.mkdir(mode=0o700)
    os.chown(directory, 10001, 10001)
    container = subprocess.check_output([
        "docker", "run", "--detach", "--init", "--network", "none", "--user", "10001:10001", "--read-only",
        "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true", "--pids-limit", "128", "--memory", "512m",
        "--mount", f"type=bind,src={data},dst=/var/lib/mddiff",
        "--mount", f"type=bind,src={journal.parent},dst={MOUNT},readonly",
        "--mount", f"type=bind,src={directory},dst=/run/mddiff-checkpoint",
        image, "--mode", "hosted", "--data-dir", "/var/lib/mddiff", "--jira-privacy-file", MOUNT + "/journal.json",
        "--operator-checkpoint-dir", "/run/mddiff-checkpoint",
    ]).decode().strip()
    try:
        for _ in range(60):
            health = subprocess.run(["docker", "exec", container, "curl", "--noproxy", "*", "--silent", "--output", "/dev/null",
                                     "--write-out", "%{http_code}", "http://127.0.0.1:3847/healthz"], capture_output=True)
            if health.returncode == 0 and health.stdout == b"503":
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("Closed checkpoint server did not become ready for the operator test.")
        pid = json.loads(subprocess.check_output(["docker", "inspect", container]))[0]["State"]["Pid"]
        for acknowledgement in (b"OPERATOR_CHECKPOINT_READY", b"OPERATOR_CHECKPOINT_FAILED"):
            subprocess.run(["docker", "kill", "--signal", "USR1", container], check=True, stdout=subprocess.DEVNULL)
            for _ in range(60):
                logs = subprocess.check_output(["docker", "logs", container], stderr=subprocess.STDOUT)
                if acknowledgement in logs:
                    break
                time.sleep(0.1)
            else:
                raise RuntimeError("Checkpoint signal was not acknowledged in the container.")
            checkpoint = directory / "work.sqlite3"
            info = checkpoint.stat()
            privacy.require(info.st_uid == 10001 and info.st_mode & 0o777 == 0o600 and info.st_nlink == 1,
                            "Checkpoint lost service ownership or private file permissions.")
            with closing(sqlite3.connect(checkpoint.resolve().as_uri() + "?mode=ro&immutable=1", uri=True)) as db:
                privacy.require(db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
                                and db.execute("SELECT generation FROM privacy_guard").fetchone() == (1,)
                                and db.execute("SELECT base_markdown,candidate_markdown FROM current_work").fetchone() == (None, None),
                                "Checkpoint lost its applied privacy barrier or restored erased work.")
            if acknowledgement.endswith(b"READY"):
                previous = checkpoint.read_bytes()
            else:
                privacy.require(checkpoint.read_bytes() == previous, "A repeated signal replaced the previous checkpoint.")
            state = json.loads(subprocess.check_output(["docker", "inspect", container]))[0]["State"]
            privacy.require(state["Running"] and state["Pid"] == pid, "Checkpoint restarted the reader.")
    finally:
        subprocess.run(["docker", "rm", "--force", container], check=True, stdout=subprocess.DEVNULL)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True)
    args = parser.parse_args()
    privacy.require(os.geteuid() == 0, "Run as the Linux operator user root to verify UID ownership.")
    image = json.loads(subprocess.check_output(["docker", "image", "inspect", args.image]))[0]["Id"]
    labels = json.loads(subprocess.check_output(["docker", "image", "inspect", image]))[0]["Config"].get("Labels") or {}
    privacy.require(labels.get("io.mddiff.operator-checkpoint") == "1", "Image did not advertise its checkpoint protocol.")
    now = int(time.time())
    with tempfile.TemporaryDirectory(prefix="mddiff-container-privacy-") as temporary:
        root = Path(temporary)
        data, directory = root / "data", root / "privacy"
        for path in (data, directory):
            path.mkdir(mode=0o700)
            os.chown(path, 10001, 10001)
        journal, state = directory / "journal.json", root / "report.json"
        privacy.write_state(journal, {"version": 1, "generation": 0, "revision_floor": now * 1_000_000, "accounts": {}},
                            owner=(10001, 10001))
        docker_apply(image, data, journal)
        database = data / "work.sqlite3"
        privacy.require(database.stat().st_uid == 10001, "Container database ownership differs from the service UID.")
        with closing(sqlite3.connect(database)) as db, db:
            ledger = {"base": {"accounts": {"synthetic:closed": now - 1}, "unclassified": False},
                      "candidate": {"accounts": {}, "unclassified": False}}
            db.execute("INSERT INTO current_work(issuer,subject,revision,base_label,base_markdown,jira_privacy) VALUES ('synthetic','owner',1,'base.md','Synthetic private work',?)",
                       (json.dumps(ledger),))
        report = privacy.state_file(state, now)
        report["pending"]["synthetic:closed"] = {"status": "closed", "received_at": now}
        privacy.write_state(state, report)
        inode = journal.stat().st_ino
        privacy.stage_erasures(database, state, journal, now)
        privacy.require(journal.stat().st_ino != inode and journal.stat().st_uid == 10001
                        and journal.stat().st_gid == 10001 and journal.stat().st_mode & 0o777 == 0o600,
                        "Atomic root publication lost the app UID or private mode.")
        docker_apply(image, data, journal)
        with closing(sqlite3.connect(database)) as db:
            privacy.require(db.execute("SELECT base_markdown,candidate_markdown FROM current_work").fetchone() == (None, None),
                            "Container did not erase the affected document.")
            privacy.require(db.execute("SELECT generation FROM privacy_guard").fetchone() == (1,),
                            "Container read an older journal inode.")
        result = subprocess.run(["docker", "run", "--rm", "--network", "none", "--user", "10001:10001",
                                 "--cap-drop", "ALL", "--mount", f"type=bind,src={directory},dst={MOUNT},readonly",
                                 "--entrypoint", "/bin/sh", image, "-c",
                                 'if printf x > /run/mddiff-privacy/write-probe; then exit 1; fi'],
                                capture_output=True, timeout=15)
        privacy.require(result.returncode == 0 and not (directory / "write-probe").exists(),
                        "The container could write its decision mount.")
        live_checkpoint(image, data, journal, root)
        journal.unlink()
        try:
            docker_apply(image, data, journal)
        except RuntimeError:
            pass
        else:
            raise RuntimeError("Protected data opened without its journal.")
        privacy.require(privacy.private_read(state)["pending"], "Container application acknowledged retained backups.")
    print("Privacy container: UID 10001, atomic journal publication, read-only authority, live checkpoint without restart and missing-journal refusal passed.")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        reason = str(error) if type(error) is RuntimeError else type(error).__name__
        raise SystemExit("Privacy container test failed: " + reason + "; no private content or credentials logged.") from None
