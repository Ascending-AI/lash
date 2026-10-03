#!/usr/bin/env python3
"""Run lash's real-server Restate suites beside a pinned `restate-server`.

This is the one owner of how a live Restate suite gets its server and how its
tests are driven. It follows the recipe Restate's own SDK test suites use
(FIG-3666):

* One `restate-server` per shard, shared by every test the shard runs. The
  server is the pinned static release binary, not a container: it is ready in
  about 0.3 s, so a job runs several side by side.
* Isolation by unique keys, never by resetting the server. Every suite names
  its sessions, groups and workflows per run.
* Every port a run binds is a `ReservedPort`: reserved bound at plan time
  and claimed only when its consumer binds, so a port is never free between
  being chosen and being served.
* Two legs per suite. `live` runs Restate's defaults. `replay` sets the
  invoker's inactivity timeout to zero, so an invocation suspends at every
  await and every resumption replays its journal from the start: the leg that
  proves a handler deterministic (upstream's `alwaysSuspending` suites).
* Bounded retries: Restate's default policy retries a failing invocation 70
  times over most of an hour, so a law whose handler failed for good used to
  hold the job until GitHub cancelled it. Every shard's server retries on a
  short, bounded schedule instead -- long enough that the laws which crash a
  handler attempt on purpose still see the redelivery they exercise, short
  enough that a permanently failing invocation is killed in about half a
  minute.
* Each test runs in its own process, so one law's process state cannot leak
  into the next law, and under a bound on how long it may go without
  progress, so a hung law fails in minutes with its own output and the
  server's log. A law that is silent has made no progress since it started;
  a law made of many steps writes a `PROGRESS_MARKER` line as each step
  completes, and its bound restarts at every one. The bound is then what a
  hang is -- no step finishing -- and never how long a starved host takes to
  run a law's whole workload.

The suites themselves -- the Buck2 label of the test binary, the filters and
the endpoints each shard binds -- live in
`scripts/restate-suites.toml`; the replay leg's known divergences live one
ticket per file under `scripts/restate-divergences/`.

Usage:
  restate_suite.py suite <name> --leg live|replay [--artifacts DIR]
      Build the suite's test binary on the shared build pool, then run it.
  restate_suite.py serve [--leg live] [--name N] [--port-base P] -- <command...>
      Run <command> beside one server, with RESTATE_INGRESS_URL and
      RESTATE_ADMIN_URL exported (for drivers that own their test processes).
      A gate that owns a port block passes its base: the server then binds
      ingress, admin and node on P, P+1 and P+2 instead of free ports.
  restate_suite.py build <label>...
      Build Buck2 labels from the shared cache and print each output path.
  restate_suite.py stage-binaries <package> <dir>
      Build every Rust binary of a Buck2 package from the shared cache and
      copy each into <dir> under its Cargo name, stripped.
  restate_suite.py server-path
      Print the pinned server binary, fetching and verifying it on first use.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import lzma
import os
import platform
import queue
import re
import shutil
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import tomllib
import urllib.request
import urllib.error
import urllib.parse
from dataclasses import dataclass, field
from pathlib import Path
from typing import Sequence

ROOT = Path(__file__).resolve().parents[2]
INVENTORY = ROOT / "tools/buck2/target-inventory.json"
VM_WORKER_LABEL = "//crates/lash-vm-worker:lash-vm-worker__bin"
REGISTRY = ROOT / "scripts" / "restate-suites.toml"
# The replay leg's known divergences, sharded one file per ticket that brings
# its laws back, so two changes never queue on restate-suites.toml.
DIVERGENCE_DIR = ROOT / "scripts" / "restate-divergences"

# ---------------------------------------------------------------------------
# The pinned server. The version matches the `restatedev/restate` image the
# compose-based harnesses use; bump them together.
# ---------------------------------------------------------------------------
RESTATE_VERSION = "1.7.12"
RESTATE_ARCHIVES = {
    "x86_64": (
        "restate-server-x86_64-unknown-linux-musl",
        "6e0fe06b730a64c3690c8611800cb01ddd09812b8de603566c01b7b63f5f6868",
    ),
    "aarch64": (
        "restate-server-aarch64-unknown-linux-musl",
        "b6cb7c107f9e15635fd9840c78028201382e4f11b5a5de1610dee2c343d821e5",
    ),
}
RELEASE_URL = "https://github.com/restatedev/restate/releases/download/v{version}/{name}.tar.xz"

# ---------------------------------------------------------------------------
# Server configuration.
# ---------------------------------------------------------------------------
LEGS: dict[str, dict[str, str]] = {
    "live": {},
    "replay": {"RESTATE_WORKER__INVOKER__INACTIVITY_TIMEOUT": "0s"},
}
# Every shard's retry policy: redelivery within tens of milliseconds (the laws
# that crash a handler attempt on purpose need it), and a permanently failing
# invocation killed in about half a minute instead of retried for most of an
# hour.
RETRIES_BOUNDED = {
    "RESTATE_DEFAULT_RETRY_POLICY__INITIAL_INTERVAL": "50ms",
    "RESTATE_DEFAULT_RETRY_POLICY__EXPONENTIATION_FACTOR": "2.0",
    "RESTATE_DEFAULT_RETRY_POLICY__MAX_INTERVAL": "1s",
    "RESTATE_DEFAULT_RETRY_POLICY__MAX_ATTEMPTS": "30",
    "RESTATE_DEFAULT_RETRY_POLICY__ON_MAX_ATTEMPTS": "kill",
}
# Every server: one partition (one node, no load to spread), a RocksDB budget
# small enough for several shards on one runner, and TCP-only loopback
# listeners (the default also binds unix sockets under the base dir).
BASE_SERVER_ENV = {
    "RESTATE_DEFAULT_NUM_PARTITIONS": "1",
    "RESTATE_ROCKSDB_TOTAL_MEMORY_SIZE": "256MB",
    "RESTATE_LOG_FILTER": "warn,restate=info",
    "RESTATE_LOG_FORMAT": "compact",
    "RESTATE_LOG_DISABLE_ANSI_CODES": "true",
    "RESTATE_LISTEN_MODE": "tcp",
    "RESTATE_BIND_IP": "127.0.0.1",
}
# The invoker must reach a test's endpoint on loopback directly; a proxy in
# the caller's environment would route its calls through the proxy.
PROXY_VARIABLES = frozenset(
    {"HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "all_proxy", "no_proxy"}
)
SERVER_READY_SECONDS = 60.0


def log(message: str) -> None:
    print(f"[restate-suite] {message}", file=sys.stderr, flush=True)


# ---------------------------------------------------------------------------
# The pinned binary.
# ---------------------------------------------------------------------------
def server_path() -> Path:
    """The pinned server binary, fetched and sha256-verified on first use."""
    explicit = os.environ.get("LASH_RESTATE_SERVER_BIN")
    if explicit:
        return Path(explicit)
    machine = platform.machine()
    if machine not in RESTATE_ARCHIVES:
        raise SystemExit(f"no pinned restate-server for {machine}")
    name, digest = RESTATE_ARCHIVES[machine]
    cache = os.environ.get("LASH_RESTATE_SERVER_CACHE") or str(
        Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache") / "lash" / "restate-server"
    )
    target = Path(cache) / RESTATE_VERSION / name / "restate-server"
    if target.is_file() and os.access(target, os.X_OK):
        return target
    target.parent.mkdir(parents=True, exist_ok=True)
    url = RELEASE_URL.format(version=RESTATE_VERSION, name=name)
    log(f"fetching {url}")
    with tempfile.TemporaryDirectory(dir=target.parent) as scratch:
        archive = Path(scratch) / f"{name}.tar.xz"
        for attempt in range(1, 6):
            try:
                with urllib.request.urlopen(url, timeout=120) as response, archive.open("wb") as out:
                    shutil.copyfileobj(response, out)
                break
            except OSError as error:
                if attempt == 5:
                    raise SystemExit(f"could not fetch {url}: {error}") from error
                log(f"fetch attempt {attempt} failed: {error}")
                time.sleep(3 * attempt)
        actual = hashlib.sha256(archive.read_bytes()).hexdigest()
        if actual != digest:
            raise SystemExit(f"{url}: sha256 {actual} does not match the pin {digest}")
        staged = Path(scratch) / "restate-server"
        with lzma.open(archive) as xz, tarfile.open(fileobj=xz) as tar:
            extracted = tar.extractfile(f"{name}/restate-server")
            if extracted is None:
                raise SystemExit(f"{url}: the archive holds no restate-server")
            with staged.open("wb") as out:
                shutil.copyfileobj(extracted, out)
        staged.chmod(0o755)
        os.replace(staged, target)
    return target


# ---------------------------------------------------------------------------
# A running server.
# ---------------------------------------------------------------------------
class ReservedPort:
    """A loopback port held bound until its consumer claims it.

    Probing a port and releasing it only proves the port was free *then*: a
    sibling shard's server still booting, another probe, or a listener the
    server opens internally can take it before the consumer binds -- which
    is how one shard's endpoint port was once stolen by a sibling's
    restate-server mid-boot and that shard's every test failed
    EADDRINUSE. A reservation keeps its socket bound, so the port cannot be
    issued to any bind until `claim` releases it.
    """

    def __init__(self) -> None:
        self._socket: socket.socket | None = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._socket.bind(("127.0.0.1", 0))
        self.port = self._socket.getsockname()[1]

    def claim(self) -> int:
        """Release the reservation and return the port for the consumer to bind."""
        if self._socket is None:
            raise RuntimeError("a reserved port is claimed once")
        port = self.port
        self._socket.close()
        self._socket = None
        return port

    def close(self) -> None:
        if self._socket is not None:
            self._socket.close()
            self._socket = None


class FixedPort:
    """A port a gate owns: its port block is the gate's alone (a kiln gate's
    or a worktree slot's lock guards it), so nothing is held until the
    consumer binds it."""

    def __init__(self, port: int) -> None:
        self.port = port

    def claim(self) -> int:
        return self.port

    def close(self) -> None:
        pass


def http_ok(url: str, body: bytes | None = None) -> bool:
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    request = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
    try:
        with opener.open(request, timeout=2) as response:
            return 200 <= response.status < 300
    except OSError:
        return False


def tail(path: Path, lines: int) -> str:
    try:
        content = path.read_text(errors="replace").splitlines()
    except OSError:
        return "(no log)"
    return "\n".join(content[-lines:])


SERVER_PORT_ROLES = ("ingress", "admin", "node")


class RestateServer:
    def __init__(self, name: str, workdir: Path, config: dict[str, str], port_base: int | None = None) -> None:
        self.name = name
        self.workdir = workdir
        self.config = config
        # Reserved at construction so a suite can hold every shard's ports
        # before the first server starts binding them; a gate that owns a
        # port block binds its own.
        self.reserved: dict[str, ReservedPort | FixedPort] = (
            {role: ReservedPort() for role in SERVER_PORT_ROLES}
            if port_base is None
            else {role: FixedPort(port_base + offset) for offset, role in enumerate(SERVER_PORT_ROLES)}
        )
        self.ingress_port = 0
        self.admin_port = 0
        self.process: subprocess.Popen[bytes] | None = None
        self.data_dir = ""

    @property
    def ingress_url(self) -> str:
        return f"http://127.0.0.1:{self.ingress_port}"

    @property
    def admin_url(self) -> str:
        return f"http://127.0.0.1:{self.admin_port}"

    @property
    def log_path(self) -> Path:
        return self.workdir / f"{self.name}.restate-server.log"

    def start(self) -> None:
        try:
            self._start()
        except BaseException:
            self.stop()
            raise

    def _start(self) -> None:
        binary = server_path()
        self.workdir.mkdir(parents=True, exist_ok=True)
        # A short scratch path of its own, kept out of the artifact tree.
        self.data_dir = tempfile.mkdtemp(prefix="lash-restate-")
        # Claim at the last moment the runner controls: the reservations
        # stayed bound while every sibling reserved its own, so nothing in
        # the run could have taken them.
        self.ingress_port = self.reserved["ingress"].claim()
        self.admin_port = self.reserved["admin"].claim()
        env = {key: value for key, value in os.environ.items() if key not in PROXY_VARIABLES}
        env.update(BASE_SERVER_ENV)
        env.update(self.config)
        env.update(
            {
                "RESTATE_BASE_DIR": self.data_dir,
                "RESTATE_NODE_NAME": "n1",
                "RESTATE_CLUSTER_NAME": f"lash-{self.name}",
                "RESTATE_BIND_PORT": str(self.reserved["node"].claim()),
                "RESTATE_INGRESS__BIND_ADDRESS": f"127.0.0.1:{self.ingress_port}",
                "RESTATE_ADMIN__BIND_ADDRESS": f"127.0.0.1:{self.admin_port}",
            }
        )
        with self.log_path.open("wb") as log_file:
            self.process = subprocess.Popen(
                [str(binary), "--no-logo"],
                env=env,
                stdout=log_file,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        started = time.monotonic()
        # Health can answer before partition 0 has a queryable leader.
        for url, body in (
            (f"{self.admin_url}/health", None),
            (f"{self.ingress_url}/restate/health", None),
            (f"{self.admin_url}/query", json.dumps({"query": "SELECT id FROM sys_invocation LIMIT 1"}).encode()),
        ):
            while not http_ok(url, body):
                if self.process.poll() is not None:
                    raise SystemExit(
                        f"restate-server {self.name} exited with {self.process.returncode} "
                        f"before {url} answered:\n{tail(self.log_path, 40)}"
                    )
                if time.monotonic() - started > SERVER_READY_SECONDS:
                    raise SystemExit(f"restate-server {self.name}: {url} not ready\n{tail(self.log_path, 40)}")
                time.sleep(0.05)
        log(f"server {self.name} ready in {time.monotonic() - started:.2f}s at {self.ingress_url}")

    def stop(self) -> None:
        try:
            if self.process is not None and self.process.poll() is None:
                os.killpg(self.process.pid, signal.SIGTERM)
                try:
                    self.process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    os.killpg(self.process.pid, signal.SIGKILL)
                    self.process.wait()
        finally:
            for reservation in self.reserved.values():
                reservation.close()
            if self.data_dir:
                shutil.rmtree(self.data_dir, ignore_errors=True)

    def env(self) -> dict[str, str]:
        return {"RESTATE_INGRESS_URL": self.ingress_url, "RESTATE_ADMIN_URL": self.admin_url}


# ---------------------------------------------------------------------------
# Building a test binary from the shared cache.
# ---------------------------------------------------------------------------
def build(labels: Sequence[str]) -> list[Path]:
    """Build labels on the shared pool and return their output files.

    The hermetic driver uses the shared pool in a Kiln fork or configured CI
    checkout. Outputs are resolved from the build report, never from a
    configuration-hashed buck-out path.
    """
    build_labels = list(dict.fromkeys([*labels, VM_WORKER_LABEL]))
    report_root = Path(os.environ.get("RUNNER_TEMP", tempfile.gettempdir()))
    report = report_root / f"restate-build-{os.getpid()}.json"
    argv = [
        str(ROOT / "scripts/hermetic-build.sh"),
        "build",
        "--jobs",
        "32" if os.environ.get("GITHUB_ACTIONS") else "16",
        "--materializations",
        "final",
        "--build-report",
        str(report),
        *build_labels,
    ]
    log(f"building {' '.join(labels)}")
    subprocess.run(argv, cwd=ROOT, check=True, stdout=sys.stderr)

    def output(label: str) -> Path:
        result = subprocess.run(
            [
                sys.executable,
                "tools/buck2/outputs.py",
                "--report",
                str(report),
                "--label",
                label,
                "--single",
            ],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
        path = Path(result.stdout.strip())
        if not path.is_file():
            raise SystemExit(f"{label} built, but {path} is missing")
        return path

    worker = output(VM_WORKER_LABEL)
    os.environ["LASH_VM_WORKER"] = str(worker)
    return [output(label) for label in labels]


def package_binaries(package: str) -> dict[str, str]:
    """A package's Rust binary labels and their Cargo names, from generated inventory."""
    manifest = package.removeprefix("//") + "/Cargo.toml"
    binaries = {
        target["label"]: target["cargo"]
        for entry in json.loads(INVENTORY.read_text(encoding="utf-8"))["packages"]
        if entry["manifest"] == manifest
        for target in entry["targets"]
        if target.get("kind") == "bin"
    }
    if not binaries:
        raise SystemExit(f"{INVENTORY.relative_to(ROOT)} declares no binaries for {package}")
    return binaries


