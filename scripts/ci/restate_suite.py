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
* Two legs per suite. `live` runs Restate's defaults. `replay` sets the
  invoker's inactivity timeout to zero, so an invocation suspends at every
  await and every resumption replays its journal from the start: the leg that
  proves a handler deterministic (upstream's `alwaysSuspending` suites).
* Retries off: Restate's default policy retries a failing invocation 70 times
  over most of an hour, so a law whose handler failed for good used to hold
  the job until GitHub cancelled it. A shard's server kills a failed
  invocation on its first failure. The laws that exercise redelivery on
  purpose are named in the suite registry and run on a server that retries on
  a short, bounded schedule.
* Each test runs in its own process under a wall-clock bound, so a hung law
  fails in minutes with its own output and the server's log, and one law's
  process state cannot leak into the next law.

The suites themselves -- the Bazel label of the test binary, the filters, the
endpoints each shard binds and the redelivery laws -- live in
`scripts/restate-suites.toml`; the replay leg's known divergences live one
ticket per file under `scripts/restate-divergences/`.

Usage:
  restate_suite.py suite <name> --leg live|replay [--artifacts DIR]
      Build the suite's test binary on the shared build pool, then run it.
  restate_suite.py serve [--leg live] -- <command...>
      Run <command> beside one server, with RESTATE_INGRESS_URL and
      RESTATE_ADMIN_URL exported (for drivers that own their test processes).
  restate_suite.py build <label>...
      Build Bazel labels from the shared cache and print each output path.
  restate_suite.py stage-binaries <package> <dir>
      Build every Rust binary of a Bazel package from the shared cache and
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
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

ROOT = Path(__file__).resolve().parents[2]
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
RETRIES_OFF = {
    "RESTATE_DEFAULT_RETRY_POLICY__MAX_ATTEMPTS": "1",
    "RESTATE_DEFAULT_RETRY_POLICY__ON_MAX_ATTEMPTS": "kill",
}
# For the laws that crash a handler attempt on purpose: redelivery within tens
# of milliseconds, and a permanently failing invocation killed in about half a
# minute instead of retried for most of an hour.
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
def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def http_ok(url: str) -> bool:
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(url, timeout=2) as response:
            return 200 <= response.status < 300
    except OSError:
        return False


def tail(path: Path, lines: int) -> str:
    try:
        content = path.read_text(errors="replace").splitlines()
    except OSError:
        return "(no log)"
    return "\n".join(content[-lines:])


class RestateServer:
    def __init__(self, name: str, workdir: Path, config: dict[str, str]) -> None:
        self.name = name
        self.workdir = workdir
        self.config = config
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
        binary = server_path()
        self.workdir.mkdir(parents=True, exist_ok=True)
        # A short scratch path of its own, kept out of the artifact tree.
        self.data_dir = tempfile.mkdtemp(prefix="lash-restate-")
        self.ingress_port = free_port()
        self.admin_port = free_port()
        env = {key: value for key, value in os.environ.items() if key not in PROXY_VARIABLES}
        env.update(BASE_SERVER_ENV)
        env.update(self.config)
        env.update(
            {
                "RESTATE_BASE_DIR": self.data_dir,
                "RESTATE_NODE_NAME": "n1",
                "RESTATE_CLUSTER_NAME": f"lash-{self.name}",
                "RESTATE_BIND_PORT": str(free_port()),
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
        for url in (f"{self.admin_url}/health", f"{self.ingress_url}/restate/health"):
            while not http_ok(url):
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
            if self.data_dir:
                shutil.rmtree(self.data_dir, ignore_errors=True)

    def env(self) -> dict[str, str]:
        return {"RESTATE_INGRESS_URL": self.ingress_url, "RESTATE_ADMIN_URL": self.admin_url}


# ---------------------------------------------------------------------------
# Building a test binary from the shared cache.
# ---------------------------------------------------------------------------
def build(labels: Sequence[str]) -> list[Path]:
    """Build labels on the shared pool and return their output files.

    CI names its cache and output base through the bazel-shared-cache action;
    a Kiln fork builds through `kiln`; anything else uses the checkout's
    `--config=shared`. Only the top-level outputs are downloaded.
    """
    flags = ["--remote_download_outputs=toplevel"]
    if os.environ.get("GITHUB_ACTIONS"):
        shared = os.environ.get("BAZEL_SHARED_CACHE_FLAGS")
        root = os.environ.get("BAZEL_OUTPUT_USER_ROOT")
        if not shared or not root:
            raise SystemExit("CI must configure the shared build cache before a Restate suite builds")
        argv = ["bazel", f"--output_user_root={root}", "build", *shared.split(), *flags, *labels]
    elif os.environ.get("KILN_REPO") and shutil.which("kiln"):
        argv = ["kiln", "build", *flags, *labels]
    else:
        argv = ["bazel", "build", "--config=shared", *flags, *labels]
    log(f"building {' '.join(labels)}")
    subprocess.run(argv, cwd=ROOT, check=True, stdout=sys.stderr)
    bazel_bin = (ROOT / "bazel-bin").resolve()
    outputs = []
    for label in labels:
        package, _, name = label.removeprefix("//").partition(":")
        path = bazel_bin / package / name
        if not path.is_file():
            raise SystemExit(f"{label} built, but {path} is missing")
        outputs.append(path)
    return outputs


def package_binaries(package: str) -> list[str]:
    """The labels of a package's Rust binaries, from its generated BUILD file."""
    build_file = ROOT / package.removeprefix("//") / "BUILD.bazel"
    names = re.findall(r'^lash_rust_binary\(\n    name = "([^"]+)",$', build_file.read_text(), flags=re.MULTILINE)
    if not names:
        raise SystemExit(f"{build_file.relative_to(ROOT)} declares no lash_rust_binary")
    return [f"{package}:{name}" for name in names]


def stage_binaries(package: str, destination: Path) -> list[Path]:
    """Build a package's binaries and stage them under their Cargo names.

    A generated binary label is `<cargo-name>__bin`; consumers (compose files,
    the E2E drivers) mount the Cargo name. Each is stripped, as Cargo's
    release profile strips them: the stage is shipped between jobs, and an
    unstripped fastbuild binary is roughly twice the size.
    """
    labels = package_binaries(package)
    destination.mkdir(parents=True, exist_ok=True)
    staged = []
    for built in build(labels):
        target = destination / built.name.removesuffix("__bin")
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
    parked_crate: str | None
    endpoints: tuple[str, ...]
    env: dict[str, str]
    shards: int
    timeout_seconds: float
    redelivery_laws: tuple[str, ...]
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
        parked_crate=raw.get("parked_crate"),
        endpoints=tuple(raw.get("endpoints", ())),
        env=dict(raw.get("env", {})),
        shards=int(raw.get("shards", 1)),
        timeout_seconds=float(raw.get("timeout_seconds", 300)),
        redelivery_laws=tuple(raw.get("redelivery_laws", ())),
        panic_gate=bool(raw.get("panic_gate", False)),
        leg_server_env={leg: dict(raw.get(leg, {}).get("server_env", {})) for leg in LEGS},
        replay_divergent=dict(raw.get("replay", {}).get("divergent", {})),
        report_only={
            leg: raw[leg]["report_only"] for leg in LEGS if isinstance(raw.get(leg), dict) and "report_only" in raw[leg]
        },
    )


def parked_skips(crate: str) -> list[str]:
    """Deferred-law invocations parked in scripts/deferred-laws/ shards."""
    output = subprocess.run(
        [sys.executable, str(ROOT / "scripts" / "check_law_execution_receipts.py"), "--parked-skips", crate],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.split()
    return [value for value in output if value != "--skip"]


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
@dataclass
class Outcome:
    name: str
    shard: str
    status: str  # ok | failed | panicked | timeout
    seconds: float
    log: Path


@dataclass
class ShardPlan:
    name: str
    config: dict[str, str]
    tests: "queue.Queue[str]"


def run_one(
    binary: Path, cwd: Path, name: str, env: dict[str, str], timeout: float, log_path: Path, panic_gate: bool
) -> tuple[str, float]:
    argv = [str(binary), name, "--exact", "--ignored", "--nocapture", "--test-threads=1"]
    started = time.monotonic()
    with log_path.open("wb") as out:
        process = subprocess.Popen(argv, cwd=cwd, env=env, stdout=out, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            return "timeout", time.monotonic() - started
    elapsed = time.monotonic() - started
    output = log_path.read_text(errors="replace")
    # A name that matched nothing exits 0; a law that ran says so.
    if code != 0 or "test result: ok. 1 passed" not in output:
        return "failed", elapsed
    # A panic on a background task can leave the law itself green.
    if panic_gate and "panicked at" in output:
        return "panicked", elapsed
    return "ok", elapsed


def run_suite(suite: Suite, leg: str, args: argparse.Namespace) -> int:
    if args.binary:
        binary = Path(args.binary).resolve()
    else:
        (binary,) = build([suite.label])
    cwd = ROOT / suite.cwd
    artifacts = Path(args.artifacts).resolve() / f"{suite.name}-{leg}"
    shutil.rmtree(artifacts, ignore_errors=True)
    artifacts.mkdir(parents=True)

    skips = list(suite.skips)
    if suite.parked_crate:
        skips += parked_skips(suite.parked_crate)
    every = list_tests(binary, cwd, suite.filters, skips)
    listed = list_tests(binary, cwd, args.only, skips) if args.only else every
    if not listed:
        log(f"{suite.name}: nothing matched {args.only or suite.filters}")
        return 1

    # A registry entry naming a law that no longer exists is an exemption
    # nobody can see; refuse it.
    stale = sorted(name for name in (*suite.replay_divergent, *suite.redelivery_laws) if name not in every)
    if stale:
        for name in stale:
            print(f"STALE: {name} is named in {REGISTRY.relative_to(ROOT)} but is not a test of {suite.label}")
        return 1
    divergent = suite.replay_divergent if leg == "replay" and not args.include_divergent else {}
    held_back = [name for name in listed if name in divergent]
    to_run = [name for name in listed if name not in divergent]
    redelivery = [name for name in to_run if name in suite.redelivery_laws]
    plain = [name for name in to_run if name not in suite.redelivery_laws]

    leg_config = {**LEGS[leg], **suite.leg_server_env.get(leg, {})}
    overrides = dict(assignment.split("=", 1) for assignment in args.server_env)
    shards: list[ShardPlan] = []
    shared: queue.Queue[str] = queue.Queue()
    for name in plain:
        shared.put(name)
    for index in range(max(1, min(args.shards or suite.shards, len(plain))) if plain else 0):
        shards.append(ShardPlan(f"{leg}-{index}", {**leg_config, **RETRIES_OFF, **overrides}, shared))
    if redelivery:
        own: queue.Queue[str] = queue.Queue()
        for name in redelivery:
            own.put(name)
        shards.append(ShardPlan(f"{leg}-redelivery", {**leg_config, **RETRIES_BOUNDED, **overrides}, own))

    timeout = args.timeout or suite.timeout_seconds
    log(
        f"{suite.name} {leg}: {len(to_run)} tests over {len(shards)} server(s) "
        f"({len(redelivery)} with redelivery), {timeout:.0f}s bound each"
    )
    for name in held_back:
        print(f"HELD BACK under replay: {name}\n    {divergent[name]}")

    outcomes: list[Outcome] = []
    failures: list[str] = []
    lock = threading.Lock()
    servers: list[RestateServer] = []

    def shard_main(index: int, plan: ShardPlan) -> None:
        server = RestateServer(name=f"{suite.name}-{plan.name}", workdir=artifacts, config=plan.config)
        with lock:
            servers.append(server)
        server.start()
        env = dict(os.environ)
        # Registry env values may name the shard and any variable the caller
        # exported, e.g. a per-shard database on the caller's server.
        env.update({key: value.format(shard=index, **os.environ) for key, value in suite.env.items()})
        env.update(server.env())
        env["LASH_RESTATE_SUITE_LEG"] = leg
        # Each shard's endpoints bind loopback ports of their own.
        for endpoint in suite.endpoints:
            port = free_port()
            env[f"{endpoint}_BIND"] = f"127.0.0.1:{port}"
            env[f"{endpoint}_URL"] = f"http://127.0.0.1:{port}"
        while True:
            try:
                name = plan.tests.get_nowait()
            except queue.Empty:
                return
            log_path = artifacts / f"{name.replace('::', '__')}.log"
            status, seconds = run_one(binary, cwd, name, env, timeout, log_path, suite.panic_gate)
            with lock:
                outcomes.append(Outcome(name, plan.name, status, seconds, log_path))
                mark = {"ok": "ok", "failed": "FAILED", "panicked": "PANICKED", "timeout": "TIMED OUT"}[status]
                print(f"[{len(outcomes)}/{len(to_run)}] {mark:9} {seconds:7.2f}s  {name}", flush=True)
                if status != "ok":
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
    wall = time.monotonic() - started

    bad = [outcome for outcome in outcomes if outcome.status != "ok"]
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
        "held_back": held_back,
        "not_run": missing,
    }
    (artifacts / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(
        f"\n{suite.name} {leg}: {len(outcomes) - len(bad)}/{len(to_run)} ok in {wall:.1f}s "
        f"({sum(o.seconds for o in outcomes):.1f}s of test time on {len(shards)} server(s)); "
        f"{len(held_back)} held back"
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
    if bad or missing or failures:
        reason = suite.report_only.get(leg)
        if reason is None:
            return 1
        # Report-only: visible in the log and as a CI annotation, never green
        # by omission, but not a failed run.
        count = len(bad) + len(missing) + len(failures)
        print(f"::warning title={suite.name} {leg} leg (report-only)::{count} law(s) failed; {reason}")
        return 0
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
    server = RestateServer(name=f"serve-{args.leg}", workdir=workdir, config={**LEGS[args.leg], **overrides})

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


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command_name", required=True)

    sub.add_parser("server-path", help="print the pinned server binary, fetching it on first use")

    build_parser = sub.add_parser("build", help="build labels from the shared cache and print their outputs")
    build_parser.add_argument("labels", nargs="+")

    stage = sub.add_parser("stage-binaries", help="build a package's binaries and stage them under Cargo names")
    stage.add_argument("package", help="a Bazel package, e.g. //runbooks/restate-postgres-workers")
    stage.add_argument("destination")

    serve = sub.add_parser("serve", help="run a command beside one server")
    serve.add_argument("--leg", choices=sorted(LEGS), default="live")
    serve.add_argument("--server-env", action="append", default=[], help="KEY=VALUE server override")
    serve.add_argument("--keep-log", help="copy the server log here on exit")
    serve.add_argument("command", nargs=argparse.REMAINDER)

    suite = sub.add_parser("suite", help="build and run a registered suite")
    suite.add_argument("name")
    suite.add_argument("--leg", choices=sorted(LEGS), required=True)
    suite.add_argument("--artifacts", default=str(ROOT / "target" / "restate-suites"))
    suite.add_argument(
        "--binary",
        default=os.environ.get("LASH_RESTATE_SUITE_BINARY"),
        help="run this test binary instead of building the suite's label (env: LASH_RESTATE_SUITE_BINARY)",
    )
    suite.add_argument("--only", action="append", default=[], help="a test-name filter replacing the suite's")
    suite.add_argument("--shards", type=int, help="servers to shard over (default: the suite's)")
    suite.add_argument("--timeout", type=float, help="per-test bound in seconds (default: the suite's)")
    suite.add_argument("--include-divergent", action="store_true", help="also run the laws the replay leg holds back")
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
    return run_suite(load_suite(args.name), args.leg, args)


if __name__ == "__main__":
    sys.exit(main())
