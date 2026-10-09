#!/usr/bin/env python3
"""The Test262 outcome ratchet across commits (FIG-3646).

The conformance runner holds each head to its own outcome record — the
`outcomes/**/*.tsv` shards, one file per test directory cut as deep as the
tree is hot (FIG-3727): a changed outcome fails until the record changes
with it. This check holds the record to its base, so the record itself can
only improve:

- a test that passed at the base passes at the head;
- a test failing at the head either failed at the base too, or was not
  executable there (refused, or waiting on the harness): lifting a refusal
  may expose a failure, but nothing that ran cleanly may start failing, and
  the failure list otherwise only shrinks.

Owners may change (a failure moving to a narrower ticket); classes may not
regress.

One exception is a ruling, not a regression: a test that passed may become
`refused <code>` when the same change adds a `rejected` row to the census
(`census/<kind>/<name>.tsv`, or the legacy `census.tsv`) that names `<code>`.
A new refusal class (an unsupported feature the dialect used to accept by
accident, or a construct `tsc --strict` rejects) can demote tests that
passed only incidentally, and the census row that registers it is the
reviewed record of that decision. Every such move is printed.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TEST262 = "crates/lash-dialect-typescript/tests/test262"
OUTCOMES = f"{TEST262}/outcomes.tsv"
OUTCOMES_DIR = f"{TEST262}/outcomes"
CENSUS = f"{TEST262}/census.tsv"
CENSUS_DIR = f"{TEST262}/census"


def parse(text: str) -> dict[str, tuple[str, str]]:
    outcomes = {}
    for line in text.splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        path, outcome_class, qualifier = line.split("\t")
        if path in outcomes:
            raise SystemExit(f"check_test262_ratchet: duplicate observation for {path}")
        outcomes[path] = (outcome_class, qualifier)
    return outcomes


def parse_shards(texts: list[tuple[str, str]]) -> dict[str, tuple[str, str]]:
    """The union of every `outcomes/<shard>.tsv`: an id may appear in only one file."""
    outcomes: dict[str, tuple[str, str]] = {}
    for name, text in texts:
        for path, outcome in parse(text).items():
            if path in outcomes:
                raise SystemExit(f"check_test262_ratchet: {path} has an outcome in two shards ({name})")
            outcomes[path] = outcome
    return outcomes


def tree_paths(ref: str | None, legacy_file: str, directory: str) -> list[str]:
    """A sharded record's paths at `ref` (None = the working tree): the legacy
    single file, or every `*.tsv` under its shard directory, recursively."""
    if ref is None:
        paths = []
        if (ROOT / legacy_file).is_file():
            paths.append(legacy_file)
        root_directory = ROOT / directory
        if root_directory.is_dir():
            paths.extend(
                f"{directory}/{path.relative_to(root_directory)}"
                for path in sorted(root_directory.rglob("*.tsv"))
            )
        return paths
    listed = subprocess.run(
        ["git", "ls-tree", "-r", "--name-only", ref, "--", legacy_file, directory],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if listed.returncode != 0:
        return []
    return [line for line in listed.stdout.splitlines() if line.endswith(".tsv")]


def outcome_paths(ref: str | None) -> list[str]:
    """The record's paths at `ref` (None = the working tree): the legacy single
    `outcomes.tsv`, or every `outcomes/**/*.tsv` shard (FIG-3727)."""
    return tree_paths(ref, OUTCOMES, OUTCOMES_DIR)


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
    paths = outcome_paths(base)
    if not paths:
        return None
    texts = [(path, show(base, path)) for path in paths]
    return parse_shards([(name, text) for name, text in texts if text is not None])


def census_rows(ref: str | None) -> str:
    """The census's full text at `ref` (None = the working tree): the legacy
    `census.tsv` or every `census/<kind>/<name>.tsv` shard, concatenated."""
    if ref is None:
        return "".join((ROOT / path).read_text() for path in tree_paths(None, CENSUS, CENSUS_DIR))
    return "".join(
        text
        for path in tree_paths(ref, CENSUS, CENSUS_DIR)
        if (text := show(ref, path)) is not None
    )


def new_refusal_codes(base: str) -> frozenset[str]:
    """The codes named by rejection rows the head's census adds over the base's."""
    head = rejected_rows(census_rows(None))
    base_rows = rejected_rows(census_rows(base))
    return frozenset(code for row, code in head.items() if base_rows.get(row) != code)


def kernel_exclusions(register: str) -> dict[str, str]:
    """Exact cases with a reviewed deviation, not diagnostic-wide waivers."""
    grounds, separator, table = register.partition("## Test262 exclusions")
    if not separator:
        raise ValueError("the deviation register has no Test262 exclusions table")
    codes = set(re.findall(r"^\| `([^`]+)` \|", grounds, re.MULTILINE))
    excluded: dict[str, str] = {}
    for line in table.splitlines():
        if not line.startswith("| `test/"):
            continue
        fields = [field.strip().strip("`") for field in line.strip("|").split("|")]
        if len(fields) != 2:
            raise ValueError(f"invalid Test262 exclusion: {line}")
        path, code = fields
        if path in excluded:
            raise ValueError(f"duplicate Test262 exclusion: {path}")
        if code not in codes:
            raise ValueError(f"{path}: unregistered deviation {code}")
        excluded[path] = code
    return excluded


def kernel_regressions(
    base: dict[str, tuple[str, str]],
    observed: dict[str, tuple[str, str]],
    excluded: dict[str, str],
) -> list[str]:
    """Gate 1: the complete kernel run preserves each main pass or names its
    deviation. Routing a missing feature never exempts it from this gate."""
    problems = []
    for path, code in sorted(excluded.items()):
        if base.get(path, (None, None))[0] != "pass":
            problems.append(f"{path}: {code} excludes a case main does not mark pass")
    for path in sorted(set(base) - set(observed)):
        problems.append(f"{path}: missing from the kernel run")
    for path in sorted(set(observed) - set(base)):
        problems.append(f"{path}: not in main's selection")
    for path, (outcome, qualifier) in sorted(observed.items()):
        if outcome not in {"pass", "refused", "fail", "harness"}:
            problems.append(f"{path}: unknown kernel outcome {outcome}")
        if base.get(path, (None, None))[0] == "pass" and outcome != "pass" and path not in excluded:
            problems.append(f"{path}: passed on main, kernel `{outcome} {qualifier}`")
    return problems


def check_kernel(base: dict[str, tuple[str, str]], outcomes: Path, register: Path) -> int:
    observed = parse_shards([(str(outcomes), outcomes.read_text())])
    excluded = kernel_exclusions(register.read_text())
    problems = kernel_regressions(base, observed, excluded)
    print("check_test262_ratchet: kernel compared with main's record:")
    for (outcome, qualifier), count in sorted(tallies(observed).items()):
        print(f"  {outcome}\t{qualifier}\t{count}")
    deviations = sum(
        path in excluded and base.get(path, (None, None))[0] == "pass" and outcome != "pass"
        for path, (outcome, _) in observed.items()
    )
    print(f"  recorded-pass\t-\t{sum(outcome == 'pass' for outcome, _ in base.values())}")
    print(f"  deviation\t-\t{deviations}")
    print(f"  regression\t-\t{len(problems)}")
    for problem in problems:
        print(f"  {problem}", file=sys.stderr)
    return int(bool(problems))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", required=True, help="the commit the head is compared with")
    parser.add_argument(
        "--kernel-outcomes", type=Path,
        help="complete kernel observation TSV emitted by test262_kernel (Gate 1)",
    )
    parser.add_argument(
        "--deviations", type=Path,
        default=ROOT / "crates/lash-dialect-typescript/deviations.md",
        help="kernel dialect's reviewed deviation register",
    )
    args = parser.parse_args()
    base = base_outcomes(args.base)
    if args.kernel_outcomes is not None:
        if base is None:
            parser.error("kernel comparison requires main's existing outcome record")
        return check_kernel(base, args.kernel_outcomes, args.deviations)
    if base is None:
        print("check_test262_ratchet: the outcomes record is new at this head; nothing to compare")
        return 0
    head = parse_shards([(path, (ROOT / path).read_text()) for path in outcome_paths(None)])
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