def stage_binaries(package: str, destination: Path) -> list[Path]:
    """Build a package's binaries and stage them under their Cargo names.

    Buck2 names a binary's output after its crate (`lash_e2e_worker`), but
    consumers (compose files, the E2E drivers) mount the Cargo `[[bin]]` name
    (`lash-e2e-worker`), which the generated inventory records per label.
    Each is stripped, as Cargo's release profile strips them: the stage is
    shipped between jobs, and an unstripped fastbuild binary is roughly twice
    the size.
    """
    binaries = package_binaries(package)
    binaries[VM_WORKER_LABEL] = package_binaries("//crates/lash-vm-worker")[VM_WORKER_LABEL]
    destination.mkdir(parents=True, exist_ok=True)
    staged = []
    for label, built in zip(binaries, build(list(binaries)), strict=True):
        target = destination / binaries[label]
        shutil.copyfile(built, target)
        target.chmod(0o755)
        subprocess.run(["strip", str(target)], check=True)
        staged.append(target)
    return staged


# ---------------------------------------------------------------------------
# The suite registry.
# ---------------------------------------------------------------------------
@dataclass(frozen=True)
class Suite:
    name: str
    label: str
    cwd: str
    filters: tuple[str, ...]
    skips: tuple[str, ...]
    endpoints: tuple[str, ...]
    env: dict[str, str]
    shards: int
    timeout_seconds: float
    panic_gate: bool
    leg_server_env: dict[str, dict[str, str]]
    replay_divergent: dict[str, str]
    # A leg whose failures are reported but do not fail the run, with the
    # reason it does not gate yet.
    report_only: dict[str, str]


