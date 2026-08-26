#!/usr/bin/env python3
"""Report Rust function complexity hotspots using rust-code-analysis-cli."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys


ANALYZER = "rust-code-analysis-cli"
INSTALL = "cargo install rust-code-analysis-cli --version 0.0.25 --locked"
EXCLUDED = {
    "crates/lash-sim/tests/cross_backend_store_differential/generated_surface.rs",
    "crates/lash-regress/src/unicodetables.rs",
    "crates/lash-regress/tests/unicode_property_escapes.rs",
}
MARKERS = ("@generated", "DO NOT EDIT")


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def is_generated(path: Path) -> bool:
    try:
        head = path.read_text(encoding="utf-8", errors="replace").splitlines()[:5]
    except OSError:
        return False
    return any(marker in line for line in head for marker in MARKERS)


def rust_files(root: Path, requested: list[str]) -> list[str]:
    paths: list[Path] = []
    for item in requested or ["crates"]:
        path = (root / item).resolve()
        if not path.exists():
            raise SystemExit(f"path does not exist: {item}")
        if path.is_file():
            paths.append(path)
        else:
            paths.extend(path.rglob("*.rs"))

    selected: list[str] = []
    for path in sorted(set(paths)):
        rel = path.relative_to(root).as_posix()
        if "/target/" in f"/{rel}" or rel in EXCLUDED or is_generated(path):
            continue
        selected.append(rel)
    return selected


def metric(space: dict, name: str) -> int:
    value = space.get("metrics", {}).get(name, {})
    if name == "loc":
        value = value.get("sloc", 0)
    else:
        value = value.get("sum", 0)
    return int(value)


def collect_functions(node: dict, path: str, rows: list[dict]) -> None:
    for space in node.get("spaces", []):
        if space.get("kind") == "function":
            rows.append(
                {
                    "path": path,
                    "line": int(space["start_line"]),
                    "end_line": int(space["end_line"]),
                    "function": space["name"],
                    "ccn": metric(space, "cyclomatic"),
                    "cognitive": metric(space, "cognitive"),
                    "sloc": metric(space, "loc"),
                }
            )
        collect_functions(space, path, rows)


def analyze(root: Path, paths: list[str]) -> list[dict]:
    exclusions = set(EXCLUDED)
    exclusions.update(path for path in paths if is_generated(root / path))
    command = [ANALYZER]
    for path in paths:
        command.extend(["-p", path])
    command.extend(["-l", "rust", "-m", "-F", "-O", "json", "--pr"])
    for path in sorted(exclusions):
        command.extend(["-X", path])
    result = subprocess.run(command, cwd=root, text=True, capture_output=True)
    if result.returncode:
        sys.stderr.write(result.stderr)
        raise SystemExit(result.returncode)

    rows: list[dict] = []
    decoder = json.JSONDecoder()
    offset = 0
    while offset < len(result.stdout):
        while offset < len(result.stdout) and result.stdout[offset].isspace():
            offset += 1
        if offset == len(result.stdout):
            break
        document, offset = decoder.raw_decode(result.stdout, offset)
        collect_functions(document, document["name"], rows)
    return sorted(rows, key=lambda row: (-row["ccn"], row["path"], row["line"]))


def markdown(rows: list[dict], top: int) -> str:
    thresholds = [10, 15, 20, 30]
    lines = [
        "# Complexity hotspots",
        "",
        "| functions | CCN > 10 | CCN > 15 | CCN > 20 | CCN > 30 |",
        "|---:|---:|---:|---:|---:|",
        "| %d | %s | %s | %s | %s |"
        % (len(rows), *(sum(row["ccn"] > threshold for row in rows) for threshold in thresholds)),
        "",
        f"Top {min(top, len(rows))} functions (ordered by CCN descending, then path and line):",
        "",
        "| # | CCN | cognitive | SLOC | span | function |",
        "|---:|---:|---:|---:|---|---|",
    ]
    for number, row in enumerate(rows[:top], 1):
        lines.append(
            f"| {number} | {row['ccn']} | {row['cognitive']} | {row['sloc']} | "
            f"`{row['path']}:{row['line']}-{row['end_line']}` | `{row['function']}` |"
        )
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("paths", nargs="*", help="Rust roots/files; defaults to crates")
    parser.add_argument("--top", type=int, default=25, metavar="N")
    parser.add_argument("--json", action="store_true", help="emit raw normalized rows")
    args = parser.parse_args()
    if args.top < 0:
        parser.error("--top must be non-negative")
    if shutil.which(ANALYZER) is None:
        print(f"{ANALYZER} is required; install it with: {INSTALL}", file=sys.stderr)
        return 1
    root = repo_root()
    paths = rust_files(root, args.paths)
    if not paths:
        print("no Rust files selected", file=sys.stderr)
        return 1
    rows = analyze(root, paths)
    if args.json:
        json.dump(rows, sys.stdout, indent=2)
        sys.stdout.write("\n")
    else:
        sys.stdout.write(markdown(rows, args.top))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
