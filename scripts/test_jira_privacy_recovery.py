#!/usr/bin/env python3
"""Exercise recovery preparation using the actual CLI and synthetic checkpoints."""

from contextlib import closing
import copy
import json
from pathlib import Path
import shutil
import sqlite3
import subprocess
import tempfile
import time

import jira_privacy_recovery as recovery
import jira_privacy_report as privacy
from test_jira_erasure import BINARY, bootstrap, check, records


def rejects(callback):
    try:
        callback()
    except (RuntimeError, OSError):
        return
    raise AssertionError("Unsafe recovery was accepted.")


def native_apply(image, data, journal):
    check(image in ("sha256:" + "a" * 64, "sha256:" + "b" * 64), "Unexpected recovery image.")
    result = subprocess.run([str(BINARY), "--mode", "hosted", "--data-dir", str(data),
                             "--jira-privacy-file", str(journal), "--apply-jira-privacy"],
                            capture_output=True, timeout=15)
    check(result.returncode == 0 and result.stdout.startswith(b"JIRA_PRIVACY_APPLIED:"), "Recovery CLI refused staging.")


def main():
    now = int(time.time())
    with tempfile.TemporaryDirectory(prefix="mddiff-privacy-recovery-") as directory:
        root = Path(directory)
        data = root / "data"
        data.mkdir(mode=0o700)
        bootstrap(data)
        current = {"commit": "a" * 40, "image_id": "sha256:" + "a" * 64, "sqlite_schema": 6}
        old = {"commit": "b" * 40, "image_id": "sha256:" + "b" * 64, "sqlite_schema": 4}
        live = {"services": {"mddiff": {"image": current["image_id"],
                "command": ["--mode", "hosted", "--jira-privacy-file", recovery.MOUNT + "/journal.json"],
                "volumes": [{"type": "bind", "source": str(root / "privacy"), "target": recovery.MOUNT, "read_only": True}]},
                "keycloak": {"image": "synthetic-identity"}}}
        saved = {"services": {"mddiff": {"image": old["image_id"]}, "keycloak": {"image": "synthetic-identity"}}}
        stage = root / "staged"
        stage.mkdir(mode=0o700)
        stage_data = stage / "data"
        stage_data.mkdir(mode=0o700)
        shutil.copy2(data / "work.sqlite3", stage_data / "work.sqlite3")
        check(recovery.prepare(root, stage, saved, old, saved, old, native_apply) == (old, saved, False),
              "Unactivated schema-4 recovery changed behavior.")
        rejects(lambda: recovery.prepare(root, stage, saved, old, live, current, native_apply))
        (root / "privacy").mkdir(mode=0o700)
        journal, state = root / "privacy/journal.json", root / "report.json"
        privacy.stage_erasures(data / "work.sqlite3", state, journal, now, create=True)
        native_apply(current["image_id"], data, journal)
        with closing(sqlite3.connect(stage_data / "work.sqlite3")) as db, db:
            db.execute("DROP TABLE privacy_guard")
            db.execute("ALTER TABLE current_work DROP COLUMN jira_privacy")
            db.execute("PRAGMA user_version=4")
            db.execute("INSERT INTO current_work(issuer,subject,revision) VALUES ('synthetic','empty',4)")
        original_live, original_journal = records(data), journal.read_bytes()
        selected, config, protected = recovery.prepare(root, stage, saved, old, live, current, native_apply)
        check(protected and selected["image_id"] == current["image_id"] and selected["sqlite_schema"] == 6,
              "Legacy checkpoint attempted to start a pre-barrier binary.")
        check(config["services"]["mddiff"] == {**live["services"]["mddiff"], "pull_policy": "never"}
              and config["services"]["keycloak"] == saved["services"]["keycloak"], "Recovery lost the current guard or changed identity.")
        check(records(stage_data)[0][0:4] == ("empty", 4, None, None), "Legacy empty work was discarded.")
        check(records(data) == original_live and journal.read_bytes() == original_journal, "Recovery changed live state.")
        invalid = copy.deepcopy(live)
        invalid["services"]["mddiff"]["volumes"][0]["read_only"] = False
        rejects(lambda: recovery.prepare(root, stage, saved, old, invalid, current, native_apply))
        rejects(lambda: recovery.prepare(root, root, saved, old, live, current, native_apply))
        unknown = root / "unknown"
        unknown.mkdir(mode=0o700)
        shutil.copytree(stage_data, unknown / "data")
        with closing(sqlite3.connect(unknown / "data/work.sqlite3")) as db, db:
            db.execute("DROP TABLE privacy_guard")
            db.execute("ALTER TABLE current_work DROP COLUMN jira_privacy")
            db.execute("PRAGMA user_version=4")
            db.execute("UPDATE current_work SET base_label='base.md',base_markdown='Unclassified synthetic work'")
        rejects(lambda: recovery.prepare(root, unknown, saved, old, live, current, native_apply))
        with closing(sqlite3.connect(unknown / "data/work.sqlite3")) as db:
            check(db.execute("SELECT base_markdown FROM current_work").fetchone()[0] == "Unclassified synthetic work",
                  "Failed classification discarded original content.")
        with closing(sqlite3.connect(stage_data / "work.sqlite3")) as db, db:
            ledger = {"base": {"accounts": {"synthetic:closed": now - 1}, "unclassified": False},
                      "candidate": {"accounts": {}, "unclassified": False}}
            db.execute("UPDATE current_work SET base_label='base.md',base_markdown='Private synthetic work',jira_privacy=?",
                       (json.dumps(ledger),))
        report = privacy.state_file(state, now)
        report["pending"]["synthetic:closed"] = {"status": "closed", "received_at": now}
        privacy.write_state(state, report)
        privacy.stage_erasures(data / "work.sqlite3", state, journal, now)
        selected6 = dict(old, sqlite_schema=6)
        selected, config, protected = recovery.prepare(root, stage, saved, selected6, live, current, native_apply)
        check(selected["image_id"] == old["image_id"] and records(stage_data)[0][2:4] == (None, None),
              "Compatible rollback skipped the latest erasure decision.")
        check(records(data) == original_live, "Preparing a rollback changed live data.")
        check(privacy.private_read(state)["pending"], "Recovery preparation acknowledged all retained copies.")
        checkpoint_dir = root / "operator-checkpoint"
        checkpoint_dir.mkdir(mode=0o700)
        with_checkpoint = copy.deepcopy(live)
        with_checkpoint["services"]["mddiff"]["command"] += ["--operator-checkpoint-dir", "/run/mddiff-checkpoint"]
        with_checkpoint["services"]["mddiff"]["volumes"].append({
            "type": "bind", "source": str(checkpoint_dir), "target": "/run/mddiff-checkpoint", "read_only": False})
        capable = dict(current, checkpoint_protocol=1)
        selected, config, _ = recovery.prepare(root, stage, saved, selected6, with_checkpoint, capable, native_apply)
        check(selected["image_id"] == current["image_id"] and selected["checkpoint_protocol"] == 1
              and config["services"]["mddiff"]["command"] == with_checkpoint["services"]["mddiff"]["command"],
              "An older schema-6 image cannot start the new checkpoint command.")
        selected, _, _ = recovery.prepare(root, stage, saved, dict(selected6, checkpoint_protocol=1),
                                          with_checkpoint, capable, native_apply)
        check(selected["image_id"] == old["image_id"], "A capable schema-6 checkpoint lost code rollback support.")
        rejects(lambda: recovery.prepare(root, stage, saved, selected6, with_checkpoint, current, native_apply))
        for invalid in (True, 2, "1"):
            rejects(lambda: recovery.prepare(root, stage, saved, dict(selected6, checkpoint_protocol=invalid),
                                             with_checkpoint, capable, native_apply))
        readonly_checkpoint = copy.deepcopy(with_checkpoint)
        readonly_checkpoint["services"]["mddiff"]["volumes"][-1]["read_only"] = True
        rejects(lambda: recovery.prepare(root, stage, saved, selected6, readonly_checkpoint, capable, native_apply))
        check(records(data) == original_live, "Checkpoint compatibility checks changed live data.")
        def changed_journal(image, data, journal):
            native_apply(image, data, journal)
            changed = privacy.private_read(journal)
            changed["generation"] += 1
            changed["revision_floor"] += privacy.REVISION_SPAN
            privacy.write_state(journal, changed)
        rejects(lambda: recovery.prepare(root, stage, saved, selected6, live, current, changed_journal))
        check(records(data) == original_live, "A concurrent decision changed live data during preparation.")
    print("Privacy recovery: legacy bridge, compatible rollback, current journal, read-only mount, live-state preservation and classification refusal passed.")


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        reason = str(error) if type(error) is RuntimeError else type(error).__name__
        raise SystemExit("Privacy recovery failed: " + reason + "; no private content or credentials logged.") from None