def merge_divergent_shard(suites: dict[str, dict], shard: Path) -> None:
    """Fold one per-ticket divergence shard into the suite registry.

    A shard is named for the ticket that brings its laws back
    (``FIG-<n>.toml``) and carries only ``[suites.<name>.replay.divergent]``
    tables: a law the replay leg holds back, and the reason -- ending in the
    shard's own ticket -- it is held. A new known divergence is a new file or
    one line in its ticket's shard, never an edit to restate-suites.toml.
    """
    overlay = tomllib.loads(shard.read_text(encoding="utf-8")).get("suites", {})
    where = f"{DIVERGENCE_DIR.name}/{shard.name}"
    for name, suite_overlay in overlay.items():
        if name not in suites:
            raise SystemExit(
                f"{where} holds back laws of suite `{name}`, which "
                f"{REGISTRY.name} does not register"
            )
        for leg, leg_overlay in suite_overlay.items():
            if leg != "replay" or set(leg_overlay) != {"divergent"}:
                raise SystemExit(
                    f"{where}: a divergence shard carries only "
                    f"[suites.<name>.replay.divergent] tables"
                )
            divergent = suites[name].setdefault(leg, {}).setdefault("divergent", {})
            for law, reason in leg_overlay["divergent"].items():
                if law in divergent:
                    raise SystemExit(
                        f"{where}: `{law}` is already held back -- a divergence "
                        "names one ticket and one shard"
                    )
                if not reason.rstrip().endswith(f"({shard.stem})"):
                    raise SystemExit(
                        f"{where}: `{law}` must name {shard.stem}, the ticket "
                        "that brings it back, as the reason's `(FIG-n)` suffix"
                    )
                divergent[law] = reason


