#!/usr/bin/env python3
"""Check the built Linux image with isolated OIDC, verified HTTPS and durable private volumes."""

import argparse
from decimal import Decimal
import json
from pathlib import Path
import queue
import shutil
import subprocess
import tarfile
import tempfile
import threading
import time
import uuid

from hosted_mcp_fixture import certificates
from test_hosted_auth import CheckFailed, ROOT, check, fixture, load_runtime


CLIENT_IMAGE = "python:3.13-slim-bookworm@sha256:281d39ff02e3b389401586134ff9e6572954687822a58055ec07a60ded4e0b7f"
LABEL = "mddiff.container-test"


def docker(*arguments, timeout=30, required=True):
    result = subprocess.run(["docker", *arguments], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
    check(not required or result.returncode == 0, "A Docker operation failed; private diagnostics were suppressed.")
    return result


class Client:
    def __init__(self, command, payload):
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                        text=True, encoding="utf-8", bufsize=1)
        self.messages = queue.Queue()
        threading.Thread(target=self.read, daemon=True).start()
        self.send(payload)

    def read(self):
        for line in self.process.stdout:
            try:
                self.messages.put(json.loads(line))
            except json.JSONDecodeError:
                self.messages.put({"event": "error", "message": "Unexpected client output was suppressed."})
        self.messages.put(None)

    def send(self, message):
        self.process.stdin.write(json.dumps(message) + "\n"); self.process.stdin.flush()

    def receive(self, event, phase=None, timeout=60):
        try:
            message = self.messages.get(timeout=timeout)
        except queue.Empty:
            raise CheckFailed("Timed out waiting for the isolated container client.") from None
        check(message is not None, "The isolated container client exited before responding.")
        if message.get("event") == "error":
            raise CheckFailed("Client phase " + message.get("phase", "bootstrap") + ": " + message.get("message", "Private diagnostics suppressed."))
        check(message.get("event") == event and (phase is None or message.get("phase") == phase), "The isolated container client returned an unexpected protocol event.")
        return message

    def phase(self, phase, timeout=60, **fields):
        self.send({"phase": phase, **fields}); message = self.receive("passed", phase, timeout=timeout)
        print("Container check passed: " + phase, flush=True)
        return message

    def close(self):
        if self.process.poll() is None:
            try:
                self.send({"phase": "quit"}); self.process.stdin.close()
                self.process.wait(timeout=20)
            except (BrokenPipeError, OSError, subprocess.TimeoutExpired):
                self.process.terminate()
                try:
                    self.process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.process.kill(); self.process.wait(timeout=5)


