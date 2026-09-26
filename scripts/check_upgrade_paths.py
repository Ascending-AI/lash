#!/usr/bin/env python3
"""Hold the upgrade-path prose exhaustive against the format registry.

ADR 0106 section 2 gives every versioned durable surface one of three upgrade
policies once the clean-slate release lands: forward **migration** (schema DDL
or a read upcaster), **drain** by deployment generation, or **coexistence**
through the roll window.

The policy value is declared exactly once, as ``upgrade = ...`` on the
surface's ``[[surface]]`` entry in ``scripts/versioned-surfaces.toml`` (or on
the ``[[unregistered]]`` entry for the two constants the ADR names a law for).
``scripts/check_format_registry.py`` validates those values and holds each
manifest surface's policy equal to its ``DurableFormat::upgrade_policy()``
arm, so the TOML and the Rust cannot diverge. ``upgrade-paths.toml`` carries
the prose beside the enum: the ``note`` that states the mechanism and the
reason a reviewer reads at upgrade time.

This check holds that prose honest in both directions:

- every ``[[surface]]`` in the registry, and every ``[[unregistered]]`` entry
  that carries a policy, appears exactly once in ``upgrade-paths.toml`` with a
  non-empty ``note``; and
- every declared entry names a constant the registry knows, and no entry
  restates ``upgrade`` — a policy written here as well would be a second
  source of truth. A stale entry fails, so the file cannot rot into a list of
  surfaces that no longer exist.

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
UPGRADE_POLICIES = check_format_registry.UPGRADE_POLICIES


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
        if "upgrade" in raw:
            raise DeclarationError(
                f"{location} ({key}) restates upgrade: the policy value is "
                "declared once, on the registry's [[surface]] or "
                "[[unregistered]] entry; delete it here"
            )
        note = raw.get("note")
        if not isinstance(note, str) or not note.strip():
            raise DeclarationError(f"{location} ({key}) needs a non-empty note")
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
    needs_note = set(registry.surfaces) | set(registry.unregistered_upgrades)
    for key in sorted(needs_note):
        if key not in declared:
            problems.append(
                f"{key} is a versioned surface with no upgrade-path note: add "
                f"an entry with a note to {declarations_path}"
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
    # The counts report the registry's policies, not the declaration file's.
    registry = check_format_registry.load_registry(args.registry)
    by_policy = {policy: 0 for policy in UPGRADE_POLICIES}
    for raw in registry.surfaces.values():
        by_policy[raw["upgrade"]] += 1
    for policy in registry.unregistered_upgrades.values():
        by_policy[policy] += 1
    print(
        "upgrade-path check passed: "
        + ", ".join(f"{count} {policy}" for policy, count in by_policy.items())
        + f" ({len(declared)} noted)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