def load_registry() -> dict[str, dict]:
    with REGISTRY.open("rb") as handle:
        suites = tomllib.load(handle)["suites"]
    if DIVERGENCE_DIR.is_dir():
        for shard in sorted(DIVERGENCE_DIR.glob("*.toml")):
            merge_divergent_shard(suites, shard)
    return suites


def load_suite(name: str) -> Suite:
    suites = load_registry()
    if name not in suites:
        raise SystemExit(f"no Restate suite `{name}` in {REGISTRY.relative_to(ROOT)}; known: {sorted(suites)}")
    raw = suites[name]
    return Suite(
        name=name,
        label=raw["label"],
        cwd=raw["cwd"],
        filters=tuple(raw["filters"]),
        skips=tuple(raw.get("skips", ())),
        endpoints=tuple(raw.get("endpoints", ())),
        env=dict(raw.get("env", {})),
        shards=int(raw.get("shards", 1)),
        timeout_seconds=float(raw.get("timeout_seconds", 300)),
        panic_gate=bool(raw.get("panic_gate", False)),
        leg_server_env={leg: dict(raw.get(leg, {}).get("server_env", {})) for leg in LEGS},
        replay_divergent=dict(raw.get("replay", {}).get("divergent", {})),
        report_only={
            leg: raw[leg]["report_only"] for leg in LEGS if isinstance(raw.get(leg), dict) and "report_only" in raw[leg]
        },
    )


