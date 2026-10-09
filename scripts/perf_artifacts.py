"""Build profiling executables through Kiln and resolve materialized reports."""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

# Reuse the build interface's materialization and label validation.
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from tools.buck2.outputs import resolve


def add_build_report_arg(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--build-report", type=Path,
        help="Kiln output report to write, or consume with --no-build.",
    )
    parser.add_argument(
        "--cpu-profile", action="store_true",
        help="Build optimized code with line tables and frame pointers for sampling.",
    )


def runtime_label(features: list[str], dhat: bool = False) -> str:
    enabled = {feature for value in features for feature in value.split(",") if feature}
    enabled.discard("default")
    if dhat:
        enabled.add("dhat-heap")
    if enabled - {"dhat-heap"}:
        raise SystemExit(f"error: unsupported Kiln profiling features: {sorted(enabled)}")
    label = "//crates/lash-perf:lash-perf__bin"
    if not enabled:
        return label
    inventory = json.loads((Path(__file__).resolve().parents[1] /
                            "tools/buck2/target-inventory.json").read_text())
    matches = {unit["label"] for unit in inventory["feature_lane_units"]
               if unit["kind"] == "bin" and unit["label"].startswith(label + "__fv_")
               and set(unit["features"]) == enabled}
    if len(matches) != 1:
        raise SystemExit(f"error: no unique Kiln profiling target for features {sorted(enabled)}")
    return matches.pop()


def artifacts(root: Path, labels: list[str], *, build: bool, report: Path | None,
              optimized: bool, symbolized: bool = False) -> dict[str, Path]:
    mode = "profiling" if symbolized else "optimized" if optimized else "dev"
    name = labels[0].split(":")[-1] + ("-group" if len(labels) > 1 else "")
    report = report or root / ".kiln" / "perf" / f"{name}-{mode}.build-report.json"
    report = report if report.is_absolute() else root / report
    if build:
        report.parent.mkdir(parents=True, exist_ok=True)
        cmd = ["kiln", "build"]
        if symbolized:
            cmd += ["--target-platforms", "//tools/buck2:profiling", "-c", "kiln.rust_profile=optimized"]
        elif optimized:
            cmd += ["--config=optimized"]
        cmd += labels + ["--materializations", "final", "--build-report", str(report)]
        print("Building profiling artifacts: " + " ".join(cmd), file=sys.stderr)
        subprocess.run(cmd, cwd=root, check=True)
    try:
        payload = json.loads(report.read_text())
        resolved = {}
        for label in labels:
            paths = resolve(payload, label)
            if len(paths) != 1:
                raise ValueError(f"expected one executable for {label}, found {len(paths)}")
            resolved[label] = Path(paths[0])
        return resolved
    except (OSError, KeyError, ValueError) as error:
        raise SystemExit(f"error: cannot resolve Kiln report {report}: {error}; build with --materializations final") from error
