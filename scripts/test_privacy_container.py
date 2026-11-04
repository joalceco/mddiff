#!/usr/bin/env python3
"""Run the privacy CLI as container UID 10001 against private Linux bind mounts."""

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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True)
    args = parser.parse_args()
    privacy.require(os.geteuid() == 0, "Run as the Linux operator user root to verify UID ownership.")
    image = json.loads(subprocess.check_output(["docker", "image", "inspect", args.image]))[0]["Id"]
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
        journal.unlink()
        try:
            docker_apply(image, data, journal)
        except RuntimeError:
            pass
        else:
            raise RuntimeError("Protected data opened without its journal.")
        privacy.require(privacy.private_read(state)["pending"], "Container application acknowledged retained backups.")
    print("Privacy container: UID 10001, root atomic publication, latest journal, read-only directory mount and missing-journal refusal passed.")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        reason = str(error) if type(error) is RuntimeError else type(error).__name__
        raise SystemExit("Privacy container test failed: " + reason + "; no private content or credentials logged.") from None