def divergence_shard(suite: Suite, law: str) -> str:
    """The file a held law's divergent entry lives in, for strict-xfail output.

    A shard's reasons end in `(FIG-n)` naming the ticket file that holds them;
    an inline `[suites.<name>.replay.divergent]` entry is edited in the
    registry itself.
    """
    match = re.search(r"\((FIG-\d+)\)\s*$", suite.replay_divergent.get(law, ""))
    if match:
        return f"{DIVERGENCE_DIR.relative_to(ROOT)}/{match.group(1)}.toml"
    return str(REGISTRY.relative_to(ROOT))


def list_tests(binary: Path, cwd: Path, filters: Sequence[str], skips: Sequence[str]) -> list[str]:
    names: list[str] = []
    for test_filter in filters:
        argv = [str(binary), test_filter]
        for skip in skips:
            argv += ["--skip", skip]
        argv += ["--list", "--ignored", "--format", "terse"]
        listing = subprocess.run(argv, cwd=cwd, check=True, capture_output=True, text=True).stdout
        for line in listing.splitlines():
            if line.endswith(": test"):
                name = line.removesuffix(": test")
                if name not in names:
                    names.append(name)
    return names


# ---------------------------------------------------------------------------
# Running tests over shards.
# ---------------------------------------------------------------------------
# The line prefix a law writes as each of its steps completes. Only this
# marker restarts a law's bound: other output, a retry loop's logging say, is
# not progress.
PROGRESS_MARKER = "[restate-suite progress] "


@dataclass
class Outcome:
    name: str
    shard: str
    status: str  # ok | failed | panicked | timeout | leftovers | teardown_failed
    seconds: float
    log: Path


@dataclass
class ShardPlan:
    name: str
    config: dict[str, str]
    tests: "queue.Queue[str]"
    server: "RestateServer | None" = None
    endpoints: "dict[str, ReservedPort]" = field(default_factory=dict)


def drain_leftovers(admin_url: str, name: str, log_path: Path) -> bool:
    """Kill and name open work before another law can use this shard."""
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    deadline = time.monotonic() + 30
    seen: set[str] = set()

    def request(path: str, method: str, body: bytes | None = None) -> object:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("leftover invocations never completed")
        req = urllib.request.Request(
            f"{admin_url.rstrip('/')}/{path}", data=body, method=method,
            headers={"Content-Type": "application/json", "Accept": "application/json"},
        )
        with opener.open(req, timeout=min(5, remaining)) as response:
            return json.load(response) if method == "POST" else None

    try:
        while True:
            response = request("query", "POST", json.dumps({
                "query": "SELECT id, target, status, retry_count, last_failure FROM sys_invocation "
                "WHERE status != 'completed' ORDER BY id",
            }).encode())
            rows = sorted(response["rows"], key=lambda row: row["id"])
            if not rows:
                return bool(seen)
            fresh = [row for row in rows if row["id"] not in seen]
            with log_path.open("a") as out:
                if not seen:
                    out.write(f"\nleftover invocations after {name}:\n")
                for row in fresh:
                    retries = row.get("retry_count")
                    attempt = retries + 1 if retries is not None else "unknown"
                    out.write(f"  {row['id']} target={row['target']!r} status={row['status']} "
                              f"attempt={attempt} last_failure={row.get('last_failure')!r}\n")
            errors = []
            for row in fresh:
                seen.add(row["id"])
                try:
                    request(f"invocations/{urllib.parse.quote(row['id'], safe='')}/kill", "PATCH")
                except urllib.error.HTTPError as error:
                    # A concurrent completion or retention sweep is accepted
                    # only when the next census confirms this id is gone.
                    if error.code not in (404, 409):
                        errors.append(f"kill {row['id']}: {error}")
                except OSError as error:
                    errors.append(f"kill {row['id']}: {error}")
            if errors:
                raise RuntimeError("; ".join(errors))
            if time.monotonic() >= deadline:
                raise TimeoutError("leftover invocations never completed")
            time.sleep(0.02)
    except Exception as error:
        message = f"{name}: teardown failed: {error}"
        with log_path.open("a") as out:
            out.write(f"\n{message}\n")
        raise RuntimeError(message) from error


