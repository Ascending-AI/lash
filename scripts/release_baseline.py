#!/usr/bin/env python3
"""Inventory registered source constants, or check a declared release baseline.

The resolver accepts literal counters, string identities and local constant
aliases with integer addition/subtraction. It evaluates cfg(feature =
"synthetic-next") in both tiers. Unsupported expressions, missing definitions,
duplicate active definitions and incomplete baseline tables fail closed.
"""

from __future__ import annotations

import argparse
import ast
import json
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
REGISTRY = Path("scripts/versioned-surfaces.toml")
BASELINE = Path("scripts/release-baseline.toml")
CONST = re.compile(
    r"^[ \t]*(?P<attrs>(?:#\[[^\]]*\]\s*)*)"
    r"(?:pub(?:\([^)]*\))?\s+)?const\s+(?P<name>[A-Z][A-Z0-9_]*)"
    r"\s*:\s*[^=;]+?=\s*(?P<value>[^;]+);", re.MULTILINE,
)
LEXEME = re.compile(r'r(?P<hashes>\#*)".*?"(?P=hashes)|"(?:\\.|[^"\\])*"|//[^\n]*|/\*', re.DOTALL)


class BaselineError(Exception):
    """The source or declaration cannot establish the baseline."""


def without_comments(text: str) -> str:
    # Preserve offsets for the reset planner, including comments between attrs.
    result = list(text)
    at = 0
    while match := LEXEME.search(text, at):
        at = match.end()
        if match.group().startswith("/*"):
            depth = 1
            while depth:
                token = re.search(r"/\*|\*/", text[at:])
                if token is None:
                    raise BaselineError("unterminated Rust block comment")
                at += token.end()
                depth += 1 if token.group() == "/*" else -1
        elif not match.group().startswith("//"):
            continue
        for index in range(match.start(), at):
            if result[index] != "\n":
                result[index] = " "
    return "".join(result)


def enabled(attrs: str, synthetic: bool) -> bool:
    for condition in re.findall(r"#\[cfg\((.*?)\)\]", attrs, re.DOTALL):
        condition = re.sub(r"\s+", "", condition)
        if condition == 'feature="synthetic-next"':
            active = synthetic
        elif condition == 'not(feature="synthetic-next")':
            active = not synthetic
        elif condition == "test":
            active = False
        elif condition == "not(test)":
            active = True
        else:
            raise BaselineError(f"unsupported cfg({condition})")
        if not active:
            return False
    if "cfg_attr" in attrs:
        raise BaselineError("cfg_attr on a version constant is unsupported")
    return True


def definitions(text: str):
    return list(CONST.finditer(without_comments(text)))


def resolve(text: str, name: str, synthetic: bool):
    constants = {}
    for match in definitions(text):
        constants.setdefault(match["name"], []).append(match)

    def value(symbol: str, trail: tuple[str, ...]):
        if symbol in trail:
            raise BaselineError(f"cyclic constant alias: {' -> '.join((*trail, symbol))}")
        candidates = [m for m in constants.get(symbol, []) if enabled(m["attrs"], synthetic)]
        if len(candidates) != 1:
            raise BaselineError(f"{symbol}: expected one active definition, found {len(candidates)}")
        expression = candidates[0]["value"].strip()
        expression = re.sub(r"\b(\d[\d_]*)(?:u(?:8|16|32|64|128|size)|i(?:8|16|32|64|128|size))\b", r"\1", expression)
        try:
            node = ast.parse(expression, mode="eval").body
        except SyntaxError as error:
            raise BaselineError(f"{symbol}: unsupported expression {expression!r}") from error

        def evaluate(node):
            if isinstance(node, ast.Constant) and type(node.value) in (int, str):
                return node.value
            if isinstance(node, ast.Name):
                return value(node.id, (*trail, symbol))
            if isinstance(node, ast.BinOp) and isinstance(node.op, (ast.Add, ast.Sub)):
                left, right = evaluate(node.left), evaluate(node.right)
                if type(left) is int and type(right) is int:
                    return left + right if isinstance(node.op, ast.Add) else left - right
            raise BaselineError(f"{symbol}: unsupported expression {expression!r}")

        return evaluate(node)

    return value(name, ())


