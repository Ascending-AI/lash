#!/usr/bin/env python3
"""The substrate port ledger stays complete (ADR 0132 §14, FIG-5179).

`docs/testing/substrate-port-ledger.toml` gives every test that uses the
Restate server double a row: what it asserts, which lane ports or deletes it,
and the written disposition of every law that is not ported. The default mode
fails when:

- a `.rs` file that names `lash_restate_test`, a `Cargo.toml` that depends on
  `lash-restate-test`, or a test file of the Restate crates has no `*` row;
- the ledger is not TOML (a union merge can fold two rows into one table);
- a row is duplicated (a union merge keeps both sides' copies of a row) or
  malformed: an unknown class, lane, need or status, no laws, a
  disposition that does not fit its class or status, a `replace` row without
  the law it owes, or a reference to a planned law or ledger row that does
  not exist;
- a `deleted` row has no disposition;
- while the double's crate exists, a `todo` row names a path that no longer
  exists, or a test function its file no longer defines. Once the double is
  deleted (L10a), a `todo` row's file is gone by design and its laws are
  owed to its lane;
- the coverage summary at the top of the ledger is stale
  (`--write-summary` rewrites it).

`--final` (the end of the port) also fails on any `todo` row, on any planned
law without a landed path, on a `covered-by` or `replaced-by` path that does
not exist, and on any remaining reference to the double.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from collections import Counter
from pathlib import Path

LEDGER = Path("docs/testing/substrate-port-ledger.toml")
DOUBLE = "lash_restate_test"
DOUBLE_PACKAGE = "lash-restate-test"
DOUBLE_MANIFEST = Path("crates/lash-restate-test/Cargo.toml")
RESTATE_CRATES = ("crates/lash-restate/", "crates/lash-restate-test/")
RESTATE_TEST_DIRS = (
    "crates/lash-restate/src/tests/",
    "crates/lash-restate/tests/",
    "crates/lash-restate-test/tests/",
)
# The gate and its fixtures must spell the double's name.
GATE_FILES = frozenset({
    "scripts/check-substrate-port-ledger.py",
    "scripts/test_check_substrate_port_ledger.py",
})

TEST_ATTRIBUTE = re.compile(r"#\[(?:tokio::)?test\b")
CLASSES = ("mechanical", "semantic", "delete", "replace")
LANES = ("L1b", "L3", "L3s", "L4", "L5", "L6b", "L7", "L7b", "L7p", "L9b", "L9c", "L9d", "L9e", "L9f", "L9g", "L9h", "L11", "L8", "L10a", "L10b", "L10g", "L3t", "L9t", "I0")
NEEDS = ("turn", "round", "wait", "process", "cell", "multinode")
STATUSES = ("todo", "ported", "deleted")
REQUIRED = ("path", "test", "area", "needs", "class", "lane", "laws", "disposition", "status")
OPTIONAL = ("note", "owed")
IDENTIFIER = re.compile(r"[a-z_][a-z0-9_]*\Z")
FINAL_DISPOSITION = re.compile(r"(subject-deleted|covered-by:.+|replaced-by:.+)\Z")

SUMMARY_BEGIN = "# BEGIN SUMMARY (scripts/check-substrate-port-ledger.py --write-summary)"
SUMMARY_END = "# END SUMMARY"


def repository_files(root: Path) -> list[str]:
    listed = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=root, capture_output=True, check=True,
    ).stdout.decode().split("\0")
    return sorted(name for name in set(listed) if name and (root / name).is_file())


def required_files(root: Path, files: list[str]) -> list[str]:
    """Every file the ledger must cover."""
    required = []
    for name in files:
        if name in GATE_FILES:
            continue
        in_restate = name.startswith(RESTATE_CRATES)
        if name.endswith(".rs"):
            text = (root / name).read_text(encoding="utf-8", errors="replace")
            if in_restate:
                if name.startswith(RESTATE_TEST_DIRS) or TEST_ATTRIBUTE.search(text):
                    required.append(name)
            elif DOUBLE in text:
                required.append(name)
        elif name.endswith("/Cargo.toml") and not name.startswith("crates/lash-restate-test/"):
            if DOUBLE_PACKAGE in (root / name).read_text(encoding="utf-8"):
                required.append(name)
    return required


def double_references(root: Path, files: list[str]) -> list[str]:
    found = []
    for name in files:
        if name in GATE_FILES or name.startswith("docs/"):
            continue
        base = name.rsplit("/", 1)[-1]
        if not (name.endswith(".rs") or base in ("Cargo.toml", "Cargo.lock", "BUCK")):
            continue
        text = (root / name).read_text(encoding="utf-8", errors="replace")
        for number, line in enumerate(text.splitlines(), 1):
            if DOUBLE in line or DOUBLE_PACKAGE in line:
                found.append(f"{name}:{number}: the Restate server double is still referenced")
                break
    return found


def law_path(target: str) -> str:
    return target.split("::", 1)[0]


def check_reference(target: str, planned: dict, rows: list[dict]) -> str | None:
    if target.startswith("planned:"):
        if target.removeprefix("planned:") not in planned:
            return f"names planned law '{target}', which [planned] does not define"
        return None
    if target.startswith("row:"):
        path, _, test = target.removeprefix("row:").partition("::")
        for row in rows:
            if row["path"] == path and (test in ("", "*") or row["test"] in ("*", test)):
                return None
        return f"names ledger row '{target}', which the ledger does not have"
    if not target or target.startswith("/"):
        return f"names law path '{target}', which is not a repository path"
    return None


def check_row(index: int, row: dict, planned: dict, rows: list[dict]) -> list[str]:
    where = f"{LEDGER}: row {index} ({row.get('path', '?')}::{row.get('test', '?')})"
    problems = []
    missing = [key for key in REQUIRED if key not in row]
    unknown = [key for key in row if key not in REQUIRED + OPTIONAL]
    if missing:
        return [f"{where}: missing {', '.join(missing)}"]
    if unknown:
        problems.append(f"{where}: unknown fields {', '.join(unknown)}")
    if row["test"] != "*" and not IDENTIFIER.match(row["test"]):
        problems.append(f"{where}: test must be '*' or a test function name")
    if row["class"] not in CLASSES:
        problems.append(f"{where}: class must be one of {', '.join(CLASSES)}")
    if row["lane"] not in LANES:
        problems.append(f"{where}: lane must be one of {', '.join(LANES)}")
    if row["status"] not in STATUSES:
        problems.append(f"{where}: status must be one of {', '.join(STATUSES)}")
    if not isinstance(row["needs"], list) or any(need not in NEEDS for need in row["needs"]):
        problems.append(f"{where}: needs must be a list drawn from {', '.join(NEEDS)}")
    laws = row["laws"]
    if not isinstance(laws, list) or not laws or not all(isinstance(law, str) and law for law in laws):
        problems.append(f"{where}: laws must name at least one law or property")
    if not row["area"]:
        problems.append(f"{where}: area is empty")

    disposition, status, kind = row["disposition"], row["status"], row["class"]
    if status == "deleted":
        if not FINAL_DISPOSITION.match(disposition):
            problems.append(
                f"{where}: a deleted row needs a disposition: subject-deleted, "
                "covered-by:<law> or replaced-by:<new law>"
            )
    elif status == "ported":
        if kind not in ("mechanical", "semantic") or disposition != "port":
            problems.append(f"{where}: only a mechanical or semantic row is ported, with disposition 'port'")
    elif kind in ("mechanical", "semantic"):
        if disposition != "port":
            problems.append(f"{where}: a {kind} row's disposition is 'port' until it is ported")
    elif kind == "delete":
        if not re.match(r"(subject-deleted|covered-by:.+)\Z", disposition):
            problems.append(f"{where}: a delete row needs subject-deleted or covered-by:<law>")
    elif kind == "replace" and disposition != "owed" and not disposition.startswith("replaced-by:"):
        problems.append(f"{where}: a replace row is 'owed' until its law exists (replaced-by:<new law>)")
    if kind == "replace" and not row.get("owed"):
        problems.append(f"{where}: a replace row names the law it owes in 'owed'")
    if disposition.startswith(("covered-by:", "replaced-by:")):
        problem = check_reference(disposition.split(":", 1)[1], planned, rows)
        if problem:
            problems.append(f"{where}: {problem}")
    return problems


def check_planned(planned: dict) -> list[str]:
    problems = []
    for name, law in planned.items():
        if not isinstance(law, dict) or not law.get("owner") or not law.get("law"):
            problems.append(f"{LEDGER}: planned law {name} needs an owner and a law")
    return problems


def summary(ledger: dict) -> list[str]:
    rows = [row for row in ledger.get("row", []) if all(key in row for key in REQUIRED)]
    by_class = Counter(row["class"] for row in rows)
    by_lane = Counter(row["lane"] for row in rows)
    by_status = Counter(row["status"] for row in rows)
    files = {row["path"] for row in rows}
    laws = sum(len(row["laws"]) for row in rows if isinstance(row["laws"], list))
    lines = [
        SUMMARY_BEGIN,
        f"# {len(rows)} rows over {len(files)} files, {laws} laws.",
        "# By class: " + ", ".join(f"{name} {by_class[name]}" for name in CLASSES) + ".",
        "# By lane: " + ", ".join(f"{name} {by_lane[name]}" for name in LANES if by_lane[name]) + ".",
        "# By status: " + ", ".join(f"{name} {by_status[name]}" for name in STATUSES) + ".",
        "#",
        "# New laws owed (replace rows), by owning lane:",
    ]
    owed = sorted((row["lane"], row["path"], row["test"], row.get("owed", "")) for row in rows
                  if row["class"] == "replace" and row["status"] == "todo")
    for lane, path, test, law in owed:
        target = path if test == "*" else f"{path}::{test}"
        lines.append(f"# - {lane}: {target}")
        lines.append(f"#   {law}")
    if not owed:
        lines.append("# - none")
    lines.append(SUMMARY_END)
    return lines


def current_summary(text: str) -> list[str] | None:
    lines = text.splitlines()
    if SUMMARY_BEGIN not in lines or SUMMARY_END not in lines:
        return None
    return lines[lines.index(SUMMARY_BEGIN):lines.index(SUMMARY_END) + 1]


def write_summary(path: Path, text: str, ledger: dict) -> None:
    lines = text.splitlines()
    fresh = summary(ledger)
    if SUMMARY_BEGIN in lines and SUMMARY_END in lines:
        lines[lines.index(SUMMARY_BEGIN):lines.index(SUMMARY_END) + 1] = fresh
    else:
        lines[0:0] = fresh + [""]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def defines(text: str, test: str) -> bool:
    return re.search(rf"\b{re.escape(test)}\b", text) is not None


def violations(root: Path, final: bool) -> list[str]:
    path = root / LEDGER
    if not path.is_file():
        return [f"{LEDGER}: missing"]
    text = path.read_text(encoding="utf-8")
    try:
        ledger = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        return [
            f"{LEDGER}: {error}; the ledger merges with merge=union (.gitattributes), which can "
            "fold two rows added at one place under one [[row]] header: give each its own"
        ]
    rows = ledger.get("row", [])
    planned = ledger.get("planned", {})
    found = check_planned(planned)
    seen: set[tuple[str, str]] = set()
    for index, row in enumerate(rows, 1):
        problems = check_row(index, row, planned, rows)
        found.extend(problems)
        if problems:
            continue
        key = (row["path"], row["test"])
        if key in seen:
            found.append(f"{LEDGER}: row {index}: {row['path']}::{row['test']} has two rows")
        seen.add(key)
    if found:
        return found

    files = repository_files(root)
    covered = {row["path"] for row in rows if row["test"] == "*"}
    for row in rows:
        if row["test"] != "*" and row["path"] not in covered:
            found.append(f"{LEDGER}: {row['path']}::{row['test']} has no '*' row for its file")
    for name in required_files(root, files):
        if name not in covered:
            found.append(f"{name}: uses the Restate server double but has no ledger row")

    double_exists = (root / DOUBLE_MANIFEST).is_file()
    for row in rows:
        if row["status"] != "todo" or not double_exists:
            continue
        file = root / row["path"]
        if not file.is_file():
            found.append(f"{LEDGER}: {row['path']} no longer exists but its row is still todo")
        elif row["test"] != "*" and not defines(file.read_text(encoding="utf-8"), row["test"]):
            found.append(f"{LEDGER}: {row['path']} no longer defines {row['test']} but its row is still todo")

    if current_summary(text) != summary(ledger):
        found.append(
            f"{LEDGER}: the coverage summary is stale; run "
            "scripts/check-substrate-port-ledger.py --write-summary"
        )

    if final:
        for row in rows:
            if row["status"] == "todo":
                found.append(f"{LEDGER}: {row['path']}::{row['test']} is still todo ({row['lane']})")
            disposition = row["disposition"]
            if disposition.startswith(("covered-by:", "replaced-by:")):
                target = disposition.split(":", 1)[1]
                if not target.startswith(("planned:", "row:")) and not (root / law_path(target)).exists():
                    found.append(f"{LEDGER}: {row['path']}::{row['test']} names {target}, which does not exist")
        for name, law in planned.items():
            landed = law.get("path")
            if not landed or not (root / law_path(landed)).exists():
                found.append(f"{LEDGER}: planned law {name} ({law.get('owner')}) has not landed")
        found.extend(double_references(root, files))
    return found


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--final", action="store_true", help="also require every row ported or deleted")
    parser.add_argument("--write-summary", action="store_true", help="rewrite the coverage summary")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    if args.write_summary and (root / LEDGER).is_file():
        text = (root / LEDGER).read_text(encoding="utf-8")
        try:
            write_summary(root / LEDGER, text, tomllib.loads(text))
        except tomllib.TOMLDecodeError:
            pass  # reported below
    found = violations(root, args.final)
    for violation in found:
        print(violation, file=sys.stderr)
    if found:
        print(f"check-substrate-port-ledger: {len(found)} violation(s); see ADR 0132 §14", file=sys.stderr)
        return 1
    mode = "final" if args.final else "default"
    print(f"check-substrate-port-ledger: ok ({mode})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