def run_one(
    binary: Path, cwd: Path, name: str, env: dict[str, str], timeout: float, log_path: Path, panic_gate: bool
) -> tuple[str, float]:
    argv = [str(binary), name, "--exact", "--ignored", "--nocapture", "--test-threads=1"]
    started = time.monotonic()
    timed_out = False
    with log_path.open("wb") as out, log_path.open("rb") as progress:
        process = subprocess.Popen(argv, cwd=cwd, env=env, stdout=out, stderr=subprocess.STDOUT, start_new_session=True)
        # The law is killed once `timeout` passes with no step completed: since
        # it started, or since its last progress marker.
        last_progress = started
        pending = b""
        marker = PROGRESS_MARKER.encode()
        while True:
            try:
                code = process.wait(timeout=0.25)
                break
            except subprocess.TimeoutExpired:
                pass
            pending += progress.read()
            *lines, pending = pending.split(b"\n")
            if any(marker in line for line in lines):
                last_progress = time.monotonic()
            if time.monotonic() - last_progress > timeout:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                timed_out = True
                break
    output = log_path.read_text(errors="replace")
    # A name that matched nothing exits 0; a law that ran says so.
    if timed_out:
        status = "timeout"
    elif code != 0 or "test result: ok. 1 passed" not in output:
        status = "failed"
    # A panic on a background task can leave the law itself green.
    elif panic_gate and "panicked at" in output:
        status = "panicked"
    else:
        status = "ok"
    if drain_leftovers(env["RESTATE_ADMIN_URL"], name, log_path):
        with log_path.open("a") as out:
            out.write(f"process status before teardown: {status}\n")
        status = "leftovers"
    return status, time.monotonic() - started


