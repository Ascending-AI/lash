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

One exception is a ruling, not a regression: a test that passed may become
`refused <code>` when the same change adds a `rejected` row to `census.tsv`
that names `<code>`. A new refusal class (an unsupported feature
the dialect used to accept by accident, or a construct `tsc --strict`
rejects) can demote tests that passed only incidentally, and the census row
that registers it is the reviewed record of that decision. Every such move is
printed.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUTCOMES = "crates/lash-typescript/tests/test262/outcomes.tsv"
CENSUS = "crates/lash-typescript/tests/test262/census.tsv"


def parse(text: str) -> dict[str, tuple[str, str]]:
    outcomes = {}
    for line in text.splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        path, outcome_class, qualifier = line.split("\t")
        outcomes[path] = (outcome_class, qualifier)
    return outcomes


def tallies(outcomes: dict[str, tuple[str, str]]) -> dict[tuple[str, str], int]:
    """The record's own tallies: the selection size, each class, each class and qualifier."""
    counts: dict[tuple[str, str], int] = {("selected", "-"): len(outcomes)}
    for outcome_class, qualifier in outcomes.values():
        counts[(outcome_class, "*")] = counts.get((outcome_class, "*"), 0) + 1
        if outcome_class != "pass":
            counts[(outcome_class, qualifier)] = counts.get((outcome_class, qualifier), 0) + 1
    return counts


def rejected_rows(text: str) -> dict[tuple[str, str], str]:
    """`census.tsv`'s rejection rows: (kind, name) -> the diagnostic code."""
    rows = {}
    for line in text.splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) >= 4 and fields[2] == "rejected":
            rows[(fields[0], fields[1])] = fields[3]
    return rows


def regressions(
    base: dict[str, tuple[str, str]],
    head: dict[str, tuple[str, str]],
    new_refusal_codes: frozenset[str] = frozenset(),
) -> list[str]:
    problems = []
    for path, (head_class, head_qualifier) in sorted(head.items()):
        base_class = base.get(path, (None, None))[0]
        if base_class == "pass" and head_class == "refused" and head_qualifier in new_refusal_codes:
            continue
        if base_class == "pass" and head_class != "pass":
            problems.append(f"{path}: passed at the base, now `{head_class} {head_qualifier}`")
        elif head_class == "fail" and base_class not in (None, "fail", "refused", "harness"):
            problems.append(f"{path}: `{base_class}` at the base, now failing ({head_qualifier})")
    for path in sorted(set(base) - set(head)):
        if base[path][0] == "pass":
            problems.append(f"{path}: passed at the base and left the record")
    return problems


def show(base: str, path: str) -> str | None:
    shown = subprocess.run(
        ["git", "show", f"{base}:{path}"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    return shown.stdout if shown.returncode == 0 else None


def base_outcomes(base: str) -> dict[str, tuple[str, str]] | None:
    text = show(base, OUTCOMES)
    return parse(text) if text is not None else None


def new_refusal_codes(base: str) -> frozenset[str]:
    """The codes named by rejection rows the head's census adds over the base's."""
    head = rejected_rows((ROOT / CENSUS).read_text())
    base_text = show(base, CENSUS)
    base_rows = rejected_rows(base_text) if base_text is not None else {}
    return frozenset(code for row, code in head.items() if base_rows.get(row) != code)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", required=True, help="the commit the head is compared with")
    args = parser.parse_args()
    base = base_outcomes(args.base)
    if base is None:
        print(f"check_test262_ratchet: {OUTCOMES} is new at this head; nothing to compare")
        return 0
    head = parse((ROOT / OUTCOMES).read_text())
    ruled = new_refusal_codes(args.base)
    problems = regressions(base, head, ruled)
    for path, (head_class, head_qualifier) in sorted(head.items()):
        if base.get(path, (None, None))[0] == "pass" and head_class == "refused" and head_qualifier in ruled:
            print(f"check_test262_ratchet: {path} passed at the base and is now refused by {head_qualifier}, which a new census rejection row registers")
    if problems:
        print("The Test262 record regressed against its base:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print("check_test262_ratchet: no regression; the record's tallies:")
    for (outcome_class, qualifier), count in sorted(tallies(head).items()):
        print(f"  {outcome_class}\t{qualifier}\t{count}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
