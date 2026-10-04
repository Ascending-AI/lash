#!/usr/bin/env python3
"""Run one exact workbench browser scenario in this fork's private Kiln gate."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import time
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
LABEL = "//crates/lash-upgrade-harness:e2e_hosts__test"
WORKBENCH = "//examples/agent-workbench:agent-workbench"
WORKER = "//crates/lash-vm-worker:lash-vm-worker__bin"
SERVER = "native//:restate"
RESTATE = ROOT / "scripts/ci/restate_suite.py"


def write(path: Path, value: dict) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n")


def output(report: Path, label: str) -> str:
    return subprocess.check_output([
        "python3", str(ROOT / "tools/buck2/outputs.py"), "--report", str(report),
        "--label", label, "--single",
    ], cwd=ROOT, text=True).strip()


def test_counts(report: Path, name: str) -> dict:
    data = json.loads(report.read_text())
    if not data["session_complete"] or data["infrastructure_errors"]:
        raise ValueError("Kiln test session did not complete without infrastructure errors")
    results = list(data["results"].values())
    if len(results) != 1 or results[0]["label"] != "root" + LABEL:
        raise ValueError("Kiln report does not name exactly the requested target")
    result = results[0]
    if result["status"] not in {"PASS", "FAIL"}:
        raise ValueError(f"Kiln test infrastructure status: {result['status']}")
    cases = list(ET.parse(result["outputs"]["junit_xml"]).getroot().iter("testcase"))
    if len(cases) != 1 or cases[0].get("name") != name or cases[0].find("skipped") is not None:
        raise ValueError("Kiln JUnit did not execute exactly the requested scenario")
    failed = int(any(cases[0].find(tag) is not None for tag in ("failure", "error")))
    if (result["status"] == "PASS") != (failed == 0):
        raise ValueError("Kiln status and JUnit verdict disagree")
    return {"executed": 1, "passed": 1 - failed, "failed": failed}


def run(name: str, artifacts: Path) -> int:
    gate = os.environ["KILN_GATE_ID"]
    # S28 owns a second cluster in this block; serve owns offsets 40–42.
    base = 61000 + (int(hashlib.sha256(gate.encode()).hexdigest()[:8], 16) % 89) * 50
    reservations = []
    try:
        for port in range(base, base + 50):
            reservation = socket.socket()
            reservations.append(reservation)
            reservation.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            reservation.bind(("127.0.0.1", port))
    except OSError as error:
        raise ValueError(f"gate {gate} port block {base} is occupied: {error}") from error
    finally:
        for reservation in reservations:
            reservation.close()

    env = dict(os.environ)
    scratch = artifacts / "tmp"
    scratch.mkdir()
    env.update({
        "TMPDIR": str(scratch),
        "UV_CACHE_DIR": str(ROOT / "target/workbench-e2e/uv-cache"),
        "UV_PYTHON_DOWNLOADS": "never",
        "PLAYWRIGHT_BROWSERS_PATH": str(Path.home() / ".cache/ms-playwright"),
    })
    build = artifacts / "build.json"
    subprocess.run([
        "kiln", "build", WORKBENCH, WORKER, SERVER, LABEL, "--materializations", "final",
        "--target-platforms", "prelude//platforms:default",
        "--build-report", str(build),
    ], cwd=ROOT, env=env, check=True)
    workbench = output(build, WORKBENCH)
    worker = output(build, WORKER)
    python = ROOT / "target/workbench-e2e/python/bin/python"
    if not python.exists():
        subprocess.run([
            "uv", "venv", "--python", sys.executable, str(python.parent.parent),
        ], cwd=ROOT, env=env, check=True)
    subprocess.run([
        "uv", "pip", "install", "--python", str(python), "playwright==1.62.0",
    ], cwd=ROOT, env=env, check=True)
    subprocess.run([
        str(python), "-c",
        "from importlib.metadata import version; from pathlib import Path; "
        "from playwright.sync_api import sync_playwright\n"
        "assert version('playwright') == '1.62.0'\n"
        "with sync_playwright() as p:\n"
        "    assert Path(p.chromium.executable_path).is_file(), 'Chromium missing from ~/.cache/ms-playwright'\n"
        "    browser = p.chromium.launch(headless=True)\n"
        "    browser.close()\n",
    ], cwd=ROOT, env=env, check=True)
    # Read the frozen current epoch rather than inventing a runner generation.
    epochs = (ROOT / "crates/lash-restate/src/process/admission.rs").read_text()
    generation = re.search(r"pub const JOURNAL_LOGIC_EPOCH: u32 = (\d+);", epochs).group(1)
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    server = output(build, SERVER)
    env.update({
        "LASH_E2E_HOST_ARTIFACTS": str(artifacts / "case"),
        "LASH_E2E_ARTIFACT_DIR": str(artifacts / "case"),
        "LASH_E2E_PORT_BASE": str(base),
        "LASH_E2E_HOST_PORT": str(base + 20),
        "LASH_E2E_CANDIDATE_SHA": source,
        "LASH_E2E_HOST_GENERATION": generation,
        "LASH_E2E_REPO": str(ROOT),
        "LASH_E2E_PYTHON": str(python),
        "LASH_E2E_WORKBENCH_BIN": workbench,
        "LASH_E2E_WORKBENCH_SHA256": hashlib.sha256(Path(workbench).read_bytes()).hexdigest(),
        "LASH_RESTATE_SERVER_BIN": server,
        "LASH_VM_WORKER": worker,
    })
    (artifacts / "case").mkdir()
    report = artifacts / "test-report.json"
    command = [
        "kiln", "test", LABEL, "--local-test-execution", "--no-test-cache",
        "--test_arg=--exact", f"--test_arg={name}", "--test_arg=--ignored",
        "--test_arg=--nocapture", "--test-report", str(report),
        "--test-output-dir", str(artifacts / "test-results"),
    ]
    # Kiln actions receive only explicitly forwarded runtime variables. serve
    # fills the endpoint URLs before Kiln resolves these two --test_env keys.
    keys = [key for key in env if key.startswith("LASH_E2E_")]
    keys += ["KILN_GATE_ID", "LASH_RESTATE_SERVER_BIN", "LASH_VM_WORKER",
             "PLAYWRIGHT_BROWSERS_PATH", "TMPDIR", "RESTATE_INGRESS_URL", "RESTATE_ADMIN_URL"]
    command.extend(f"--test_env={key}" for key in sorted(keys))
    write(artifacts / "provenance.json", {
        "scenario": name, "source_sha": source, "gate": gate, "port_base": base,
        "generation": generation, "playwright": "1.62.0",
        "workbench": {"path": workbench, "sha256": env["LASH_E2E_WORKBENCH_SHA256"]},
        "server": {"path": server, "sha256": hashlib.sha256(Path(server).read_bytes()).hexdigest()},
    })
    code = subprocess.call([
        "python3", str(RESTATE), "serve", "--name", f"workbench-{gate}",
        "--port-base", str(base + 40), "--keep-log", str(artifacts / "restate.log"),
        "--", *command,
    ], cwd=ROOT, env=env)
    counts = test_counts(report, name)
    write(artifacts / "execution.json", {
        "scenario": name, "source_sha": source, "counts": counts,
        "exit_code": code, "artifacts": str(artifacts),
    })
    print(f"{name}: executed={counts['executed']} passed={counts['passed']} "
          f"failed={counts['failed']} artifacts={artifacts}", flush=True)
    return code if code else int(counts["failed"] != 0)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scenario", help="full exact test name in e2e_hosts")
    parser.add_argument("--artifacts", type=Path, help="fresh directory inside this fork")
    args = parser.parse_args()
    if not re.fullmatch(r"[a-zA-Z0-9_:]+", args.scenario):
        parser.error("scenario must be a full test path")
    artifacts = (args.artifacts or ROOT / "target/workbench-e2e" / args.scenario / str(time.time_ns())).resolve()
    if not artifacts.is_relative_to(ROOT):
        parser.error("artifacts must be inside this fork")
    if not os.environ.get("KILN_GATE_ID"):
        os.execvp("kiln", [
            "kiln", "gate", "lash", ROOT.name, "--", "python3", str(Path(__file__).resolve()),
            args.scenario, "--artifacts", str(artifacts),
        ])
    if Path(os.environ.get("KILN_FORK_DIR", "")).resolve() != ROOT:
        parser.error("Kiln gate belongs to another fork")
    lock = ROOT / "target/workbench-e2e/gate.lock"
    lock.parent.mkdir(parents=True, exist_ok=True)
    with lock.open("a") as lease:
        fcntl.flock(lease, fcntl.LOCK_EX)
        artifacts.mkdir(parents=True, exist_ok=False)
        print(f"workbench artifacts: {artifacts}", flush=True)
        try:
            return run(args.scenario, artifacts)
        except (OSError, ValueError, KeyError, ET.ParseError, subprocess.CalledProcessError) as error:
            write(artifacts / "execution.json", {
                "scenario": args.scenario, "counts": {"executed": 0, "passed": 0, "failed": 0},
                "infrastructure_error": str(error), "artifacts": str(artifacts),
            })
            print(f"{args.scenario}: executed=0 passed=0 failed=0 "
                  f"infrastructure_error={error}; artifacts={artifacts}", file=sys.stderr)
            return 1


if __name__ == "__main__":
    sys.exit(main())