def run_suite(suite: Suite, leg: str, args: argparse.Namespace) -> int:
    if args.binary:
        binary = Path(args.binary).resolve()
        if not os.environ.get("LASH_VM_WORKER"):
            build([VM_WORKER_LABEL])
    else:
        (binary,) = build([suite.label])
    cwd = ROOT / suite.cwd
    artifacts = Path(args.artifacts).resolve() / f"{suite.name}-{leg}"
    shutil.rmtree(artifacts, ignore_errors=True)
    artifacts.mkdir(parents=True)
    # Suite `env` values format against the process environment; publish the
    # run's artifact directory so a template can name a per-run file under it
    # (the directory outlives every test process of the run).
    os.environ["LASH_RESTATE_SUITE_ARTIFACTS"] = str(artifacts)

    skips = list(suite.skips)
    every = list_tests(binary, cwd, suite.filters, skips)
    listed = getattr(args, "selected_tests", None)
    if listed is None:
        listed = list_tests(binary, cwd, args.only, skips) if args.only else every
    if not listed:
        log(f"{suite.name}: nothing matched {args.only or suite.filters}")
        return 1

    # A registry entry naming a law that no longer exists is an exemption
    # nobody can see; refuse it.
    stale = sorted(name for name in suite.replay_divergent if name not in every)
    if stale:
        for name in stale:
            print(f"STALE: {name} is named in {REGISTRY.relative_to(ROOT)} but is not a test of {suite.label}")
        return 1
    # Strict xfail: a law the replay leg holds back still runs, so a hold whose
    # divergence is fixed cannot hide -- a held law that passes fails the run.
    divergent = suite.replay_divergent if leg == "replay" and not args.include_divergent else {}
    held = [name for name in listed if name in divergent]
    to_run = list(listed)

    leg_config = {**LEGS[leg], **suite.leg_server_env.get(leg, {})}
    overrides = dict(assignment.split("=", 1) for assignment in args.server_env)
    shards: list[ShardPlan] = []
    shared: queue.Queue[str] = queue.Queue()
    for name in to_run:
        shared.put(name)
    for index in range(max(1, min(args.shards or suite.shards, len(to_run))) if to_run else 0):
        # A suite's own server settings refine the bounded policy: a suite
        # whose laws need an exhausted invocation paused, as Restate's default
        # policy leaves it, says so for its servers.
        shards.append(ShardPlan(f"{leg}-{index}", {**RETRIES_BOUNDED, **leg_config, **overrides}, shared))

    # Reserve every port the suite binds before the first consumer starts:
    # reservations stay bound until claimed, so no allocation of this run --
    # a server's listeners, another shard's endpoints -- can be handed a port
    # a sibling already claimed.
    for plan in shards:
        plan.server = RestateServer(name=f"{suite.name}-{plan.name}", workdir=artifacts, config=plan.config)
        plan.endpoints = {endpoint: ReservedPort() for endpoint in suite.endpoints}
    servers = [plan.server for plan in shards if plan.server is not None]

    timeout = args.timeout or suite.timeout_seconds
    log(
        f"{suite.name} {leg}: {len(to_run)} tests over {len(shards)} server(s), "
        f"{timeout:.0f}s without progress bounds each"
    )
    for name in held:
        print(f"HELD under replay (expected to diverge): {name}\n    {divergent[name]}")

    outcomes: list[Outcome] = []
    failures: list[str] = []
    lock = threading.Lock()

    def shard_main(index: int, plan: ShardPlan) -> None:
        assert plan.server is not None
        server = plan.server
        server.start()
        env = dict(os.environ)
        # Registry env values may name the shard and any variable the caller
        # exported, e.g. a per-shard database on the caller's server.
        env.update({key: value.format(shard=index, **os.environ) for key, value in suite.env.items()})
        env.update(server.env())
        env["LASH_RESTATE_SUITE_LEG"] = leg
        # Each shard's endpoints bind loopback ports of their own, reserved
        # before any server started so no other bind of this run holds them.
        for endpoint, reservation in plan.endpoints.items():
            port = reservation.claim()
            env[f"{endpoint}_BIND"] = f"127.0.0.1:{port}"
            env[f"{endpoint}_URL"] = f"http://127.0.0.1:{port}"
        while True:
            try:
                name = plan.tests.get_nowait()
            except queue.Empty:
                return
            log_path = artifacts / f"{name.replace('::', '__')}.log"
            law_started = time.monotonic()
            try:
                status, seconds = run_one(binary, cwd, name, env, timeout, log_path, suite.panic_gate)
            except RuntimeError:
                with lock:
                    outcomes.append(Outcome(name, plan.name, "teardown_failed", time.monotonic() - law_started, log_path))
                raise
            with lock:
                outcomes.append(Outcome(name, plan.name, status, seconds, log_path))
                if name in divergent and status != "leftovers":
                    mark = "HEALED" if status == "ok" else "held"
                else:
                    mark = {"ok": "ok", "failed": "FAILED", "panicked": "PANICKED", "timeout": "TIMED OUT",
                            "leftovers": "LEFTOVERS"}[status]
                print(f"[{len(outcomes)}/{len(to_run)}] {mark:9} {seconds:7.2f}s  {name}", flush=True)
                if status != "ok" and (name not in divergent or status == "leftovers"):
                    print(f"----- {name}: output tail -----\n{tail(log_path, args.tail_lines)}")
                    print(f"----- {server.name}: server log tail -----\n{tail(server.log_path, 40)}\n-----", flush=True)

    def guarded(index: int, plan: ShardPlan) -> None:
        try:
            shard_main(index, plan)
        except BaseException as error:  # noqa: BLE001 - reported below
            with lock:
                failures.append(f"{plan.name}: {error}")

    started = time.monotonic()
    threads = [threading.Thread(target=guarded, args=pair, daemon=True) for pair in enumerate(shards)]
    try:
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
    finally:
        for server in servers:
            server.stop()
        for plan in shards:
            for reservation in plan.endpoints.values():
                reservation.close()
    wall = time.monotonic() - started

    healed = [outcome for outcome in outcomes if outcome.name in divergent and outcome.status == "ok"]
    still_held = [outcome for outcome in outcomes if outcome.name in divergent
                  and outcome.status not in ("ok", "leftovers", "teardown_failed")]
    bad = [outcome for outcome in outcomes if outcome.status in ("leftovers", "teardown_failed")
           or (outcome.name not in divergent and outcome.status != "ok")]
    missing = sorted(set(to_run) - {outcome.name for outcome in outcomes})
    ordered = sorted(outcomes, key=lambda outcome: -outcome.seconds)
    summary = {
        "suite": suite.name,
        "leg": leg,
        "servers": len(shards),
        "wall_seconds": round(wall, 2),
        "tests": [
            {"name": o.name, "status": o.status, "seconds": round(o.seconds, 2), "shard": o.shard} for o in ordered
        ],
        "held": held,
        "healed": [outcome.name for outcome in healed],
        "not_run": missing,
    }
    (artifacts / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(
        f"\n{suite.name} {leg}: {len(outcomes) - len(bad) - len(still_held)}/{len(to_run)} ok in {wall:.1f}s "
        f"({sum(o.seconds for o in outcomes):.1f}s of test time on {len(shards)} server(s)); "
        f"{len(still_held)} still held, {len(healed)} held law(s) now pass"
    )
    print("slowest:")
    for outcome in ordered[:10]:
        print(f"  {outcome.seconds:7.2f}s  {outcome.status:7}  {outcome.name}")
    for failure in failures:
        print(f"SHARD FAILED: {failure}")
    for outcome in bad:
        print(f"{outcome.status.upper()}: {outcome.name} (log: {outcome.log})")
    for name in missing:
        print(f"NOT RUN: {name}")
    for outcome in healed:
        print(f"{outcome.name}: held law now passes: remove it from {divergence_shard(suite, outcome.name)}")
    if bad or missing or failures or healed:
        if failures or any(outcome.status == "leftovers" for outcome in bad):
            return 1
        reason = suite.report_only.get(leg)
        if reason is None:
            return 1
        # Report-only: visible in the log and as a CI annotation, never green
        # by omission, but not a failed run.
        count = len(bad) + len(missing) + len(failures) + len(healed)
        print(f"::warning title={suite.name} {leg} leg (report-only)::{count} law(s) failed; {reason}")
        return 0
    if not args.keep_test_logs:
        for outcome in outcomes:
            outcome.log.unlink(missing_ok=True)
    return 0


# ---------------------------------------------------------------------------
# Commands.
# ---------------------------------------------------------------------------
def command_serve(args: argparse.Namespace) -> int:
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        raise SystemExit("serve needs a command after --")
    workdir = Path(tempfile.mkdtemp(prefix="lash-restate-serve-"))
    overrides = dict(assignment.split("=", 1) for assignment in args.server_env)
    server = RestateServer(
        name=args.name or f"serve-{args.leg}",
        workdir=workdir,
        config={**LEGS[args.leg], **overrides},
        port_base=args.port_base,
    )

    # The server runs in a session of its own, so a signal to this process
    # group never reaches it: a SIGTERM (a CI cancel, `timeout`) has to unwind
    # through the `finally` below, which stops it, or it outlives the run.
    def terminated(signum: int, _frame: object) -> None:
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, terminated)
    try:
        server.start()
        env = dict(os.environ)
        env.update(server.env())
        env["LASH_RESTATE_SUITE_LEG"] = args.leg
        code = subprocess.call(command, env=env)
        if code != 0:
            print(f"----- restate-server log tail -----\n{tail(server.log_path, 60)}", file=sys.stderr)
        return code
    finally:
        server.stop()
        if args.keep_log:
            destination = Path(args.keep_log)
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(server.log_path, destination)
        shutil.rmtree(workdir, ignore_errors=True)


