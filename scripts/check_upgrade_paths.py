#!/usr/bin/env python3
"""Hold the upgrade-path declaration exhaustive against the format registry.

ADR 0106 section 2 gives every versioned durable surface one of three upgrade
policies once the clean-slate release lands: forward **migration** (schema DDL
or a read upcaster), **drain** by deployment generation, or **coexistence**
through the roll window. ``scripts/upgrade-paths.toml`` is where each surface
declares its policy, keyed by the same ``<constant_path>:<constant>`` pair
``scripts/versioned-surfaces.toml`` registers.

This check holds that declaration honest in both directions:

- every ``[[surface]]`` in the registry appears exactly once in
  ``upgrade-paths.toml`` with ``upgrade = "migrate"|"drain"|"coexist"``; and
- every declared entry names a constant the registry knows -- a registered
  surface, or an ``[[unregistered]]`` entry the ADR still names a law for
  (the live-cursor and journal-identity stamps). A stale entry fails, so the
  file cannot rot into a list of surfaces that no longer exist.

It reads sources only and uses the standard library alone, so it can run
before the Rust toolchain is installed.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_format_registry  # noqa: E402

DEFAULT_REGISTRY = Path(__file__).with_name("versioned-surfaces.toml")
DEFAULT_DECLARATIONS = Path(__file__).with_name("upgrade-paths.toml")
UPGRADE_POLICIES = ("migrate", "drain", "coexist")


class DeclarationError(Exception):
    """The declaration file cannot be read or is not in the expected shape."""


def load_declarations(path: Path) -> dict[str, dict]:
    try:
        document = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise DeclarationError(f"cannot read {path}: {error}") from error

    declared: dict[str, dict] = {}
    for index, raw in enumerate(document.get("surface", []), start=1):
        location = f"{path}: surface {index}"
        constant = raw.get("constant")
        constant_path = raw.get("constant_path")
        if not isinstance(constant, str) or not isinstance(constant_path, str):
            raise DeclarationError(f"{location} needs constant and constant_path")
        key = f"{constant_path}:{constant}"
        if key in declared:
            raise DeclarationError(f"{location} duplicates {key}")
        upgrade = raw.get("upgrade")
        if upgrade not in UPGRADE_POLICIES:
            raise DeclarationError(
                f"{location} ({key}) needs upgrade = one of "
                + "|".join(UPGRADE_POLICIES)
            )
        declared[key] = raw
    if not declared:
        raise DeclarationError(f"{path}: no [[surface]] entries")
    return declared


def check(
    registry_path: Path, declarations_path: Path
) -> tuple[list[str], dict[str, dict]]:
    try:
        registry = check_format_registry.load_registry(registry_path)
        declared = load_declarations(declarations_path)
    except (check_format_registry.RegistryError, DeclarationError) as error:
        raise DeclarationError(str(error)) from error

    problems: list[str] = []
    for key in sorted(registry.surfaces):
        if key not in declared:
            problems.append(
                f"{key} is a registered surface with no upgrade declaration: "
                f"add an entry with upgrade = {'|'.join(UPGRADE_POLICIES)} to "
                f"{declarations_path}"
            )
    known = set(registry.surfaces) | set(registry.unregistered)
    for key in sorted(declared):
        if key not in known:
            problems.append(
                f"{key} is declared in {declarations_path} but the registry does "
                "not know it; delete the stale entry or register the constant in "
                "versioned-surfaces.toml first"
            )
    return problems, declared


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--registry", type=Path, default=DEFAULT_REGISTRY)
    parser.add_argument("--declarations", type=Path, default=DEFAULT_DECLARATIONS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        problems, declared = check(args.registry, args.declarations)
    except DeclarationError as error:
        print(f"upgrade-path check error: {error}", file=sys.stderr)
        return 2
    if problems:
        print("upgrade-path check failed:", file=sys.stderr)
        for problem in problems:
            print(f"- {problem}", file=sys.stderr)
        return 1
    by_policy = {policy: 0 for policy in UPGRADE_POLICIES}
    for raw in declared.values():
        by_policy[raw["upgrade"]] += 1
    print(
        "upgrade-path check passed: "
        + ", ".join(f"{count} {policy}" for policy, count in by_policy.items())
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
