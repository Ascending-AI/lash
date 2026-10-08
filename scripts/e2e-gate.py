#!/usr/bin/env python3
"""Run one exact E2E registration in this fork's private Kiln gate.

A case boots its own lash nodes over the row's store; this script builds the
hosts, supplies PostgreSQL when the row needs it, runs the exact test and
splits the case's receipt into the role files `lash-e2e.py` reconciles.
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import time
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
# The real-host harness (crates/lash-e2e): every registered case is one test
# function of its one integration target.
LABELS: frozenset[str] = frozenset({"//crates/lash-e2e:e2e__test"})
WORKBENCH = "//examples/agent-workbench:agent-workbench"
WORKER = "//crates/lash-vm-worker:lash-vm-worker__bin"
LASHCTL = "//crates/lashctl:lashctl"
CONSUMER = "//examples/e2e-consumer:e2e-consumer"
RLM_HOST = "//runbooks/rlm-smoke:rlm-smoke"
INVENTORY = ROOT / "tools/buck2/target-inventory.json"


def write(path: Path, value: dict) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n")


def output(report: Path, label: str) -> str:
    return subprocess.check_output([
        "python3", str(ROOT / "tools/buck2/outputs.py"), "--report", str(report),
        "--label", label, "--single",
    ], cwd=ROOT, text=True).strip()


def feature_variant(units: list[dict], package: str, base: str, features: list[str]) -> str:
    matches = sorted({
        unit["label"] for unit in units
        if unit["package"] == package and unit["kind"] == "bin"
        and unit["features"] == features and unit["label"].startswith(base + "__fv_")
    })
    if len(matches) != 1:
        raise ValueError(f"{base} feature variant {features}: expected one match, got {matches}")
    return matches[0]


def test_counts(report: Path, label: str, name: str) -> dict:
    data = json.loads(report.read_text())
    if not data["session_complete"] or data["infrastructure_errors"]:
        raise ValueError("Kiln test session did not complete without infrastructure errors")
    results = list(data["results"].values())
    if len(results) != 1 or results[0]["label"] != "root" + label:
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
    return {"executed": 1, "passed": 1 - failed, "failed": failed,
            "junit_xml": result["outputs"]["junit_xml"]}


def hardlink(path: Path, destination: Path) -> None:
    try:
        os.link(path, destination)
    except OSError as error:
        if error.errno != errno.EXDEV:
            raise
        shutil.copy2(path, destination)


def certify_case(artifacts: Path, junit: Path, outputs: dict[str, Path], source: str,
                 case: str | None, leg: str, base_provenance: dict) -> dict:
    """Split the case's CaseReceipt into the role files reconcile consumes.

    The case reports the labelled commits it observed with the replay
    tripwire's counts, and every lash node it booted. A resume leg must show a
    node killed and its work resumed on another node.
    """
    errors: list[str] = []
    shutil.copy2(junit, artifacts / "junit.xml")
    evidence: dict = {}
    receipts = sorted((artifacts / "case").rglob("receipt.json"))
    if len(receipts) != 1:
        errors.append(f"expected one CaseReceipt under case/, found {len(receipts)}")
    else:
        try:
            evidence = json.loads(receipts[0].read_text())["case"]["evidence"]
            if not isinstance(evidence, dict):
                raise ValueError("receipt evidence is not an object")
        except (OSError, ValueError, KeyError, TypeError) as error:
            errors.append(f"unreadable CaseReceipt: {error}")
    commits = evidence.get("commits") or []
    tripwire = evidence.get("tripwire")
    nodes = evidence.get("nodes") or []
    cleanup = evidence.get("cleanup") or []
    if evidence:
        if commits and isinstance(tripwire, dict):
            write(artifacts / "commits.json", {"commits": commits, "tripwire": tripwire})
        else:
            errors.append("no labelled commits or replay tripwire evidence")
        names = {str(node.get("node")) for node in nodes}
        killed = {str(node.get("node")) for node in nodes if node.get("killed")}
        if not names:
            errors.append("no node evidence")
        elif leg == "resume" and not (killed and names - killed):
            errors.append("a resume leg needs a killed node and a node that resumed its work")
        write(artifacts / "store.json", {"stores": evidence.get("stores") or []})
        write(artifacts / "host.json", {
            "outputs": evidence.get("outputs") or [],
            "artifacts": evidence.get("artifacts") or [],
        })
        write(artifacts / "trace.json", {
            "barriers": evidence.get("barriers") or [],
            "faults": evidence.get("faults") or [],
            "effects": evidence.get("effects") or [],
        })
        write(artifacts / "cleanup.json", {
            "complete": bool(cleanup) and all(receipt.get("closed") for receipt in cleanup),
            "errors": [f"{receipt.get('resource')}: {receipt.get('detail')}"
                       for receipt in cleanup if not receipt.get("closed")]
                      + ([] if cleanup else ["no cleanup receipts"]),
            "remaining": [receipt.get("resource") for receipt in cleanup
                          if not receipt.get("closed")],
        })
    binaries: dict[str, dict] = {}
    binary_dir = artifacts / "binaries"
    binary_dir.mkdir(exist_ok=True)

    def descriptor(role: str, path: Path) -> dict:
        hardlink(path, binary_dir / role)
        sha = hashlib.sha256(path.read_bytes()).hexdigest()
        return {"source_sha": source,
                "artifact": {"path": f"binaries/{role}", "sha256": sha}}

    if evidence:
        artifact_shas = {
            artifact["sha256"] for artifact in evidence.get("artifacts") or []
        }
        hosts = {
            name: hashlib.sha256(outputs[name].read_bytes()).hexdigest()
            for name in ("workbench", "workbench_e2e", "node", "consumer", "rlm_host")
            if name in outputs
        }
        matched = sorted(name for name, sha in hosts.items() if sha in artifact_shas)
        if len({hosts[name] for name in matched}) == 1:
            binaries["host"] = descriptor("host", outputs[matched[0]])
        else:
            errors.append(f"expected one host artifact match, found {len({hosts[name] for name in matched})}")
        if "node_next" in outputs and \
                hashlib.sha256(outputs["node_next"].read_bytes()).hexdigest() in artifact_shas:
            binaries["synthetic_next_host"] = descriptor("synthetic_next_host", outputs["node_next"])
            binaries["operator"] = descriptor("operator", outputs["lashctl_n"])
            binaries["synthetic_next_operator"] = descriptor(
                "synthetic_next_operator", outputs["lashctl_next"])
            binaries["synthetic_next_vm_worker"] = descriptor(
                "synthetic_next_vm_worker", outputs["vm_worker_next"])
    binaries["vm_worker"] = descriptor("vm_worker", outputs["vm_worker"])
    provenance = {
        **base_provenance,
        "case": case,
        "nodes": len({str(node.get("node")) for node in nodes}) or None,
        "binaries": binaries,
        "evidence_error": "; ".join(errors) or None,
    }
    write(artifacts / "provenance.json", provenance)
    return provenance


def claim_port_block(locks: Path) -> tuple[int, int]:
    """Claim a free 50-port block of the 61000-65499 E2E space.

    The claim is an flock on the block's file in the shared gate state
    root: the same files the worktree gates lock, so a fork never picks
    a live block, and the OS releases the claim when this process exits,
    however it exits.
    """
    locks.mkdir(parents=True, exist_ok=True)
    for slot in range(90):
        path = locks / f"port-slot-{slot}.lock"
        fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error:
            os.close(fd)
            if error.errno not in (errno.EACCES, errno.EAGAIN):
                raise
            continue
        stat = Path("/proc/self/stat").read_text()
        os.ftruncate(fd, 0)
        os.write(fd, (
            f"battery=e2e-gate\npid={os.getpid()}\n"
            f"pid_start={stat.rpartition(')')[2].split()[19]}\n"
            f"started_at={time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}\n"
            f"worktree_slug={ROOT.name}\nworktree_root={ROOT}\n"
            f"scope=port-slot\nlock_path={path}\n"
        ).encode())
        return 61000 + slot * 50, fd
    raise ValueError("every E2E port block is claimed")


def scratch_dir(artifacts: Path) -> Path:
    """The run's TMPDIR: fixed-width for every test name and artifacts path.

    test_runner.py binds executor sockets under <TMPDIR>/lash-tests-*/;
    fleet hosts bind hashed control sockets directly under TMPDIR.
    An AF_UNIX path dies past sun_path (107 bytes). The artifacts path
    alone already exceeds that under the default layout, so TMPDIR can never
    be derived from it by concatenation: hash it instead.
    """
    return Path("/tmp") / f"lash-e2e-{hashlib.sha256(str(artifacts).encode()).hexdigest()[:8]}"


def run(label: str, name: str, artifacts: Path, case: str | None,
        store: str, leg: str, live_replay: str) -> int:
    gate = os.environ["KILN_GATE_ID"]
    # A case's nodes and fixtures take ports from this block; fleet
    # PostgreSQL owns offset 40.
    base, block_fd = claim_port_block(Path(os.environ.get(
        "LASH_GATE_STATE_ROOT", f"/tmp/lash-gate-{os.getuid()}")))
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
    # TMPDIR reaches the executor as a resolved --test_env value each run, so
    # a daemon that inherited an older artifacts path cannot pin a stale long
    # path for its sockets.
    scratch = scratch_dir(artifacts)
    shutil.rmtree(scratch, ignore_errors=True)
    scratch.mkdir(parents=True)
    env.update({
        "TMPDIR": str(scratch),
        "UV_CACHE_DIR": str(ROOT / "target/e2e-gate/uv-cache"),
        "UV_PYTHON_DOWNLOADS": "never",
        "PLAYWRIGHT_BROWSERS_PATH": str(Path.home() / ".cache/ms-playwright"),
    })
    try:
        units = json.loads(INVENTORY.read_text())["feature_lane_units"]
        workbench_e2e = feature_variant(units, "agent-workbench", WORKBENCH, ["e2e-tools"])
        worker_next = feature_variant(
            units, "lash-internal-vm-worker", WORKER, ["synthetic-next", "testing"])
        lashctl_n = feature_variant(units, "lashctl", LASHCTL, [])
        lashctl_next = feature_variant(units, "lashctl", LASHCTL, ["synthetic-next"])
        build = artifacts / "build.json"
        subprocess.run([
            "kiln", "build", WORKBENCH, workbench_e2e, WORKER,
            lashctl_n, lashctl_next, worker_next, CONSUMER, RLM_HOST, label,
            "--materializations", "final",
            "--target-platforms", "prelude//platforms:default",
            "--build-report", str(build),
        ], cwd=ROOT, env=env, check=True)
        outputs = {
            "workbench": Path(output(build, WORKBENCH)),
            "workbench_e2e": Path(output(build, workbench_e2e)),
            "lashctl_n": Path(output(build, lashctl_n)),
            "lashctl_next": Path(output(build, lashctl_next)),
            "consumer": Path(output(build, CONSUMER)),
            "rlm_host": Path(output(build, RLM_HOST)),
            "vm_worker": Path(output(build, WORKER)),
            "vm_worker_next": Path(output(build, worker_next)),
        }
        workbench = str(outputs["workbench"])
        worker = str(outputs["vm_worker"])
        python = ROOT / "target/e2e-gate/python/bin/python"
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
        source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
        env.update({
            "LASH_E2E_HOST_ARTIFACTS": str(artifacts / "case"),
            "LASH_E2E_ARTIFACT_DIR": str(artifacts / "case"),
            "LASH_PHASE_A_ARTIFACT_DIR": str(artifacts / "case"),
            "LASH_E2E_PORT_BASE": str(base),
            "LASH_E2E_HOST_PORT": str(base + 20),
            "LASH_E2E_CANDIDATE_SHA": source,
            "LASH_E2E_STORE": store,
            "LASH_E2E_LEG": leg,
            # The live replay store every workbench of the case runs on
            # unless the case names one (FIG-5101).
            "LASH_E2E_LIVE_REPLAY": live_replay,
            "LASH_E2E_REPO": str(ROOT),
            "LASH_E2E_PYTHON": str(python),
            "LASH_E2E_WORKBENCH_BIN": workbench,
            "LASH_E2E_WORKBENCH_SHA256": hashlib.sha256(Path(workbench).read_bytes()).hexdigest(),
            "LASH_WORKBENCH_E2E_BIN": str(outputs["workbench_e2e"]),
            "LASH_WORKBENCH_E2E_SHA256": hashlib.sha256(outputs["workbench_e2e"].read_bytes()).hexdigest(),
            "LASH_UPGRADE_LASHCTL_N": str(outputs["lashctl_n"]),
            "LASH_UPGRADE_LASHCTL_NEXT": str(outputs["lashctl_next"]),
            "LASH_E2E_CONSUMER_BIN": str(outputs["consumer"]),
            "LASH_E2E_CONSUMER_SHA256": hashlib.sha256(outputs["consumer"].read_bytes()).hexdigest(),
            "LASH_E2E_RLM_HOST_BIN": str(outputs["rlm_host"]),
            "LASH_E2E_RLM_HOST_SHA256": hashlib.sha256(outputs["rlm_host"].read_bytes()).hexdigest(),
            "LASH_VM_WORKER": worker,
        })
        (artifacts / "case").mkdir()
        report = artifacts / "test-report.json"
        command = [
            "kiln", "test", label, "--local-test-execution", "--no-test-cache",
            "--test_arg=--exact", f"--test_arg={name}", "--test_arg=--include-ignored",
            "--test_arg=--nocapture", "--test-report", str(report),
            "--test-output-dir", str(artifacts / "test-results"),
        ]
        # Kiln actions receive only explicitly forwarded runtime variables.
        keys = [key for key in env if key.startswith("LASH_E2E_")]
        keys += ["KILN_GATE_ID", "LASH_VM_WORKER",
                 "LASH_WORKBENCH_E2E_BIN", "LASH_WORKBENCH_E2E_SHA256",
                 "LASH_UPGRADE_LASHCTL_N", "LASH_UPGRADE_LASHCTL_NEXT",
                 "LASH_PHASE_A_ARTIFACT_DIR", "PLAYWRIGHT_BROWSERS_PATH", "TMPDIR"]
        # The paid live rows' provider credential and model reach the case only
        # when the operator supplies them; without them those rows record NotRun.
        keys += [key for key in ("OPENROUTER_API_KEY", "OPENROUTER_MODEL") if env.get(key)]
        postgres = store == "postgresql" or live_replay == "postgresql"
        if postgres:
            # with-service.sh exports the server address into the command's
            # environment; Kiln resolves this --test_env key against it.
            keys.append("LASH_POSTGRES_DATABASE_URL")
        command.extend(f"--test_env={key}" for key in sorted(keys))
        if postgres:
            command = [
                str(ROOT / "scripts/ci/with-service.sh"), "pg", "--", *command,
            ]
        code = subprocess.call(command, cwd=ROOT, env=env)
        counts = test_counts(report, label, name)
        junit = Path(counts.pop("junit_xml"))
        provenance = certify_case(artifacts, junit, outputs, source, case, leg, {
            "scenario": name, "label": label, "source_sha": source,
            "gate": gate, "port_base": base,
            "store": store, "leg": leg,
            "live_replay": live_replay,
            "playwright": "1.62.0",
            "workbench": {"path": workbench,
                          "sha256": env["LASH_E2E_WORKBENCH_SHA256"]},
        })
        write(artifacts / "execution.json", {
            "scenario": name, "label": label, "source_sha": source, "counts": counts,
            "store": store, "leg": leg,
            "exit_code": code, "evidence_error": provenance["evidence_error"],
            "artifacts": str(artifacts),
        })
        print(f"{name}: executed={counts['executed']} passed={counts['passed']} "
              f"failed={counts['failed']} artifacts={artifacts}", flush=True)
        return code if code else int(counts["failed"] != 0 or provenance["evidence_error"] is not None)
    finally:
        os.close(block_fd)
        shutil.rmtree(scratch, ignore_errors=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("label", choices=sorted(LABELS), help="registered test target label")
    parser.add_argument("test", help="full exact test path inside the label")
    parser.add_argument("--store", choices=("sqlite_memory", "sqlite_file", "postgresql"), required=True,
                        help="store the case runs its host over")
    parser.add_argument("--leg", choices=("live", "resume"), required=True,
                        help="live, or resume: kill a node and resume its work on another")
    parser.add_argument("--artifacts", type=Path, help="fresh directory inside this fork")
    parser.add_argument("--case", help="manifest scenario/variant/store/leg/channel key")
    parser.add_argument("--live-replay", choices=("memory", "postgresql"), default="memory",
                        help="live replay store the case's workbenches run on unless the case names one")
    args = parser.parse_args()
    if not re.fullmatch(r"[a-zA-Z0-9_:]+", args.test):
        parser.error("test must be a full test path")
    label_name = args.label.rsplit(":", 1)[-1]
    artifacts = (args.artifacts or ROOT / "target/e2e-gate" / label_name / args.test / str(time.time_ns())).resolve()
    if not artifacts.is_relative_to(ROOT):
        parser.error("artifacts must be inside this fork")
    if not os.environ.get("KILN_GATE_ID"):
        command = [
            "kiln", "gate", "lash", ROOT.name, "--", "python3", str(Path(__file__).resolve()),
            args.label, args.test, "--store", args.store, "--leg", args.leg,
            "--live-replay", args.live_replay, "--artifacts", str(artifacts),
        ]
        if args.case is not None:
            command += ["--case", args.case]
        os.execvp("kiln", command)
    if Path(os.environ.get("KILN_FORK_DIR", "")).resolve() != ROOT:
        parser.error("Kiln gate belongs to another fork")
    lock = ROOT / "target/e2e-gate/gate.lock"
    lock.parent.mkdir(parents=True, exist_ok=True)
    with lock.open("a") as lease:
        fcntl.flock(lease, fcntl.LOCK_EX)
        artifacts.mkdir(parents=True, exist_ok=False)
        print(f"e2e artifacts: {artifacts}", flush=True)
        try:
            return run(args.label, args.test, artifacts, args.case,
                       args.store, args.leg, args.live_replay)
        except (OSError, ValueError, KeyError, ET.ParseError, subprocess.CalledProcessError) as error:
            write(artifacts / "execution.json", {
                "scenario": args.test, "label": args.label,
                "counts": {"executed": 0, "passed": 0, "failed": 0},
                "evidence_error": None,
                "infrastructure_error": str(error), "artifacts": str(artifacts),
            })
            print(f"{args.test}: executed=0 passed=0 failed=0 "
                  f"infrastructure_error={error}; artifacts={artifacts}", file=sys.stderr)
            return 1


if __name__ == "__main__":
    sys.exit(main())
