"""Row identities and committed references for the counted instruction gate."""
from __future__ import annotations

import copy
import hashlib
import json
from pathlib import Path
import re
import subprocess


def kernel_workloads(root: Path) -> dict:
    directory = root / "crates/lash-perf/src/compare"
    registry = (directory / "workloads.rs").read_text()
    workloads = {}
    for block in re.findall(r"Workload\s*\{([^{}]+)\}", registry):
        name = re.search(r'name:\s*"([^"]+)"', block)
        if name is None:
            continue
        low = int(re.search(r"low:\s*(\d+)", block)[1])
        ops = int(re.search(r"ops:\s*(\d+)", block)[1])
        sources = {}
        for field, dialect in (("js", "typescript"), ("py", "python")):
            path = re.search(rf'{field}:\s*include_str!\("([^"]+)"\)', block)[1]
            sources[dialect] = directory / path
        workloads[name[1]] = (low, ops, sources)
    if len(workloads) != len(re.findall(r'name:\s*"', registry)) or not workloads:
        raise ValueError("cannot read complete compare workload registry")
    return workloads


def kernel_geometry(root: Path) -> dict:
    geometry = {}
    for name, (low, ops, sources) in kernel_workloads(root).items():
        trips = [low, low * (10 if low >= 10000 else 100)]
        for dialect, path in sources.items():
            geometry[f"kernel/{dialect}/{name}"] = {
                "source_hash": hashlib.sha256(path.read_bytes()).hexdigest(),
                "trips": trips, "operations": [n * ops for n in trips],
                "runs": 1 if low >= 20000 else 5, "warmups": 1,
            }
    return geometry


def geometry_key(name: str) -> str:
    return name.rsplit("/", 1)[0] if name.startswith("replay/") else name


def source_drift(root: Path, baseline: dict) -> list[str]:
    current = kernel_geometry(root)
    expected = {k: v for k, v in baseline["geometry"].items() if k.startswith("kernel/")}
    drift = [name for name in sorted(current.keys() | expected.keys())
             if current.get(name) != expected.get(name)]
    directory = root / "crates/lash-perf/workloads/replay"
    subset = json.loads((directory / "budgets.json").read_text())["subset"]
    replay = {f"replay/{spec['workload']}": spec for spec in subset}
    previous = {k for k in baseline["geometry"] if k.startswith("replay/")}
    drift.extend(sorted(replay.keys() ^ previous))
    for key in sorted(replay.keys() & previous):
        spec = replay[key]
        path = directory / f"{spec['workload']}.json"
        source = json.loads(path.read_text())
        provenance = baseline.get("row_provenance", {}).get(key, baseline["provenance"])
        old = subprocess.check_output(["git", "show", f"{provenance['commit']}:{path.relative_to(root)}"], cwd=root)
        geometry = baseline["geometry"][key]
        turns = spec.get("turns", sum(group.get("repeat", 1) for group in source["turns"]))
        if (old != path.read_bytes() or turns != geometry["turns"]
                or source.get("sessions", 1) != geometry["sessions"]):
            drift.append(key)
    expected_rows = set(current)
    for key in replay:
        source = json.loads((directory / f"{replay[key]['workload']}.json").read_text())
        expected_rows.add(f"{key}/host")
        if source["protocol"] != "standard":
            expected_rows.add(f"{key}/worker")
    drift.extend(sorted(expected_rows ^ baseline["rows"].keys()))
    return sorted(set(drift))


def check_sources(root: Path) -> None:
    baseline = json.loads((root / "docs/perf/baselines/instructions.json").read_text())
    drift = source_drift(root, baseline)
    if drift:
        raise ValueError("stale instruction baseline for " + ", ".join(drift)
                         + "; run just perf-gate-instructions --calibrate to update only changed rows")


def references(baseline: dict, candidate: dict, committed: dict) -> dict:
    """Preserve comparable previous rows; require a committed replacement for others."""
    for source in (baseline, committed):
        for identity in ("identity", "cpu"):
            if not source.get(identity) or source[identity] != candidate.get(identity):
                raise ValueError(f"incomparable instructions baseline: {identity}")
    selected = copy.deepcopy(baseline)
    selected["rows"] = {}
    selected["geometry"] = candidate["geometry"]
    verdicts = {}
    if set(candidate["rows"]) != set(committed["rows"]) or set(candidate["geometry"]) != set(committed["geometry"]):
        raise ValueError("row population changed without a committed baseline update")
    for name in candidate["rows"]:
        key = geometry_key(name)
        geometry = candidate["geometry"].get(key)
        if not geometry or committed["geometry"].get(key) != geometry:
            raise ValueError(f"{name}: changed geometry needs a committed baseline update")
        comparable = name in baseline["rows"] and baseline["geometry"].get(key) == geometry
        source = baseline if comparable else committed
        selected["rows"][name] = source["rows"][name]
        verdicts[name] = "gated" if comparable else "rebaselined: geometry changed"
    for name in baseline["rows"].keys() - candidate["rows"].keys():
        verdicts[name] = "rebaselined: row removed"
    selected["row_verdicts"] = verdicts
    return selected


def calibrate_changed(baseline: dict, candidate: dict) -> dict:
    """Retain measured references and provenance for every unchanged row."""
    refreshed = copy.deepcopy(baseline)
    changed = {key for key in candidate["geometry"]
               if candidate["geometry"][key] != baseline["geometry"].get(key)}
    refreshed["geometry"] = candidate["geometry"]
    refreshed["rows"] = {name: candidate["rows"][name] if geometry_key(name) in changed or name not in baseline["rows"]
                         else baseline["rows"][name] for name in candidate["rows"]}
    for field in ("noise", "ceilings"):
        refreshed[field] = {name: (0 if field == "noise" else candidate["rows"][name])
                           if geometry_key(name) in changed or name not in baseline["rows"]
                           else baseline.get(field, {}).get(name, 0 if field == "noise" else baseline["rows"][name])
                           for name in candidate["rows"]}
    provenance = baseline.get("row_provenance", {})
    refreshed["row_provenance"] = {key: copy.deepcopy(value) for key, value in provenance.items()
                                   if key in candidate["geometry"]}
    for key in changed:
        evidence = dict(candidate["provenance"])
        artifact = "kernel" if key.startswith("kernel/") else key.split("/", 1)[1]
        if artifact in candidate.get("artifacts", {}):
            evidence["executable_sha256"] = candidate["artifacts"][artifact]
        refreshed["row_provenance"][key] = evidence
    return refreshed
