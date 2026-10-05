#!/usr/bin/env python3
"""Plan real-host E2E cases and reconcile their exact-source evidence.

This entrypoint owns selection and receipts, never service supervision. Held
registrations are visible in plans and refuse execution and certification.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "scripts/lash-e2e-manifest.json"
SMOKE = {"S01", "S02", "S17", "S18", "S26", "S30"}
AUDITS = {"F04", "Z0A", "Z0P", "Z01", "Z02", "Z03", "Z04", "Z05"}
GATES = {"phase_a", "facade", "schema"}
COUNTS = ("selected", "executed", "passed", "failed", "not_run")
RUNNER = "scripts/e2e-gate.py"
SPEC = importlib.util.spec_from_file_location("e2e_gate", ROOT / RUNNER)
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)
SHA = re.compile(r"[0-9a-f]{40}")
DIGEST = re.compile(r"[0-9a-f]{64}")
AXES = ("scenario", "variant", "store", "leg", "channel")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def case_key(row: dict) -> str:
    return "/".join(row[axis] for axis in AXES)


def load_manifest(path: Path = MANIFEST) -> dict:
    manifest = json.loads(path.read_text())
    require(set(manifest) == {"scenarios", "server", "release_audits", "release_gates"}, "invalid manifest fields")
    server = manifest["server"]
    require(server["protocol"] == "V7" and DIGEST.fullmatch(server["archive_sha256"]) is not None, "invalid server pin")
    require(server["executable_sha256"] is None or DIGEST.fullmatch(server["executable_sha256"]) is not None, "invalid server executable pin")
    scenarios = manifest["scenarios"]
    require(isinstance(scenarios, list), "scenarios must be a list")
    ids = [row["id"] for row in scenarios]
    require(len(ids) == len(set(ids)) and set(ids) == {f"S{i:02}" for i in range(1, 38)}, "manifest must name S01–S37 exactly once")
    require(set(manifest["release_audits"]) == AUDITS, "release audits are incomplete")
    require(set(manifest["release_gates"]) == GATES, "release gates are incomplete")
    for scenario in scenarios:
        require(bool(scenario["rules"]) or bool(scenario["risks"]), f"{scenario['id']}: missing named rule/risk")
        require(bool(scenario["cases"]), f"{scenario['id']}: no permutations")
        for guard in scenario["arc_guards"]:
            require(set(guard) == {"ticket", "commit"} and re.fullmatch(r"FIG-\d+", guard["ticket"]), "invalid arc guard")
            require(guard["commit"] is None or SHA.fullmatch(guard["commit"]) is not None, "invalid arc landing commit")
        keys = []
        for case in scenario["cases"]:
            key = case_key({"scenario": scenario["id"], **case})
            keys.append(key)
            require(case["store"] in {"sqlite_memory", "sqlite_file", "postgresql"}, f"{key}: invalid store")
            require(case["leg"] in {"live", "replay"}, f"{key}: invalid leg")
            require(case["channel"] in {"standard", "rlm"}, f"{key}: invalid channel")
            tiers = case["tiers"]
            require(bool(tiers) and set(tiers) <= {"smoke", "full", "release", "live"}, f"{key}: invalid tiers")
            require("full" not in tiers or "release" in tiers, f"{key}: full row missing release")
            require(case["state"] in {"held", "ready"}, f"{key}: invalid state")
            registration = case["registration"]
            if case["state"] == "held":
                require(bool(case["hold_reason"]) and registration is None, f"{key}: held row needs reason and no registration")
            else:
                require(not case["hold_reason"] and isinstance(registration, dict), f"{key}: ready row needs registration")
                require(set(registration) == {"label", "test"}, f"{key}: invalid registration fields")
                require(registration["label"] in GATE.LABELS, f"{key}: unknown runner label")
                require(bool(registration["test"]) and not registration["test"].startswith("-") and not any(c.isspace() for c in registration["test"]), f"{key}: need full test path")
            require({"journal", "store", "host", "trace", "cleanup", "junit", "provenance"} <= set(case["artifacts"]), f"{key}: incomplete required artifacts")
        require(len(keys) == len(set(keys)), f"{scenario['id']}: duplicate permutation")
    smoke = select(manifest, "smoke", [])
    require(len(smoke) == 6 and {r['scenario'] for r in smoke} == SMOKE, "smoke must select exactly its six scenarios")
    require(all(r["leg"] == "live" and r["store"] == ("sqlite_file" if r["scenario"] in {"S01", "S02", "S17", "S18", "S26"} else "sqlite_memory") for r in smoke), "smoke needs live SQLite rows; S01/S02/S17/S18/S26 persisted")
    deterministic = {f"S{i:02}" for i in range(1, 35)} | {"S37"}
    for tier in ("full", "release"):
        require({r["scenario"] for r in select(manifest, tier, [])} == deterministic, f"{tier}: deterministic catalogue incomplete")
    require({r["scenario"] for r in select(manifest, "live", [])} == {"S35", "S36"}, "live catalogue incomplete")
    return manifest


def select(manifest: dict, tier: str, scenarios: list[str]) -> list[dict]:
    require(tier in {"smoke", "full", "release", "live"}, "unknown tier")
    require(len(scenarios) == len(set(scenarios)), "duplicate scenario selector")
    available = {r["id"] for r in manifest["scenarios"]}
    require(set(scenarios) <= available, "unknown scenario selector")
    rows = [
        {"scenario": scenario["id"], "arc_guards": scenario["arc_guards"], **case}
        for scenario in manifest["scenarios"]
        for case in scenario["cases"]
        if tier in case["tiers"] and (not scenarios or scenario["id"] in scenarios)
    ]
    require(bool(rows), "zero selected cases")
    require(not set(scenarios) - {r["scenario"] for r in rows}, "selector absent from tier")
    return sorted(rows, key=case_key)


def ancestor(commit: str, source: str) -> bool:
    return subprocess.run(["git", "merge-base", "--is-ancestor", commit, source], cwd=ROOT, check=False).returncode == 0


def plan(manifest: dict, manifest_sha: str, tier: str, scenarios: list[str], source: str, cases: list[str] | None = None, ready: bool = False) -> dict:
    require(SHA.fullmatch(source) is not None, "source must be an exact commit SHA")
    require(tier != "release" or not (scenarios or cases or ready), "release certification cannot select a subset")
    rows = select(manifest, tier, scenarios)
    if cases:
        require(len(cases) == len(set(cases)), "duplicate case selector")
        require(set(cases) <= {case_key(row) for row in rows}, "case selector absent from tier/scenarios")
        rows = [row for row in rows if case_key(row) in cases]
    excluded_held = [case_key(r) for r in rows if r["state"] == "held"] if ready else []
    if ready:
        rows = [r for r in rows if r["state"] != "held"]
        require(bool(rows), "zero selected cases")
    return {"source_sha": source, "manifest_sha256": manifest_sha, "tier": tier,
            "selectors": scenarios, "selected": len(rows), "cases": rows,
            "guarded": [case_key(r) for r in rows if any(g["commit"] is None or not ancestor(g["commit"], source) or not ancestor(g["commit"], "origin/main") for g in r["arc_guards"])],
            "excluded_held": excluded_held, "tier_complete": not excluded_held,
            "held": [case_key(r) for r in rows if r["state"] == "held"]}


def artifact(root: Path, item: dict) -> Path:
    require(set(item) == {"path", "sha256"}, "invalid artifact descriptor")
    relative = Path(item["path"])
    require(not relative.is_absolute() and ".." not in relative.parts, "artifact path must remain inside receipt directory")
    path = (root / relative).resolve()
    require(path.is_relative_to(root.resolve()) and path.is_file(), f"missing artifact: {relative}")
    require(DIGEST.fullmatch(item["sha256"]) is not None and digest(path) == item["sha256"], f"artifact digest mismatch: {relative}")
    return path


def counts(rows: list[dict]) -> dict:
    return {"selected": len(rows), "executed": sum(r["executed"] for r in rows),
            "passed": sum(r["status"] == "passed" for r in rows),
            "failed": sum(r["status"] == "failed" for r in rows),
            "not_run": sum(r["status"] == "not_run" for r in rows)}


def check_counts(reported: dict, actual: dict, label: str) -> None:
    require(isinstance(reported, dict) and set(reported) == set(COUNTS), f"{label}: invalid count fields")
    require(all(type(reported[k]) is int and reported[k] >= 0 for k in COUNTS) and reported == actual, f"{label}: counts differ")


def store_leg_groups(cases: list[dict], rows: list[dict]) -> dict:
    wanted = {case_key(row): row for row in cases}
    groups = {}
    for key in sorted({f"{r['store']}/{r['leg']}" for r in wanted.values()}):
        groups[key] = counts([r for r in rows if f"{wanted[r['key']]['store']}/{wanted[r['key']]['leg']}" == key])
    return groups


def reconcile(expected: dict, receipt: dict, root: Path, manifest: dict) -> dict:
    require(not expected["held"], "held cases cannot certify")
    require(not expected["guarded"], "unlanded arc guards cannot certify")
    require(set(receipt) == {"source_sha", "manifest_sha256", "tier", "cases", "counts", "groups", "audits", "gates"}, "invalid receipt fields")
    for field in ("source_sha", "manifest_sha256", "tier"):
        require(receipt[field] == expected[field], f"receipt {field} differs from plan")
    rows = receipt["cases"]
    require(isinstance(rows, list), "receipt cases must be a list")
    wanted = {case_key(r): r for r in expected["cases"]}
    keys = [r["key"] for r in rows]
    require(len(keys) == len(set(keys)) and set(keys) == set(wanted), "missing, unexpected or duplicate case receipt")
    for row in rows:
        require(set(row) == {"key", "status", "executed", "artifacts", "quarantine"}, f"{row['key']}: invalid case receipt fields")
        require(type(row["executed"]) is bool, "executed must be boolean")
        require(row["status"] in {"passed", "failed", "not_run"}, "unknown case status")
        require(row["executed"] == (row["status"] != "not_run"), "status/executed disagreement")
        require(row["quarantine"] is None, "quarantined cases cannot certify")
        require(row["status"] == "passed", f"{row['key']}: {row['status']}")
        spec = wanted[row["key"]]
        require(set(row["artifacts"]) == set(spec["artifacts"]), f"{row['key']}: incomplete artifacts")
        paths = {name: artifact(root, value) for name, value in row["artifacts"].items()}
        cleanup = json.loads(paths["cleanup"].read_text())
        require(set(cleanup) == {"complete", "errors", "remaining"} and cleanup["complete"] is True and cleanup["errors"] == [] and cleanup["remaining"] == [], f"{row['key']}: cleanup incomplete")
        xml = ET.parse(paths["junit"]).getroot()
        junit = list(xml.iter("testcase"))
        require(len(junit) == 1 and junit[0].get("name") == spec["registration"]["test"], f"{row['key']}: JUnit count/test mismatch")
        require(not any(list(xml.iter(tag)) for tag in ("failure", "error", "skipped")), f"{row['key']}: unsuccessful JUnit case")
        provenance = json.loads(paths["provenance"].read_text())
        require(provenance.get("evidence_error") is None, f"{row['key']}: evidence error: {provenance.get('evidence_error')}")
        require(provenance["source_sha"] == expected["source_sha"] and provenance["case"] == row["key"], "wrong binary provenance")
        require(provenance["protocol"] == "V7", "missing negotiated V7 receipt")
        require(provenance["server_nodes"] == spec["server_nodes"], "wrong cluster size")
        binaries = provenance["binaries"]
        require(set(binaries) == set(spec["binaries"]), "missing binary provenance")
        for name, binary in binaries.items():
            require(SHA.fullmatch(binary["source_sha"]) is not None and binary["source_sha"] == expected["source_sha"], f"{name}: binary from another source")
            artifact(paths["provenance"].parent, binary["artifact"])
        server = provenance["server"]
        require(server["version"] == manifest["server"]["version"] and server["archive_sha256"] == manifest["server"]["archive_sha256"], "server archive differs from pin")
        require(manifest["server"]["executable_sha256"] is not None, "server executable pin not yet registered")
        require(server["artifact"]["sha256"] == manifest["server"]["executable_sha256"], "server executable differs from pin")
        artifact(paths["provenance"].parent, server["artifact"])
    actual = counts(rows)
    check_counts(receipt["counts"], actual, "aggregate")
    groups = store_leg_groups(expected["cases"], rows)
    require(receipt["groups"] == groups, "per-store/per-leg counts differ")
    for key, group in groups.items():
        check_counts(receipt["groups"][key], group, key)
    require(actual["selected"] == actual["executed"] == actual["passed"] > 0, "incomplete execution")
    require(set(receipt["audits"]) == (AUDITS if expected["tier"] == "release" else set()), "missing or unexpected release audits")
    require(set(receipt["gates"]) == (GATES if expected["tier"] == "release" else set()), "missing or unexpected release gates")
    for name, audit in receipt["audits"].items():
        require(set(audit) == {"commit", "receipt"}, f"{name}: invalid audit fields")
        require(SHA.fullmatch(audit["commit"]) is not None and ancestor(audit["commit"], expected["source_sha"]) and ancestor(audit["commit"], "origin/main"), f"{name}: audit is not landed in candidate ancestry")
        gate = json.loads(artifact(root, audit["receipt"]).read_text())
        require(gate == {"ticket": manifest["release_audits"][name], "source_sha": audit["commit"], "status": "passed"}, f"{name}: ticket gate did not pass")
    for name, descriptor in receipt["gates"].items():
        gate = json.loads(artifact(root, descriptor).read_text())
        require(gate == {"gate": name, "source_sha": expected["source_sha"], "status": "passed"}, f"{name}: candidate gate did not pass")
    return {"source_sha": expected["source_sha"], "tier": expected["tier"], "counts": actual, "groups": groups, "status": "passed"}


def write(path: Path, value: dict) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def conclude(expected: dict, receipt_path: Path, manifest: dict, destination: Path) -> dict:
    """Certify a receipt against the plan, recording the refusal reason instead."""
    conclusion = {"source_sha": expected["source_sha"],
                  "manifest_sha256": expected["manifest_sha256"],
                  "tier": expected["tier"], "receipt_sha256": None,
                  "certified": False, "reason": None, "counts": None, "groups": None,
                  "excluded_held": expected["excluded_held"],
                  "tier_complete": expected["tier_complete"]}
    try:
        receipt = json.loads(receipt_path.read_text())
        conclusion["receipt_sha256"] = digest(receipt_path)
        if isinstance(receipt, dict):
            conclusion["counts"] = receipt.get("counts")
            conclusion["groups"] = receipt.get("groups")
        result = reconcile(expected, receipt, receipt_path.parent, manifest)
        conclusion.update(counts=result["counts"], groups=result["groups"], certified=True)
    except (ValueError, KeyError, TypeError, OSError, ET.ParseError) as error:
        conclusion["reason"] = str(error)
    write(destination / "conclusion.json", conclusion)
    return conclusion


def run_cases(expected: dict, artifacts: Path, manifest: dict) -> dict:
    require(not expected["held"], f"unavailable registrations: {', '.join(expected['held'])}")
    require(not expected["guarded"], f"unlanded arc guards: {', '.join(expected['guarded'])}")
    require(subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip() == expected["source_sha"], "checkout differs from exact source SHA")
    require(not subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=ROOT, text=True).strip(), "execution requires a clean source checkout")
    role_files = {"journal": "journal.json", "store": "store.json", "host": "host.json",
                  "trace": "trace.json", "cleanup": "cleanup.json", "junit": "junit.xml",
                  "provenance": "provenance.json"}
    rows = []
    for index, row in enumerate(expected["cases"]):
        directory = artifacts / f"case-{index}"
        subprocess.call([
            "python3", str(ROOT / RUNNER), row["registration"]["label"], row["registration"]["test"],
            "--artifacts", str(directory), "--case", case_key(row),
            "--store", row["store"], "--leg", row["leg"],
        ], cwd=ROOT)
        execution_path = directory / "execution.json"
        if execution_path.is_file():
            execution = json.loads(execution_path.read_text())
            require(execution["scenario"] == row["registration"]["test"], "wrong scenario report")
        else:
            # A runner that died before reporting cannot certify; the case still
            # lands in the receipt so reconcile names the refusal.
            execution = {"counts": {"executed": 0, "passed": 0, "failed": 0}}
        executed = bool(execution["counts"]["executed"])
        if executed:
            require(execution["source_sha"] == expected["source_sha"], "wrong source report")
        status = ("passed" if executed and execution["counts"]["passed"] == 1
                  and execution.get("exit_code") == 0
                  else "failed" if executed else "not_run")
        evidence = {role: {"path": f"case-{index}/{name}", "sha256": digest(directory / name)}
                    for role, name in role_files.items() if (directory / name).is_file()}
        rows.append({"key": case_key(row), "status": status, "executed": executed,
                     "artifacts": evidence, "quarantine": None})
    # These are execution results. Tier certification still requires reconcile's
    # complete journal/store/trace/cleanup/provenance receipts and release gates.
    receipt = {"source_sha": expected["source_sha"], "manifest_sha256": expected["manifest_sha256"],
               "tier": expected["tier"], "cases": rows, "counts": counts(rows),
               "groups": store_leg_groups(expected["cases"], rows), "audits": {}, "gates": {}}
    write(artifacts / "receipt.json", receipt)
    return conclude(expected, artifacts / "receipt.json", manifest, artifacts)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("plan", "run", "reconcile"):
        command = commands.add_parser(name)
        command.add_argument("--tier", choices=("smoke", "full", "release", "live"), required=True)
        command.add_argument("--scenario", action="append", default=[])
        command.add_argument("--case", action="append", default=[], help="exact scenario/variant/store/leg/channel permutation (non-release tiers)")
        command.add_argument("--ready", action="store_true", help="exclude held registrations (non-release tiers)")
        command.add_argument("--sha", required=True)
        command.add_argument("--artifacts", type=Path, required=True)
        if name == "reconcile":
            command.add_argument("--receipt", type=Path, required=True)
    args = parser.parse_args()
    try:
        manifest = load_manifest(args.manifest)
        expected = plan(manifest, digest(args.manifest), args.tier, args.scenario, args.sha, args.case, args.ready)
        args.artifacts = args.artifacts.resolve()
        args.artifacts.mkdir(parents=True, exist_ok=True)
        destination = args.artifacts / "plan.json"
        require(args.command != "run" or not destination.exists(), "run needs a fresh artifact directory; preserve first failure")
        require(not destination.exists() or json.loads(destination.read_text()) == expected, "artifact directory belongs to another plan")
        write(destination, expected)
        if args.command == "plan":
            print(json.dumps(expected, indent=2))
            return 0
        if args.command == "run":
            result = run_cases(expected, args.artifacts, manifest)
            print(json.dumps(result))
            return int(not result["certified"])
        result = conclude(expected, args.receipt, manifest, args.artifacts)
        print(json.dumps(result))
        return int(not result["certified"])
    except (ValueError, KeyError, TypeError, OSError, ET.ParseError, subprocess.CalledProcessError) as error:
        print(f"lash-e2e rejected: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
