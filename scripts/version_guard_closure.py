#!/usr/bin/env python3
"""Print what each version constant guards by reachability.

``check_version_bumps.py`` derives a surface's guard set from the root types
its ``version_guard(..)`` marker names (``roots(..)``) and from the shapes it
sweeps (``shapes(..)``): every type those serialize is guarded with them. This
command prints that closure for the working tree, so a reviewer can see what a
root pulls in and what the walk does not follow:

- each reachable shape, with its depth below the roots;
- the cycles among them;
- what is opaque: another package's types and adapters, associated types,
  types whose Serde impls are hand-written, and macro-declared types;
- what could not be resolved, which is also an error in the gate and in
  ``check_format_registry.py``.

It exits 2 when a reachable type cannot be resolved, and 0 otherwise. It is a
report, not a gate: the gate is ``check_version_bumps.py``.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_version_bumps as gate  # noqa: E402


def surface_report(view: gate.TreeView, surface: gate.Surface) -> dict:
    declaration = gate._declaration(view, surface)
    report: dict = {"surface": surface.key, "roots": [], "shapes": [], "cycles": [], "opaque": [],
                    "unresolved": [], "missing_roots": []}
    if declaration is None or declaration.unshaped is not None:
        return report
    depths: dict[tuple[str, str], int] = {}
    cycles: set[tuple[str, ...]] = set()
    opaque: set[tuple[str, str]] = set()
    for guard in declaration.guards:
        if guard.kind not in {"roots", "shapes"}:
            continue
        closure, missing = gate.closure_of(view, guard)
        report["missing_roots"].extend(missing)
        report["unresolved"].extend(closure.problems)
        cycles.update(closure.cycles)
        opaque.update(closure.opaque)
        for shape, depth in closure.depths:
            key = (shape.name, shape.path)
            depths[key] = min(depth, depths.get(key, depth))
    report["roots"] = sorted(name for (name, _), depth in depths.items() if depth == 0)
    report["shapes"] = [
        {"name": name, "path": path, "depth": depth}
        for (name, path), depth in sorted(depths.items(), key=lambda item: (item[1], item[0]))
    ]
    report["cycles"] = [list(cycle) for cycle in sorted(cycles)]
    report["opaque"] = [{"what": what, "why": why} for what, why in sorted(opaque)]
    report["unresolved"] = sorted(set(report["unresolved"]))
    return report


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--constant", action="append", default=[],
                        help="report only this constant (repeatable)")
    parser.add_argument("--json", action="store_true", help="print the full report as JSON")
    parser.add_argument("--repo", type=Path, default=gate.ROOT, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    view = gate.WorktreeView(args.repo)
    try:
        registry = view.content(gate.REGISTRY)
        if registry is None:
            raise gate.CheckError(f"cannot read {gate.REGISTRY}")
        reports = [
            surface_report(view, surface)
            for surface in gate.load_surfaces(registry, gate.REGISTRY)
            if not args.constant or surface.constant in args.constant
        ]
    except gate.CheckError as error:
        print(f"version-guard closure error: {error}", file=sys.stderr)
        return 2
    failed = any(report["unresolved"] or report["missing_roots"] for report in reports)
    if args.json:
        json.dump(reports, sys.stdout, indent=2)
        print()
        return 2 if failed else 0
    for report in reports:
        shapes = report["shapes"]
        deepest = max((shape["depth"] for shape in shapes), default=0)
        print(
            f"{report['surface']}: {len(shapes)} shapes from {len(report['roots'])} roots, "
            f"deepest {deepest}, {len(report['cycles'])} cycles, {len(report['opaque'])} opaque, "
            f"{len(report['unresolved']) + len(report['missing_roots'])} unresolved"
        )
        if args.constant:
            for shape in shapes:
                print(f"  {shape['depth']}  {shape['name']}  ({shape['path']})")
            for cycle in report["cycles"]:
                print(f"  cycle: {' -> '.join(cycle)}")
            for entry in report["opaque"]:
                print(f"  opaque: {entry['what']}: {entry['why']}")
        for name in report["missing_roots"]:
            print(f"  missing root: {name}", file=sys.stderr)
        for problem in report["unresolved"]:
            print(f"  unresolved: {problem}", file=sys.stderr)
    return 2 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
