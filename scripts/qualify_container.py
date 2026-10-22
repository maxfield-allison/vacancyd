#!/usr/bin/env python3
"""Qualify a vacancyd container image through the Docker CLI alone.

Runs the image the way docs/container.md tells people to: configuration on a
read-only mount, recovery records on a state volume, a read-only root
filesystem, no capabilities. A stub OpenAI-compatible upstream and the test
client share a private internal network with it, so every request arrives as
a remote client would and nothing depends on reaching the Docker host.

Checks: image metadata (non-root user, health check, labels), version, the
missing-configuration failure, the running process's user, health, token
scopes (unauthenticated and wrong-scope requests refused, a borrow request
admitted and served), a lending switch refusing with 503 + Retry-After
without reaching the upstream, a graceful stop, and that `auto` presence
without the host system bus and a GPU resource without the NVIDIA runtime
both refuse to lend.

With --gpu, the headless run lends a real NVIDIA GPU through the NVIDIA
Container Toolkit (`docker run --gpus`) instead of system memory, and the
no-runtime refusal check is skipped. Every container, network and volume it
creates is named with --prefix and removed at the end.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import secrets
import subprocess
import sys
import time

API_PORT = 9811
GATE_PORT = 11435
UPSTREAM_PORT = 18080
USER = "65532:65532"
CONFIG_PATH = "/etc/vacancyd/vacancyd.toml"
# Tests only; any image with Python 3 works. Pinned so runs are comparable.
DEFAULT_FIXTURE_IMAGE = (
    "docker.io/library/python:3.13-slim"
    "@sha256:8d9d0b8bcf6506481eae4907c18f5e3e7902e629f5f6d684f9e7c32e85e3ddf0"
)

# Stub upstream: counts admitted chat requests so the client can prove a
# refused request never reached it.
FIXTURE = r"""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
count = 0
class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def reply(self, value, code=200):
        data = json.dumps(value).encode()
        self.send_response(code)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        if self.path == '/fixture/count':
            self.reply({'count': count})
        else:
            self.reply({'object': 'list', 'data': []})
    def do_POST(self):
        global count
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        if self.path != '/v1/chat/completions' or json.loads(body).get('model') != 'qualification':
            self.reply({'error': 'unexpected'}, 400)
            return
        count += 1
        self.reply({'id': 'qualification', 'choices': [{'message': {'content': 'accepted'}}]})
ThreadingHTTPServer(('0.0.0.0', %d), Handler).serve_forever()
""" % UPSTREAM_PORT

# One HTTP exchange, printed as JSON. Runs inside the fixture container.
CLIENT = r"""
import json, sys, urllib.error, urllib.request
method, url, token, extra, body = sys.argv[1:6]
headers = json.loads(extra)
if token:
    headers['Authorization'] = 'Bearer ' + token
data = body.encode() if body else None
if data is not None:
    headers.setdefault('Content-Type', 'application/json')
request = urllib.request.Request(url, data=data, method=method, headers=headers)
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
try:
    with opener.open(request, timeout=10) as response:
        code, head, payload = response.status, response.headers, response.read()
except urllib.error.HTTPError as error:
    code, head, payload = error.code, error.headers, error.read()
print(json.dumps({'status': code, 'retry_after': head.get('Retry-After'), 'body': payload.decode('utf-8', 'replace')}))
"""

WRITE_FILES = r"""
import json, os, sys
for name, spec in json.load(sys.stdin).items():
    path = os.path.join('/target', name)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, 'w') as stream:
        stream.write(spec['text'])
    os.chmod(path, int(spec['mode'], 8))
    os.chown(path, spec['uid'], spec['uid'])