def remote_suite(suite: Suite, leg: str, args: argparse.Namespace) -> int:
    label = suite.label.split(":", 1)[0] + ":restate_" + suite.name.replace("-", "_") + "_" + leg
    command = [str(ROOT / "scripts/hermetic-build.sh"), "test", label,
               "--test-output-dir", args.artifacts]
    for selector in args.only:
        command.append(f"--test_arg={selector}")
    if args.shards or args.timeout or args.server_env or args.include_divergent:
        raise SystemExit("suite diagnostics need --binary; registered action settings come from the registry")
    return subprocess.call(command, cwd=ROOT)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command_name", required=True)

    sub.add_parser("server-path", help="print the pinned server binary, fetching it on first use")

    build_parser = sub.add_parser("build", help="build labels from the shared cache and print their outputs")
    build_parser.add_argument("labels", nargs="+")

    stage = sub.add_parser("stage-binaries", help="build a package's binaries and stage them under Cargo names")
    stage.add_argument("package", help="a Buck2 package, e.g. //runbooks/restate-postgres-workers")
    stage.add_argument("destination")

    serve = sub.add_parser("serve", help="run a command beside one server")
    serve.add_argument("--leg", choices=sorted(LEGS), default="live")
    serve.add_argument("--server-env", action="append", default=[], help="KEY=VALUE server override")
    serve.add_argument("--keep-log", help="copy the server log here on exit")
    serve.add_argument("--name", help="the server's name (its cluster name and log file); default serve-<leg>")
    serve.add_argument(
        "--port-base", type=int, help="bind ingress, admin and node on this port and the next two (a gate's block)"
    )
    serve.add_argument("command", nargs=argparse.REMAINDER)

    suite = sub.add_parser("suite", help="build and run a registered suite")
    suite.add_argument("name")
    suite.add_argument("--leg", choices=sorted(LEGS), required=True)
    suite.add_argument("--artifacts", default=str(ROOT / "target" / "restate-suites"))
    suite.add_argument("--keep-test-logs", action="store_true", help="retain passing per-test logs")
    suite.add_argument(
        "--binary",
        default=os.environ.get("LASH_RESTATE_SUITE_BINARY"),
        help="run this test binary instead of building the suite's label (env: LASH_RESTATE_SUITE_BINARY)",
    )
    suite.add_argument("--only", action="append", default=[], help="a test-name filter replacing the suite's")
    suite.add_argument("--shards", type=int, help="servers to shard over (default: the suite's)")
    suite.add_argument(
        "--timeout", type=float, help="per-test bound on time without progress, in seconds (default: the suite's)"
    )
    suite.add_argument(
        "--include-divergent",
        action="store_true",
        help="run the laws the replay leg holds back as ordinary gating tests "
        "(default: they run as expected failures, and a pass fails the run)",
    )
    suite.add_argument("--server-env", action="append", default=[], help="KEY=VALUE server override (diagnostics)")
    suite.add_argument("--tail-lines", type=int, default=60)

    args = parser.parse_args(argv)
    if args.command_name == "server-path":
        print(server_path())
        return 0
    if args.command_name == "build":
        for path in build(args.labels):
            print(path)
        return 0
    if args.command_name == "stage-binaries":
        for path in stage_binaries(args.package, Path(args.destination)):
            print(path)
        return 0
    if args.command_name == "serve":
        return command_serve(args)
    suite = load_suite(args.name)
    if args.binary or load_registry()[args.name].get("ci_driver"):
        return run_suite(suite, args.leg, args)
    return remote_suite(suite, args.leg, args)


if __name__ == "__main__":
    sys.exit(main())