def surfaces(repo: Path):
    rows = tomllib.loads((repo / REGISTRY).read_text())["surface"]
    keys = [f'{row["constant_path"]}:{row["constant"]}' for row in rows]
    if not rows or len(set(keys)) != len(keys):
        raise BaselineError("surface registry is empty or contains duplicate keys")
    return rows


def inventory(repo: Path):
    rows = []
    for surface in surfaces(repo):
        key = f'{surface["constant_path"]}:{surface["constant"]}'
        try:
            text = (repo / surface["constant_path"]).read_text()
            default = resolve(text, surface["constant"], False)
            synthetic = resolve(text, surface["constant"], True)
        except BaselineError as error:
            raise BaselineError(f"{key}: {error}") from error
        rows.append({"key": key, "default": default, "synthetic": synthetic,
                     "upgrade": surface["upgrade"], "manifest": surface.get("manifest")})
    return rows


def load_baseline(path: Path):
    baseline = tomllib.loads(path.read_text())["baseline"]
    if not baseline or any(type(v) not in (int, str) for v in baseline.values()):
        raise BaselineError("baseline needs a nonempty table of counters or string identities")
    return baseline


def mismatches(rows: list[dict], baseline: dict):
    by_key = {row["key"]: row for row in rows}
    errors = [f"{key}: omitted from baseline" for key in sorted(by_key.keys() - baseline.keys())]
    errors += [f"{key}: baseline has an unregistered surface" for key in sorted(baseline.keys() - by_key.keys())]
    for key in sorted(by_key.keys() & baseline.keys()):
        actual, expected = by_key[key]["default"], baseline[key]
        if type(actual) is not type(expected) or actual != expected:
            errors.append(f"{key}: default {actual!r}, declared baseline {expected!r}")
    return errors


def verify_build(rows: list[dict], report: Path, synthetic: bool):
    text = report.read_text()
    marker = "release-inventory-build="
    line = next((line.split(marker, 1)[1] for line in text.splitlines() if marker in line), None)
    if line is None:
        raise BaselineError("build probe did not execute or emitted no inventory")
    built = json.loads(line)
    tier = "synthetic" if synthetic else "default"
    expected = {row["key"].rsplit(":", 1)[1]: row[tier] for row in rows if row["manifest"]}
    actual = {row["constant"]: row["value"] for row in built["formats"]}
    if actual != expected or len(actual) != len(built["formats"]):
        raise BaselineError(f"{tier} durable_formats differs from source: expected={expected}, actual={actual}")
    for name, wire in (("REMOTE_PROTOCOL_VERSION", "remote_protocol"), ("RESTATE_WIRE_VERSION", "restate")):
        value = next(row[tier] for row in rows if row["key"].endswith(":" + name))
        bound = "min" if wire == "remote_protocol" else "max"
        if built["version"]["wires"][wire][bound] != value:
            raise BaselineError(f"lashctl version's {wire} differs from source")
    print(f"build probe agrees on {len(actual)} durable formats and 2 lashctl wires", file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=ROOT)
    commands = parser.add_subparsers(dest="command", required=True)
    inventory_command = commands.add_parser("inventory")
    inventory_command.add_argument("--build-report", type=Path)
    inventory_command.add_argument("--synthetic", action="store_true")
    check = commands.add_parser("check")
    check.add_argument("--baseline", type=Path, default=BASELINE)
    args = parser.parse_args()
    try:
        rows = inventory(args.repo)
        if args.command == "inventory":
            if args.build_report is not None:
                verify_build(rows, args.build_report, args.synthetic)
            print(json.dumps(rows, indent=2))
            return 0
        path = args.baseline if args.baseline.is_absolute() else args.repo / args.baseline
        errors = mismatches(rows, load_baseline(path))
        if errors:
            print("\n".join(errors), file=sys.stderr)
            return 1
        print(f"release baseline: {len(rows)} surfaces, zero omissions, zero mismatches")
        return 0
    except (BaselineError, OSError, KeyError, tomllib.TOMLDecodeError, json.JSONDecodeError) as error:
        print(f"release baseline error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