"""


def render_config(upstream_host: str, presence: str, resource: str, gpu_selector: str = "") -> str:
    """The qualification configuration. `resource` is memory or gpu."""
    lines = [
        "[server]",
        f'listen = "0.0.0.0:{API_PORT}"',
        "",
        "[auth]",
        "trust_loopback = true",
        "allow_unauthenticated_remote = false",
    ]
    for name, scopes in (("borrow", ["borrow"]), ("read", ["read"]), ("admin", ["read", "admin"])):
        lines += [
            "",
            "[[auth.token]]",
            f'name = "{name}"',
            f"scopes = {json.dumps(scopes)}",
            f'value_file = "/etc/vacancyd/tokens/{name}.token"',
        ]
    lines += [
        "",
        "[poll]",
        "interval_ms = 200",
        "",
        "[owner]",
        f'presence = "{presence}"',
        "release_after_s = 0",
        "",
        "[owner.gpu_pressure]",
        "enabled = false",
        "",
        "[[resource]]",
        'id = "cpu"',
        'kind = "system-cpu"',
        "reserve = 99",
        "",
        "[[resource]]",
    ]
    if resource == "gpu":
        lines += ['id = "gpu0"', 'kind = "nvidia-gpu"', f'selector = "{gpu_selector or "0"}"']
        lending, minimum = "gpu0", 1
    elif resource == "memory":
        lines += ['id = "memory"', 'kind = "system-memory"', "reserve = 16777216"]
        lending, minimum = "memory", 1
    else:
        raise ValueError(f"unknown resource {resource!r}")
    lines += [
        "",
        "[[upstream]]",
        'id = "qualification"',
        f'listen = "0.0.0.0:{GATE_PORT}"',
        f'target = "http://{upstream_host}:{UPSTREAM_PORT}"',
        'kind = "openai"',
        f'resource = "{lending}"',
        'also_requires = ["cpu"]',
        f"default_min_free_mib = {minimum}",
        "",
    ]
    return "\n".join(lines)


def image_metadata_failures(inspect: dict, version: str) -> list[str]:
    """Why an image's configuration does not match the documented contract."""
    config = inspect.get("Config") or {}
    failures = []
    if config.get("User") != USER:
        failures.append(f"image user is {config.get('User')!r}, expected {USER}")
    if config.get("Entrypoint") != ["/usr/local/bin/vacancyd"]:
        failures.append(f"unexpected entrypoint {config.get('Entrypoint')!r}")
    if config.get("Cmd") != ["run", "--config", CONFIG_PATH]:
        failures.append(f"unexpected default command {config.get('Cmd')!r}")
    test = (config.get("Healthcheck") or {}).get("Test") or []
    if test[:3] != ["CMD", "/usr/local/bin/vacancyd", "healthcheck"]:
        failures.append(f"unexpected health check {test!r}")
    environment = dict(item.split("=", 1) for item in config.get("Env") or [])
    if environment.get("STATE_DIRECTORY") != "/var/lib/vacancyd":
        failures.append("STATE_DIRECTORY is not /var/lib/vacancyd")
    labels = config.get("Labels") or {}
    if labels.get("org.opencontainers.image.version") != version:
        failures.append(f"version label is {labels.get('org.opencontainers.image.version')!r}")
    for label in ("source", "licenses", "revision", "base.name"):
        if not labels.get(f"org.opencontainers.image.{label}"):
            failures.append(f"missing org.opencontainers.image.{label} label")
    return failures


