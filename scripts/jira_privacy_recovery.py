"""Filter verified recovery staging before operator helpers replace live data."""

from contextlib import closing
import copy
import os
from pathlib import Path
import re
import sqlite3
import stat
import subprocess
import time

from jira_privacy_report import read_journal, require

MOUNT = "/run/mddiff-privacy"


def docker_apply(image, data, journal):
    for path in (data, *data.rglob("*")):
        require(not path.is_symlink() and (path.is_dir() or path.is_file()), "Unexpected recovery data entry.")
        path.chmod(0o700 if path.is_dir() else 0o600)
        os.chown(path, 10001, 10001)
    result = subprocess.run([
        "docker", "run", "--rm", "--network", "none", "--user", "10001:10001", "--read-only",
        "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true", "--pids-limit", "128",
        "--memory", "512m", "--memory-swap", "512m", "--cpus", "1",
        "--mount", f"type=bind,src={data},dst=/var/lib/mddiff",
        "--mount", f"type=bind,src={journal.parent},dst={MOUNT},readonly",
        image, "--mode", "hosted", "--data-dir", "/var/lib/mddiff",
        "--jira-privacy-file", MOUNT + "/journal.json", "--apply-jira-privacy",
    ], capture_output=True, timeout=120)
    require(result.returncode == 0 and result.stdout.startswith(b"JIRA_PRIVACY_APPLIED:"),
            "Offline privacy recovery failed; preserve staging and keep maintenance enabled.")


def prepare(root, stage, restored, manifest, live, current, apply=docker_apply):
    """Call after archive/image/identity verification, under the operator maintenance lock.

    The caller preserves current provider configuration and trusted operator code.
    No live data, journal, identity or source archive is changed here.
    """
    root, stage = Path(root), Path(stage)
    journal = root / "privacy/journal.json"
    protected = current["sqlite_schema"] >= 6 or manifest["sqlite_schema"] >= 6 or journal.exists() or journal.is_symlink()
    if not protected:
        return manifest, restored, False
    require(current["sqlite_schema"] == 6 and manifest["sqlite_schema"] in (3, 4, 5, 6),
            "Protected recovery requires an installed trusted schema-6 release and the current external journal.")
    for record in (current, manifest):
        require("checkpoint_protocol" not in record
                or type(record["checkpoint_protocol"]) is int and record["checkpoint_protocol"] == 1
                and record["sqlite_schema"] == 6,
                "Unsupported operator checkpoint protocol in release metadata.")
    rules = read_journal(journal, int(time.time()))
    app = live["services"]["mddiff"]
    require(app.get("image") == current["image_id"], "The trusted recovery image must be pinned in live configuration.")
    arguments = app.get("command")
    require(isinstance(arguments, list) and all(isinstance(arg, str) for arg in arguments)
            and arguments.count("--jira-privacy-file") == 1
            and arguments[arguments.index("--jira-privacy-file") + 1:][:1] == [MOUNT + "/journal.json"]
            and "--apply-jira-privacy" not in arguments,
            "Live configuration must enforce the external journal before recovery.")
    mounts = [item for item in app.get("volumes", []) if isinstance(item, dict) and item.get("target") == MOUNT]
    require(len(mounts) == 1 and mounts[0].get("type") == "bind" and mounts[0].get("read_only") is True
            and Path(mounts[0].get("source", "")).absolute() == journal.parent.absolute(),
            "Live configuration must mount the current journal directory read-only.")
    checkpoints = "--operator-checkpoint-dir" in arguments
    if checkpoints:
        position = arguments.index("--operator-checkpoint-dir")
        require(current.get("checkpoint_protocol") == 1 and arguments.count("--operator-checkpoint-dir") == 1
                and position + 1 < len(arguments) and Path(arguments[position + 1]).is_absolute(),
                "Live checkpoint configuration requires a verified protocol-1 runtime and directory.")
        destinations = [item for item in app.get("volumes", []) if isinstance(item, dict)
                        and item.get("target") == arguments[position + 1]]
        require(len(destinations) == 1 and destinations[0].get("type") == "bind"
                and destinations[0].get("read_only", False) is False
                and Path(destinations[0].get("source", "")).is_absolute()
                and not Path(destinations[0]["source"]).resolve().is_relative_to((root / "data").resolve())
                and Path(destinations[0]["source"]).resolve() != journal.parent.resolve(),
                "The operator checkpoint needs a separate writable bind mount outside data and journal.")
    data = stage / "data"
    require(data.is_dir() and not data.is_symlink() and not stage.is_symlink()
            and not data.resolve().is_relative_to((root / "data").resolve()),
            "Privacy recovery may only modify a separate staging directory.")
    for path in data.rglob("*"):
        info = path.lstat()
        require(stat.S_ISREG(info.st_mode) or stat.S_ISDIR(info.st_mode), "Unexpected recovery data entry.")
    require((data / "work.sqlite3").is_file() and (data / "work.sqlite3").stat().st_nlink == 1,
            "Recovery requires a separate regular database checkpoint.")
    selected = dict(current if manifest["sqlite_schema"] < 6
                    or checkpoints and manifest.get("checkpoint_protocol") != 1 else manifest)
    require(re.fullmatch(r"sha256:[0-9a-f]{64}", selected["image_id"]) is not None,
            "Recovery requires an immutable compatible image ID.")
    config = copy.deepcopy(restored)
    config["services"]["mddiff"] = copy.deepcopy(app)
    config["services"]["mddiff"].update(image=selected["image_id"], pull_policy="never")
    config["services"]["mddiff"].pop("build", None)
    apply(selected["image_id"], data, journal)
    with closing(sqlite3.connect((data / "work.sqlite3").resolve().as_uri() + "?mode=ro", uri=True)) as db:
        require(db.execute("PRAGMA user_version").fetchone()[0] == 6
                and db.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
                and db.execute("SELECT generation,revision_floor FROM privacy_guard WHERE singleton=1").fetchone()
                == (rules["generation"], rules["revision_floor"]),
                "The staged database did not apply the current privacy barrier.")
    require(read_journal(journal, int(time.time())) == rules,
            "Privacy decisions changed during recovery; keep maintenance and retry with the latest journal.")
    selected.update(sqlite_schema=6, image=selected["image_id"])
    return selected, config, True