def scan_image(image, identity, secrets):
    info = json.loads(docker("image", "inspect", image).stdout)[0]
    check(info["Config"]["User"] == "10001:10001" and info["Config"]["Entrypoint"] == ["/usr/local/bin/mddiff"], "The runtime image has an unexpected identity or entrypoint.")
    check(bool(info["Config"].get("Healthcheck", {}).get("Test")), "The image does not configure its public health check.")
    check(all(secret.encode() not in json.dumps(info).encode() for secret in secrets), "Image metadata contains a private fixture credential.")
    name = identity + "-image"
    docker("create", "--name", name, "--label", LABEL + "=" + identity, info["Id"])
    process = subprocess.Popen(["docker", "export", name], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    found_binary = False
    patterns = [secret.encode() for secret in secrets]
    overlap = max(map(len, patterns), default=1) - 1
    try:
        with tarfile.open(fileobj=process.stdout, mode="r|") as archive:
            for member in archive:
                if not member.isfile():
                    continue
                path = Path(member.name)
                check(not ({".git", "target", ".codex"} & set(path.parts)) and path.suffix != ".rs"
                      and path.name not in {"Cargo.toml", "Cargo.lock", "oidc.json", "connections.json", "connections.key", "runtime.json"}
                      and not path.name.startswith(".env"), "The runtime image contains source, build output or private configuration.")
                found_binary |= member.name.lstrip("/") == "usr/local/bin/mddiff"
                tail = b""
                stream = archive.extractfile(member)
                while block := stream.read(256 * 1024):
                    window = tail + block
                    check(all(pattern not in window for pattern in patterns), "The runtime image filesystem contains a private credential.")
                    tail = window[-overlap:] if overlap else b""
        check(process.wait(timeout=10) == 0 and found_binary, "The runtime image export failed or omitted the application binary.")
    finally:
        process.stdout.close()
        if process.poll() is None:
            process.kill(); process.wait(timeout=5)
        docker("rm", "-f", name, required=False)
    return info["Id"]


def audit_context():
    with tempfile.TemporaryDirectory(prefix="mddiff-context-audit-") as temporary:
        directory = Path(temporary)
        context, output = directory / "context", directory / "export"
        context.mkdir()
        for filename in ("Cargo.toml", "Cargo.lock", "Dockerfile", ".dockerignore"):
            shutil.copyfile(ROOT / filename, context / filename)
        for name in ("src", "web"):
            shutil.copytree(ROOT / name, context / name)
        canary = "SYNTHETIC-CONTEXT-CANARY-" + uuid.uuid4().hex
        for relative in ("target/private.md", "private/token.json", "deploy/oidc.json", ".git/secret", ".env",
                         "src/private.json", "src/test.env", "web/private.key", "web/private.md"):
            path = context / relative
            path.parent.mkdir(parents=True, exist_ok=True); path.write_text(canary)
        result = subprocess.run(["docker", "build", "--file", "-", "--output", "type=local,dest=" + str(output), str(context)],
                                input=b"FROM scratch\nCOPY . /\n", stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60)
        check(result.returncode == 0, "The isolated Docker build-context audit failed.")
        files = [path for path in output.rglob("*") if path.is_file()]
        check(files and any(path.relative_to(output) == Path("Cargo.toml") for path in files), "The build-context audit did not export application sources.")
        for path in files:
            relative = path.relative_to(output)
            check(relative.parts[0] in {"Cargo.toml", "Cargo.lock", "Dockerfile", "src", "web"}
                  and canary.encode() not in path.read_bytes(), "Docker sent a private/build/deployment canary through the source allowlist.")


def audit_compose():
    baseline = None
    for override in (False, True):
        arguments = ["compose", "-f", str(ROOT / "deploy/compose.yaml")]
        if override:
            arguments += ["-f", str(ROOT / "deploy/compose.loopback.yaml")]
        service = json.loads(docker(*arguments, "config", "--format", "json").stdout)["services"]["mddiff"]
        check(service["user"] == "10001:10001" and service["read_only"] is True and service["init"] is True
              and "ALL" in service["cap_drop"] and not service.get("cap_add") and not service.get("privileged", False)
              and any(value in {"no-new-privileges", "no-new-privileges:true"} for value in service["security_opt"]),
              "Compose weakened the runtime identity, read-only root or privilege protections.")
        check(all(Decimal(str(service[field])) == expected for field, expected in
                  (("pids_limit", 128), ("mem_limit", 512 * 1024 * 1024), ("memswap_limit", 512 * 1024 * 1024), ("cpus", 1))), "Compose lost its process, memory, swap or CPU limits.")
        mounts = {item["target"]: item for item in service["volumes"]}
        targets = {"/var/lib/mddiff", *("/run/secrets/mddiff/" + name for name in ("oidc.json", "connections.json", "connections.key"))}
        check(set(mounts) == targets and len(service["volumes"]) == len(targets)
              and all(item["type"] == "bind" and item.get("bind", {}).get("create_host_path") is False for item in mounts.values())
              and not mounts["/var/lib/mddiff"].get("read_only", False)
              and all(mounts[target].get("read_only") is True for target in targets - {"/var/lib/mddiff"}),
              "Compose changed private external mounts, their access mode or host-path creation policy.")
        ports = service.pop("ports", [])
        if not override:
            check(not ports, "Default Compose publishes an application port to the host.")
            baseline = service
        else:
            check(len(ports) == 1 and ports[0].get("host_ip") == "127.0.0.1" and int(ports[0]["target"]) == 3847
                  and int(ports[0]["published"]) == 3847 and ports[0].get("protocol") == "tcp" and service == baseline,
                  "The loopback override exposed a non-loopback port or changed another runtime setting.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", default="mddiff:issue13")
    parser.add_argument("--client-image", default=CLIENT_IMAGE)
    parser.add_argument("--capacity", action="store_true", help="Run ten private sessions under the Compose resource limits.")
    arguments = parser.parse_args()
    identity = "mddiff-e2e-" + uuid.uuid4().hex[:12]
    containers, volumes = [], []
    client = None
    phase = "preflight"
    try:
        phase = "Compose configuration"
        audit_compose()
        print("Container check passed: Compose defaults and loopback override", flush=True)
        phase = "preflight"
        runtime = load_runtime(fixture.RUNTIME)
        check(runtime["issuer"] == "http://localhost:8848/realms/mddiff-spike", "Only the synthetic Keycloak runtime is permitted.")
        owned = json.loads(docker("inspect", fixture.NAME).stdout)[0]
        check(owned["Config"]["Labels"].get("mddiff.identity-spike") == "true" and owned["State"]["Running"], "Start the owned synthetic Keycloak fixture before this test.")
        network = "container:" + owned["Id"]
        check("@sha256:" in arguments.client_image and arguments.client_image.startswith("python:"), "Pin the official Python client image by digest.")
        if docker("image", "inspect", arguments.client_image, required=False).returncode != 0:
            docker("pull", arguments.client_image, timeout=180)
        secrets = {runtime["admin_password"], runtime["resource_client"]["secret"], *(user["password"] for user in runtime["users"].values())}
        phase = "isolated source build context"
        audit_context()
        phase = "runtime image filesystem"
        tested_image = scan_image(arguments.image, identity, secrets)
        print("Container check passed: image filesystem and identity", flush=True)
        mapping = {}
        for suffix, target in [("data", "/var/lib/mddiff"), ("secrets", "/run/secrets/mddiff"),
                               ("owner", "/negative-owner"), ("mode", "/negative-mode"),
                               ("db", "/negative-db"), ("unsafe", "/negative-secrets"), ("config-data", "/negative-config-data")]:
            name = identity + "-" + suffix
            docker("volume", "create", "--label", LABEL + "=" + identity, name)
            check(json.loads(docker("volume", "inspect", name).stdout)[0].get("Labels", {}).get(LABEL) == identity, "A test volume name belongs to an unrelated resource.")
            volumes.append(name); mapping[suffix] = (name, target)
        with tempfile.TemporaryDirectory(prefix="mddiff-container-ca-") as temporary:
            ca, certificate, key = certificates(Path(temporary))
            payload = {"runtime": runtime, "capacity": arguments.capacity,
                       "tls": {"ca.pem": ca.read_text(), "server.pem": certificate.read_text(), "server.key": key.read_text()}}
            client_name = identity + "-client"
            command = ["docker", "run", "--name", client_name, "--label", LABEL + "=" + identity, "--network", network,
                       "--interactive", "--env", "PYTHONDONTWRITEBYTECODE=1", "--workdir", "/suite"]
            for source, target in mapping.values():
                command += ["--mount", "type=volume,src=" + source + ",dst=" + target]
            for source, target in [(ROOT / "scripts", "/suite/scripts"), (ROOT / "spikes/identity", "/suite/spikes/identity")]:
                command += ["--mount", "type=bind,src=" + str(source) + ",dst=" + target + ",readonly"]
            command += [arguments.client_image, "python", "/suite/scripts/container_client.py"]
            phase = "private client bootstrap"
            client = Client(command, payload); containers.append(client_name)
            ready = client.receive("ready")
            check(ready["uid"] == ready["gid"] == 10001 and ready["origin"].startswith("https://127.0.0.1:"), "Container client weakened its identity or HTTPS origin.")

            def create(name, data="data", configuration="secrets", port=3847):
                command = ["create", "--name", name, "--label", LABEL + "=" + identity, "--network", network,
                    "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges:true",
                    "--init", "--memory", "512m", "--memory-swap", "512m", "--cpus", "1", "--pids-limit", "128"]
                if data != "data":
                    command += ["--workdir", "/"]  # Let the app itself reject inaccessible data instead of failing Docker's initial chdir.
                command += [
                    "--mount", "type=volume,src=" + mapping[data][0] + ",dst=/var/lib/mddiff,volume-nocopy",
                    "--mount", "type=volume,src=" + mapping[configuration][0] + ",dst=/run/secrets/mddiff,readonly,volume-nocopy",
                    tested_image, "--mode", "hosted", "--bind", "0.0.0.0", "--port", str(port), "--data-dir", "/var/lib/mddiff",
                    "--oidc-config", "/run/secrets/mddiff/oidc.json", "--connections-config", "/run/secrets/mddiff/connections.json"]
                docker(*command); containers.append(name)
                info = json.loads(docker("inspect", name).stdout)[0]
                host = info["HostConfig"]
                check(host["Memory"] == host["MemorySwap"] == 512 * 1024 * 1024 and host["NanoCpus"] == 1_000_000_000
                      and host["PidsLimit"] == 128 and host["Init"] is True, "Docker did not configure the intended resource limits.")
                check(info["Config"]["User"] == "10001:10001" and host["ReadonlyRootfs"] and "ALL" in host["CapDrop"]
                      and any(option.startswith("no-new-privileges") for option in host["SecurityOpt"])
                      and host["NetworkMode"] == network and not host.get("PortBindings"), "App container lost its isolation settings.")
                mounts = {item["Destination"]: item for item in info["Mounts"]}
                check(mounts["/var/lib/mddiff"]["RW"] and not mounts["/run/secrets/mddiff"]["RW"]
                      and all(item["Type"] != "bind" for item in info["Mounts"]), "The application mounted secrets writable or mounted the source checkout.")
                docker("start", name)

            def stop(name):
                docker("stop", "--time", "15", name, timeout=20)
                state = json.loads(docker("inspect", name).stdout)[0]["State"]
                check(state["ExitCode"] == 0 and not state["OOMKilled"], "SIGTERM did not exit cleanly while SSE was active.")
                logs = docker("logs", name)
                client.phase("logs", content=(logs.stdout + logs.stderr).decode(errors="replace"))
                client.phase("stopped")

            app = identity + "-app"
            phase = "two accounts in the container"
            create(app); bootstrap = client.phase("first", timeout=120 if arguments.capacity else 60)
            if arguments.capacity:
                phase = "ten-user resource-limited workload"
                report = {"users": 10, "memory_limit_bytes": 512 * 1024 * 1024, "cpu_limit": 1, "pids_limit": 128,
                          "image_id": tested_image, "profiles": [], "status": "running", "bootstrap": bootstrap["metrics"]}
                output = ROOT / "target/capacity-report.json"
                check(docker("exec", app, "cat", "/sys/fs/cgroup/memory.max").stdout.strip() == b"536870912"
                      and docker("exec", app, "cat", "/sys/fs/cgroup/memory.swap.max").stdout.strip() == b"0"
                      and docker("exec", app, "cat", "/sys/fs/cgroup/pids.max").stdout.strip() == b"128", "Linux cgroups did not enforce the declared limits.")
                quota, period = docker("exec", app, "cat", "/sys/fs/cgroup/cpu.max").stdout.split()
                check(quota == period, "Linux cgroups did not enforce the one-CPU quota.")
                report["cgroup_limits_verified"] = True
                fixture.private_write(output, json.dumps(report, indent=2) + "\n")
                for profile in ("small", "near-limit"):
                    report["profiles"].append(client.phase("load", profile=profile, timeout=180)["metrics"])
                    peak = docker("exec", app, "cat", "/sys/fs/cgroup/memory.peak").stdout.strip()
                    report["profiles"][-1]["cgroup_memory_peak_bytes"] = int(peak)
                    fixture.private_write(output, json.dumps(report, indent=2) + "\n")
                client.phase("limits", timeout=120)
                report["concurrent_render_rejections"] = 10
                report["memory_peak_bytes"] = int(docker("exec", app, "cat", "/sys/fs/cgroup/memory.peak").stdout.strip())
                info = json.loads(docker("info", "--format", "{{json .}}").stdout)
                report["environment"] = {key: info[key] for key in ("Architecture", "OSType", "NCPU", "MemTotal", "ServerVersion", "CgroupVersion")}
                phase = "ten-user recovery"
                stop(app); docker("start", app); client.phase("restart", timeout=120)
                report["restart_memory_peak_bytes"] = int(docker("exec", app, "cat", "/sys/fs/cgroup/memory.peak").stdout.strip())
                stop(app); docker("rm", app); containers.remove(app)
                app = identity + "-recreated"; create(app); client.phase("recreated", timeout=120)
                report["recreated_memory_peak_bytes"] = int(docker("exec", app, "cat", "/sys/fs/cgroup/memory.peak").stdout.strip())
                report["memory_peak_bytes"] = max(report[key] for key in ("memory_peak_bytes", "restart_memory_peak_bytes", "recreated_memory_peak_bytes"))
                stop(app)
                report["isolation_and_limits"] = "passed"
                report["restart_and_recreation"] = "passed"
                idp_state = json.loads(docker("inspect", fixture.NAME).stdout)[0]["State"]
                check(idp_state["Running"] and not idp_state["OOMKilled"], "The identity fixture stopped or exceeded its own memory budget during capacity validation.")
                report["idp_fixture_oom"] = False
                report["status"] = "passed" if all(profile["latency_threshold_passed"] for profile in report["profiles"]) else "latency-failed"
                report["memory_peak_ceiling_bytes"] = 460 * 1024 * 1024
                if report["memory_peak_bytes"] > report["memory_peak_ceiling_bytes"]:
                    report["status"] = "memory-margin-failed"
                fixture.private_write(output, json.dumps(report, indent=2) + "\n")
                client.send({"phase": "quit"}); client.receive("closed")
                check(client.process.wait(timeout=15) == 0, "Capacity client failed to restore synthetic OAuth configuration.")
                check(report["status"] == "passed", "Ten-user latency or memory margin exceeded its threshold; aggregate evidence remains in target/capacity-report.json.")
                print("Ten-user capacity integration passed; aggregate-only report: target/capacity-report.json", flush=True)
                return
            check(docker("exec", app, "id", "-u").stdout.strip() == b"10001", "The app is not running as the configured unprivileged user.")
            check(docker("exec", app, "touch", "/run/secrets/mddiff/forbidden", required=False).returncode != 0, "External secrets are writable inside the app.")
            check(docker("exec", app, "curl", "--fail", "--silent", "--output", "/dev/null", "--write-out", "%{http_code}", "http://127.0.0.1:3847/healthz").stdout == b"204", "Image-local public health failed.")
            phase = "SIGTERM and same-container restart"
            stop(app); docker("start", app); client.phase("restart")
            phase = "single SQLite writer"
            concurrent = identity + "-concurrent"
            create(concurrent, port=3848)
            check(docker("wait", concurrent, timeout=12).stdout.strip() == b"1", "A second instance acquired the same SQLite volume.")
            logs = docker("logs", concurrent)
            client.phase("logs", content=(logs.stdout + logs.stderr).decode(errors="replace"))
            phase = "new container with the same durable volumes"
            stop(app); docker("rm", app); containers.remove(app)
            app = identity + "-recreated"
            create(app); client.phase("recreated")
            phase = "incorrect volume ownership and file permissions"
            client.phase("negative-permissions")
            for suffix, data, configuration in [("wrong-owner", "owner", "secrets"), ("wrong-mode", "mode", "secrets"),
                                                 ("public-db", "db", "secrets"), ("public-config", "config-data", "unsafe")]:
                phase = "unsafe volume " + suffix
                name = identity + "-" + suffix
                create(name, data=data, configuration=configuration, port=3848)
                check(docker("wait", name, timeout=12).stdout.strip() == b"1", "Unsafe volume ownership, database or configuration permissions did not fail closed.")
                logs = docker("logs", name)
                client.phase("logs", content=(logs.stdout + logs.stderr).decode(errors="replace"))
            stop(app)
            client.send({"phase": "quit"}); client.receive("closed")
            check(client.process.wait(timeout=15) == 0, "The client did not restore its temporary synthetic OAuth configuration.")
        print("Container integration passed: verified loopback HTTPS, two private accounts/MCP/SSE, encrypted durable volumes, clean SIGTERM, one SQLite writer and closed permission failures.")
    except CheckFailed as error:
        raise SystemExit("Container integration failed during " + phase + ": " + str(error)) from None
    except Exception as error:
        raise SystemExit("Container integration failed during " + phase + " (" + type(error).__name__ + "); private diagnostics were suppressed.") from None
    finally:
        def owned_container(name):
            found = docker("inspect", name, required=False)
            return found.returncode == 0 and json.loads(found.stdout)[0]["Config"].get("Labels", {}).get(LABEL) == identity
        for name in reversed(containers):
            if name.endswith("-client") or not owned_container(name):
                continue
            docker("stop", "--time", "3", name, timeout=8, required=False)
            docker("rm", "-f", name, required=False)
        if client is not None:
            client.close()
        for name in containers:
            if name.endswith("-client") and owned_container(name):
                docker("rm", "-f", name, required=False)
        for name in reversed(volumes):
            found = docker("volume", "inspect", name, required=False)
            if found.returncode == 0 and json.loads(found.stdout)[0].get("Labels", {}).get(LABEL) == identity:
                docker("volume", "rm", name, required=False)


if __name__ == "__main__":
    main()