class Qualification:
    def __init__(self, image: str, version: str, prefix: str, fixture_image: str, evidence: Path, gpus: str):
        self.image = image
        self.version = version
        self.prefix = prefix
        self.fixture_image = fixture_image
        self.evidence = evidence
        self.gpus = gpus
        self.network = f"{prefix}-net"
        self.upstream = f"{prefix}-upstream"
        self.containers: list[str] = []
        self.volumes: list[str] = []
        self.tokens = {name: secrets.token_urlsafe(32) for name in ("borrow", "read", "admin")}
        self.checks: dict[str, str] = {}

    # -- plumbing ---------------------------------------------------------

    def docker(self, *args: str, check: bool = True, stdin: str | None = None, timeout: int = 180):
        result = subprocess.run(
            ["docker", *args], input=stdin, text=True, capture_output=True, timeout=timeout
        )
        if check and result.returncode:
            raise RuntimeError(f"docker {args[0]} {args[1] if len(args) > 1 else ''} failed: {result.stderr.strip()[-600:]}")
        return result

    def passed(self, name: str, detail: str = "passed") -> None:
        self.checks[name] = detail
        print(f"PASS {name}: {detail}", flush=True)

    def http(self, method: str, url: str, token: str = "", headers: dict | None = None, body: dict | None = None) -> dict:
        result = self.docker(
            "exec", self.upstream, "python3", "-c", CLIENT, method, url, token,
            json.dumps(headers or {}), json.dumps(body) if body is not None else "",
        )
        return json.loads(result.stdout)

    def upstream_count(self) -> int:
        reply = self.http("GET", f"http://127.0.0.1:{UPSTREAM_PORT}/fixture/count")
        return json.loads(reply["body"])["count"]

    def write_volume(self, volume: str, files: dict) -> None:
        self.docker(
            "run", "--rm", "-i", "--network", "none", "-v", f"{volume}:/target",
            "--name", f"{self.prefix}-writer", self.fixture_image, "python3", "-c", WRITE_FILES,
            stdin=json.dumps(files),
        )

    def volume(self, name: str) -> str:
        full = f"{self.prefix}-{name}"
        self.docker("volume", "create", full)
        self.volumes.append(full)
        return full

    def start_vacancyd(self, name: str, config_volume: str, state_volume: str, gpus: str = "") -> str:
        container = f"{self.prefix}-{name}"
        args = [
            "run", "-d", "--name", container, "--network", self.network,
            "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
            "--tmpfs", "/run/vacancyd:uid=65532,gid=65532,mode=0755",
            "-v", f"{config_volume}:/etc/vacancyd:ro", "-v", f"{state_volume}:/var/lib/vacancyd",
        ]
        if gpus:
            args += ["--gpus", gpus]
        self.docker(*args, self.image)
        self.containers.append(container)
        return container

    def await_health(self, container: str, timeout: float = 90) -> None:
        deadline = time.monotonic() + timeout
        state = {}
        while time.monotonic() < deadline:
            state = json.loads(self.docker("inspect", "--format", "{{json .State}}", container).stdout)
            if not state.get("Running"):
                raise RuntimeError(f"{container} exited: {self.logs(container)[-800:]}")
            if (state.get("Health") or {}).get("Status") == "healthy":
                return
            time.sleep(1)
        raise RuntimeError(f"{container} never became healthy: {json.dumps(state.get('Health'))[-800:]}")

    def status(self, container: str) -> dict:
        reply = self.http("GET", f"http://{container}:{API_PORT}/status", self.tokens["read"])
        if reply["status"] != 200:
            raise RuntimeError(f"/status answered {reply['status']}")
        return json.loads(reply["body"])

    def await_status(self, container: str, predicate, what: str, timeout: float = 30) -> dict:
        deadline = time.monotonic() + timeout
        status = {}
        while time.monotonic() < deadline:
            status = self.status(container)
            if predicate(status):
                return status
            time.sleep(0.5)
        raise RuntimeError(f"{container}: {what} not reached; last status {json.dumps(status)[-1200:]}")

    def logs(self, container: str) -> str:
        result = self.docker("logs", container, check=False)
        return result.stdout + result.stderr

    def config_files(self, config: str) -> dict:
        files = {"vacancyd.toml": {"text": config, "mode": "0644", "uid": 0}}
        for name, value in self.tokens.items():
            files[f"tokens/{name}.token"] = {"text": value + "\n", "mode": "0400", "uid": 65532}
        return files

    # -- checks -----------------------------------------------------------

    def check_image(self) -> None:
        inspect = json.loads(self.docker("image", "inspect", self.image).stdout)[0]
        failures = image_metadata_failures(inspect, self.version)
        if failures:
            raise RuntimeError("; ".join(failures))
        self.checks["architecture"] = inspect.get("Architecture", "")
        self.passed("image_metadata", f"user {USER}, health check, labels, {inspect.get('Architecture')}")
        reported = self.docker("run", "--rm", "--network", "none", self.image, "--version").stdout.strip()
        if reported != f"vacancyd {self.version}":
            raise RuntimeError(f"image reports {reported!r}")
        self.passed("version", reported)
        missing = self.docker("run", "--rm", "--network", "none", self.image, check=False)
        if missing.returncode == 0 or CONFIG_PATH not in missing.stderr:
            raise RuntimeError(f"missing configuration did not fail clearly: {missing.stderr[-400:]}")
        self.passed("missing_config_refused", next(line.strip() for line in missing.stderr.splitlines() if CONFIG_PATH in line))

    def check_headless(self) -> None:
        resource = "gpu" if self.gpus else "memory"
        config = self.volume("config-headless")
        state = self.volume("state-headless")
        self.write_volume(config, self.config_files(render_config(self.upstream, "headless", resource)))
        container = self.start_vacancyd("headless", config, state, self.gpus)
        self.await_health(container)
        self.passed("healthy", "HEALTHCHECK reports healthy on a read-only root with no capabilities")

        users = self.docker("top", container, "-eo", "pid,uid,gid,comm").stdout.split("\n")[1:]
        ids = {tuple(line.split()[1:3]) for line in users if line.strip()}
        if ids != {("65532", "65532")}:
            raise RuntimeError(f"process identities {sorted(ids)}")
        self.passed("non_root_process", "every process runs as 65532:65532")

        status = self.await_status(
            container, lambda s: all(u["vacancy"] for u in s["upstreams"]), "vacancy"
        )
        if status["presence_backend"] != "headless":
            raise RuntimeError(f"presence backend {status['presence_backend']}")
        if self.gpus:
            gpu = next(r for r in status["resources"] if r["id"] == "gpu0")
            if not gpu.get("total_mib"):
                raise RuntimeError(f"GPU capacity not read: {json.dumps(gpu)}")
            self.passed("gpu_capacity", f"{gpu.get('label')}: {gpu.get('free_mib')} / {gpu.get('total_mib')} MiB free")
        self.passed("headless_vacancy", "presence backend headless; upstream reports VACANCY")

        gate = f"http://{container}:{GATE_PORT}/v1/chat/completions"
        work = {"model": "qualification", "messages": [{"role": "user", "content": "test"}]}
        before = self.upstream_count()
        for token, expected in (("", 401), ("not-a-token", 401), (self.tokens["read"], 403)):
            reply = self.http("POST", gate, token, body=work)
            if reply["status"] != expected:
                raise RuntimeError(f"gate answered {reply['status']} for a {'missing' if not token else 'wrong'} credential")
        if self.upstream_count() != before:
            raise RuntimeError("a refused credential reached the upstream")
        self.passed("gate_refuses_unauthenticated", "no token and bad token 401, read token 403, upstream untouched")

        reply = self.http("POST", gate, self.tokens["borrow"], body=work)
        if reply["status"] != 200 or json.loads(reply["body"]).get("id") != "qualification":
            raise RuntimeError(f"borrow request answered {reply['status']}: {reply['body'][:300]}")
        if self.upstream_count() != before + 1:
            raise RuntimeError("admitted request did not reach the upstream exactly once")
        self.passed("borrow_served", "borrow token admitted and served by the upstream")

        api = f"http://{container}:{API_PORT}"
        for path, token, expected in (
            ("/status", "", 401), ("/status", self.tokens["borrow"], 403),
            ("/metrics", "", 401), ("/healthz", "", 200),
        ):
            reply = self.http("GET", api + path, token)
            if reply["status"] != expected:
                raise RuntimeError(f"GET {path} answered {reply['status']}, expected {expected}")
        admin = {"X-Vacancyd-Admin": "1"}
        if self.http("POST", api + "/admin/lending/disable", self.tokens["borrow"], admin, {})["status"] != 403:
            raise RuntimeError("borrow token disabled lending")
        self.passed("api_scopes", "status/metrics need read, healthz open, admin refuses borrow")

        if self.http("POST", api + "/admin/lending/disable", self.tokens["admin"], admin, {})["status"] != 200:
            raise RuntimeError("admin token could not disable lending")
        count = self.upstream_count()
        reply = self.http("POST", gate, self.tokens["borrow"], body=work)
        if reply["status"] != 503 or not reply["retry_after"]:
            raise RuntimeError(f"disabled lending answered {reply['status']} retry-after {reply['retry_after']}")
        if self.upstream_count() != count:
            raise RuntimeError("disabled lending reached the upstream")
        self.passed("lending_switch", f"503 with Retry-After {reply['retry_after']}, upstream untouched")

        if self.http("POST", api + "/admin/lending/enable", self.tokens["admin"], admin, {})["status"] != 200:
            raise RuntimeError("admin token could not enable lending")
        self.await_status(container, lambda s: all(u["vacancy"] for u in s["upstreams"]), "vacancy after enable")
        if self.http("POST", gate, self.tokens["borrow"], body=work)["status"] != 200:
            raise RuntimeError("re-enabled lending did not serve")

        # Loopback inside the container reads and pauses only; the admin
        # socket decides the rest by the caller's account (docs/container.md).
        def admin_request(action: dict, user: str = "") -> int:
            request = json.dumps({"protocol_version": 1, "action": action})
            argv = ["exec", "-i"] + (["-u", user] if user else []) + [
                container, "/usr/local/bin/vacancyd", "admin-request", "--request", "-"]
            return self.docker(*argv, stdin=request, check=False).returncode
        switch_off = {"kind": "set_resource", "resource": "cpu", "enabled": False}
        switch_on = {"kind": "set_resource", "resource": "cpu", "enabled": True}
        if admin_request(switch_off) != 20:
            raise RuntimeError("the service user switched a resource without being an administrator")
        if admin_request({"kind": "pause"}) != 0 or admin_request({"kind": "resume"}) != 0:
            raise RuntimeError("the service user could not pause and resume its own pause")
        if admin_request(switch_off, "0") != 0 or admin_request(switch_on, "0") != 0:
            raise RuntimeError("root in the container could not switch a resource over the admin socket")
        self.passed("admin_socket", "service user may pause and resume only; root via docker exec administers")

        probe = self.docker(
            "run", "--rm", "--network", "none", "-v", f"{state}:/state:ro", self.fixture_image,
            "python3", "-c",
            "import os,json; s=os.stat('/state/admission'); print(json.dumps([s.st_uid, s.st_gid, oct(s.st_mode & 0o777)]))",
        ).stdout
        if json.loads(probe) != [65532, 65532, "0o700"]:
            raise RuntimeError(f"admission directory ownership/mode {probe.strip()}")
        self.passed("state_volume", "admission records on the state volume, 65532 0700")

        started = time.monotonic()
        self.docker("stop", "--time", "20", container)
        elapsed = time.monotonic() - started
        code = self.docker("inspect", "--format", "{{.State.ExitCode}}", container).stdout.strip()
        if code != "0" or elapsed > 15:
            raise RuntimeError(f"stop took {elapsed:.1f}s with exit code {code}")
        self.passed("graceful_stop", f"SIGTERM handled in {elapsed:.1f}s, exit 0")
        (self.evidence / "headless.log").write_text(self.logs(container))

    def check_refusal(self, name: str, presence: str, resource: str, expected: str) -> None:
        config = self.volume(f"config-{name}")
        state = self.volume(f"state-{name}")
        self.write_volume(config, self.config_files(render_config(self.upstream, presence, resource)))
        container = self.start_vacancyd(name, config, state)
        self.await_health(container)
        count = self.upstream_count()
        time.sleep(2)  # several polls: a refusal must hold, not just start closed
        work = {"model": "qualification", "messages": [{"role": "user", "content": "test"}]}
        reply = self.http("POST", f"http://{container}:{GATE_PORT}/v1/chat/completions", self.tokens["borrow"], body=work)
        if reply["status"] != 503 or self.upstream_count() != count:
            raise RuntimeError(f"{name}: lent ({reply['status']}) when it must refuse")
        logs = self.logs(container)
        (self.evidence / f"{name}.log").write_text(logs)
        if expected not in logs + reply["body"]:
            raise RuntimeError(f"{name}: refusal does not explain itself ({expected!r} absent)")
        self.passed(name, f"503, upstream untouched; explains: {expected}")
        self.docker("stop", "--time", "20", container)

    def run(self) -> None:
        self.docker("network", "create", "--internal", self.network)
        self.docker(
            "run", "-d", "--name", self.upstream, "--network", self.network,
            self.fixture_image, "python3", "-c", FIXTURE,
        )
        self.containers.append(self.upstream)
        self.check_image()
        self.check_headless()
        self.check_refusal("auto_presence_without_bus", "auto", "memory", "systemd-logind is unavailable")
        if not self.gpus:
            self.check_refusal("gpu_without_runtime", "headless", "gpu", "nvidia-smi")

    def cleanup(self) -> None:
        for container in reversed(self.containers):
            self.docker("rm", "-f", "-v", container, check=False)
        self.docker("rm", "-f", f"{self.prefix}-writer", check=False)
        for volume in self.volumes:
            self.docker("volume", "rm", "-f", volume, check=False)
        self.docker("network", "rm", self.network, check=False)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--image", required=True, help="image reference or ID to qualify")
    parser.add_argument("--version", required=True, help="expected vacancyd version, without v")
    parser.add_argument("--evidence", required=True, type=Path, help="new directory for the receipt and logs")
    parser.add_argument("--prefix", default="vacancyd-qualify-" + secrets.token_hex(4))
    parser.add_argument("--fixture-image", default=DEFAULT_FIXTURE_IMAGE)
    parser.add_argument("--gpu", default="", metavar="DEVICES",
                        help="lend a real NVIDIA GPU: value for docker run --gpus, e.g. all or device=0")
    args = parser.parse_args()
    if args.evidence.exists():
        parser.error("evidence directory must be new")
    args.evidence.mkdir(parents=True)
    run = Qualification(args.image, args.version.removeprefix("v"), args.prefix, args.fixture_image, args.evidence, args.gpu)
    receipt = {"image": args.image, "version": args.version, "gpu": args.gpu or None, "passed": False}
    try:
        run.run()
        receipt["passed"] = True
    except Exception as error:  # the receipt records any failure
        receipt["error"] = str(error)
        for container in run.containers:
            (args.evidence / f"{container}.failure.log").write_text(run.logs(container))
    finally:
        receipt["checks"] = run.checks
        receipt["image_id"] = run.docker("image", "inspect", "--format", "{{.Id}}", args.image, check=False).stdout.strip()
        receipt["server"] = run.docker("version", "--format", "{{.Server.Version}} {{.Server.Os}}/{{.Server.Arch}}", check=False).stdout.strip()
        run.cleanup()
        (args.evidence / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps(receipt, indent=2))
    return 0 if receipt["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
