#!/usr/bin/env python3
"""The Test262 outcome ratchet across commits (FIG-3646).

The conformance runner holds each head to its own `outcomes.tsv`: a changed
outcome fails until the record changes with it. This check holds the record
to its base, so the record itself can only improve:

- a test that passed at the base passes at the head;
- a test failing at the head either failed at the base too, or was not
  executable there (refused, or waiting on the harness): lifting a refusal
  may expose a failure, but nothing that ran cleanly may start failing, and
  the failure list otherwise only shrinks.

Owners may change (a failure moving to a narrower ticket); classes may not
regress.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUTCOMES = "crates/lash-typescript/tests/test262/outcomes.tsv"


def parse(text: str) -> dict[str, tuple[str, str]]:
    outcomes = {}
    for line in text.splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        path, outcome_class, qualifier = line.split("\t")
        outcomes[path] = (outcome_class, qualifier)
    return outcomes


def regressions(base: dict[str, tuple[str, str]], head: dict[str, tuple[str, str]]) -> list[str]:
    problems = []
    for path, (head_class, head_qualifier) in sorted(head.items()):
        base_class = base.get(path, (None, None))[0]
        if base_class == "pass" and head_class != "pass":
            problems.append(f"{path}: passed at the base, now `{head_class} {head_qualifier}`")
        elif head_class == "fail" and base_class not in (None, "fail", "refused", "harness"):
            problems.append(f"{path}: `{base_class}` at the base, now failing ({head_qualifier})")
    for path in sorted(set(base) - set(head)):
        if base[path][0] == "pass":
            problems.append(f"{path}: passed at the base and left the record")
    return problems


def base_outcomes(base: str) -> dict[str, tuple[str, str]] | None:
    shown = subprocess.run(
        ["git", "show", f"{base}:{OUTCOMES}"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    return parse(shown.stdout) if shown.returncode == 0 else None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", required=True, help="the commit the head is compared with")
    args = parser.parse_args()
    base = base_outcomes(args.base)
    if base is None:
        print(f"check_test262_ratchet: {OUTCOMES} is new at this head; nothing to compare")
        return 0
    head = parse((ROOT / OUTCOMES).read_text())
    problems = regressions(base, head)
    if problems:
        print("The Test262 record regressed against its base:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    passes = sum(1 for outcome in head.values() if outcome[0] == "pass")
    fails = sum(1 for outcome in head.values() if outcome[0] == "fail")
    print(f"check_test262_ratchet: no regression ({passes} pass, {fails} fail)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
