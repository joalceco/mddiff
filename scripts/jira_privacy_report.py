#!/usr/bin/env python3
"""Offline inventory and resumable Jira 3LO reporting; does not attest Sharing or erase copies."""

import argparse
from contextlib import closing
from datetime import datetime, timezone
from decimal import Decimal, ROUND_CEILING
import fcntl
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener

ENDPOINT = "https://api.atlassian.com/app/report-accounts/"
DAY = 86400
DEFAULT_CYCLE = 7 * DAY
ACCOUNT = re.compile(r"[A-Za-z0-9:-]{1,128}\Z")


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def account_id(value):
    return isinstance(value, str) and value != "unknown" and ACCOUNT.fullmatch(value) is not None


def private_read(path, limit=64 * 1024 * 1024):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_mode & 0o077 == 0 and info.st_nlink == 1,
                "Expected a private regular file without links.")
        raw = stream.read(limit + 1)
    require(len(raw) <= limit, "Private file exceeds its size limit.")
    return json.loads(raw)


def private_parent(path):
    parent = Path(path).parent
    info = parent.lstat()
    require(stat.S_ISDIR(info.st_mode) and info.st_mode & 0o077 == 0,
            "State directory must be private, existing and not a symbolic link.")
    return parent


def write_state(path, state):
    parent = private_parent(path)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", dir=parent, prefix=".jira-report-", delete=False) as output:
            temporary = Path(output.name)
            json.dump(state, output, separators=(",", ":"), sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        directory = os.open(parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def inventory(databases, now):
    """Read coherent, stopped-writer checkpoints; aggregate oldest copies across all inputs."""
    accounts, unclassified, documents = {}, 0, 0
    require(databases, "Supply the current checkpoint and every retained backup checkpoint.")
    for path in databases:
        path = Path(path)
        info = path.lstat()
        require(stat.S_ISREG(info.st_mode) and info.st_mode & 0o077 == 0,
                "Inventory requires private regular SQLite checkpoints.")
        with closing(sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True, timeout=1)) as db:
            require(db.execute("PRAGMA user_version").fetchone()[0] == 5,
                    "Inventory requires schema 5; older checkpoints need classification.")
            for base, candidate, raw in db.execute("SELECT base_label IS NOT NULL,candidate_label IS NOT NULL,jira_privacy FROM current_work"):
                if not (base or candidate):
                    continue
                value = json.loads(raw) if raw is not None else None
                require(value is None or isinstance(value, dict) and set(value) == {"base", "candidate"},
                        "Invalid private inventory shape.")
                for name, present in (("base", base), ("candidate", candidate)):
                    if not present:
                        continue
                    documents += 1
                    if value is None:
                        unclassified += 1
                        continue
                    item = value[name]
                    require(isinstance(item, dict) and set(item) == {"accounts", "unclassified"}
                            and isinstance(item["accounts"], dict) and type(item["unclassified"]) is bool,
                            "Invalid document inventory.")
                    unclassified += int(item["unclassified"])
                    for key, retrieved in item["accounts"].items():
                        require(account_id(key) and type(retrieved) is int and 0 < retrieved <= now,
                                "Invalid account identity or retrieval time.")
                        accounts[key] = min(accounts.get(key, retrieved), retrieved)
    return accounts, {"checkpoints": len(databases), "documents": documents, "accounts": len(accounts), "unclassified": unclassified}


def state_file(path, now):
    if not Path(path).exists() and not Path(path).is_symlink():
        return {"version": 1, "accounts": {}, "pending": {}, "retry_after": 0}
    state = private_read(path)
    require(isinstance(state, dict) and set(state) == {"version", "accounts", "pending", "retry_after"}
            and type(state["version"]) is int and state["version"] == 1
            and isinstance(state["accounts"], dict) and isinstance(state["pending"], dict)
            and type(state["retry_after"]) is int and state["retry_after"] >= 0,
            "Invalid reporting state; preserve it and investigate.")
    for key, item in state["accounts"].items():
        require(account_id(key) and isinstance(item, dict)
                and set(item) == {"last_report", "cycle", "inflight"}
                and type(item["last_report"]) is int and 0 <= item["last_report"] <= now
                and type(item["cycle"]) is int and 0 < item["cycle"] <= 365 * DAY
                and type(item["inflight"]) is bool,
                "Invalid per-account reporting state.")
    for key, item in state["pending"].items():
        require(account_id(key) and isinstance(item, dict) and set(item) == {"status", "received_at"}
                and item["status"] in ("closed", "updated")
                and type(item["received_at"]) is int and 0 < item["received_at"] <= now,
                "Invalid pending erasure state.")
    return state


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def post_report(payload, token):
    request = Request(ENDPOINT, data=json.dumps(payload).encode(), method="POST",
                      headers={"Authorization": "Bearer " + token, "Content-Type": "application/json", "Accept": "application/json"})
    opener = build_opener(ProxyHandler({}), NoRedirect())
    try:
        response = opener.open(request, timeout=15)
    except HTTPError as error:
        response = error
    with response:
        raw = response.read(65537)
        require(len(raw) <= 65536, "Reporting response exceeds the limit; delivery is uncertain.")
        return response.status, response.headers, raw


def seconds_header(headers, name, default, multiplier=1):
    value = headers.get(name)
    if value is None:
        return default
    pattern = r"[0-9]{1,9}" if name == "Retry-After" else r"[0-9]{1,9}(?:\.[0-9]{1,8})?"
    require(isinstance(value, str) and re.fullmatch(pattern, value) is not None,
            "Invalid reporting interval; preserve the uncertain request.")
    seconds = Decimal(value) * multiplier
    require((0 <= seconds if name == "Retry-After" else 0 < seconds) and seconds <= 365 * DAY,
            "Invalid reporting interval; preserve the uncertain request.")
    return int(seconds.to_integral_value(rounding=ROUND_CEILING))


def report(accounts, path, token, now, send=post_report, clock=None):
    require(isinstance(token, str) and 0 < len(token) <= 8192
            and all(33 <= ord(c) <= 126 for c in token), "Invalid private reporting credential.")
    clock = clock or (lambda: now)
    private_parent(path)
    lock_path = Path(str(path) + ".lock")
    descriptor = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "r+") as lock:
        info = os.fstat(lock.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_mode & 0o077 == 0 and info.st_nlink == 1,
                "Reporting lock must be a private regular file.")
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise RuntimeError("Another reporter holds the lock.") from None
        state = state_file(path, now)
        require(not any(item["inflight"] for item in state["accounts"].values()),
                "An earlier response is uncertain; reconcile it before sending again.")
        require(not state["pending"], "Erasure/update actions are pending; apply them before reporting again.")
        require(state["retry_after"] <= now, "Atlassian retry delay is still active.")
        due = [key for key in sorted(accounts) if key not in state["accounts"]
               or state["accounts"][key]["last_report"] + state["accounts"][key]["cycle"] <= now]
        sent = 0
        for offset in range(0, len(due), 90):
            batch = due[offset:offset + 90]
            previous = {key: state["accounts"].get(key) for key in batch}
            for key in batch:
                require(account_id(key) and type(accounts[key]) is int and 0 < accounts[key] <= now,
                        "Invalid report input.")
                old = previous[key]
                state["accounts"][key] = {"last_report": old["last_report"] if old else 0,
                                           "cycle": old["cycle"] if old else DEFAULT_CYCLE, "inflight": True}
            write_state(path, state)
            payload = {"accounts": [{"accountId": key, "updatedAt": datetime.fromtimestamp(accounts[key], timezone.utc).isoformat().replace("+00:00", "Z")}
                                     for key in batch]}
            status, headers, raw = send(payload, token)
            received_at = max(now, int(clock()))
            if status in (400, 401, 403, 429):
                for key, old in previous.items():
                    if old is None:
                        del state["accounts"][key]
                    else:
                        state["accounts"][key] = old
                state["retry_after"] = received_at + seconds_header(headers, "Retry-After", 300) if status == 429 else 0
                write_state(path, state)
                raise RuntimeError("Atlassian rejected reporting; check credentials, input or retry delay.")
            require(status in (200, 204), "Reporting failed with uncertain delivery; preserve state and investigate.")
            cycle = seconds_header(headers, "Cycle-Period", DEFAULT_CYCLE, DAY)
            actions = []
            if status == 200:
                body = json.loads(raw)
                require(isinstance(body, dict) and set(body) == {"accounts"} and isinstance(body["accounts"], list),
                        "Malformed reporting response; delivery is uncertain.")
                actions = body["accounts"]
            seen = set()
            for action in actions:
                require(isinstance(action, dict) and set(action) == {"accountId", "status"}
                        and isinstance(action["accountId"], str) and action["accountId"] in batch
                        and action["accountId"] not in seen and action["status"] in ("closed", "updated"),
                        "Unexpected account/action in reporting response; delivery is uncertain.")
                seen.add(action["accountId"])
            for key in batch:
                state["accounts"][key] = {"last_report": received_at, "cycle": cycle, "inflight": False}
            for action in actions:
                state["pending"][action["accountId"]] = {"status": action["status"], "received_at": received_at}
            state["retry_after"] = 0
            write_state(path, state)
            sent += len(batch)
            require(not actions, "Atlassian requested erasure/update; pending actions were saved durably.")
        return {"reported": sent, "deferred": len(accounts) - sent, "pending": len(state["pending"])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("inventory", "report"))
    parser.add_argument("--database", action="append", required=True, type=Path)
    parser.add_argument("--state", type=Path)
    parser.add_argument("--credential", type=Path, help="Private JSON containing only access_token; never pass the token as an argument.")
    args = parser.parse_args()
    now = int(time.time())
    accounts, summary = inventory(args.database, now)
    if args.command == "report":
        require(summary["unclassified"] == 0, "Unclassified documents remain; no report was sent.")
        require(args.state is not None and args.credential is not None, "Reporting requires private state and credential paths.")
        credential = private_read(args.credential, 16384)
        require(isinstance(credential, dict) and set(credential) == {"access_token"}, "Invalid private reporting credential.")
        summary.update(report(accounts, args.state, credential["access_token"], now, clock=time.time))
    print(json.dumps(summary, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        reason = str(error) if type(error) is RuntimeError else type(error).__name__
        raise SystemExit("Jira privacy reporting stopped: " + reason + "; identifiers, documents and credentials were not logged.") from None
