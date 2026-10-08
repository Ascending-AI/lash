#!/usr/bin/env python3
"""Refuse any mention of Restate in the tracked tree (ruling #76, FIG-5192).

Lash's own runtime is its only durable engine (ADR 0132 §1). Code, comments,
identifiers and docs describe that engine, so a new mention of the deleted
one, or an English "restate" that reads as one, is refused. The check fails
on every case-insensitive `restate` in a tracked path or in a tracked text
file's contents, except:

- a camelCase join such as `FixtureState`, whose `reState` is two words;
- this gate's own name where its wiring invokes it;
- the permanent exceptions below, each with its reason;
- a replaced or retired ADR, whose `## Status` begins `Replaced by` or
  `Retired:`, and the README index row linking to it: the file is a short
  note kept while code cites it (docs/adr/README.md);
- a pending exception, a mention another lane removes with the code it owns.
  A pending exception that no longer matches anything fails, so its owner
  deletes the row in the change that removes the mention.

Run it from anywhere: `python3 scripts/check-no-restate.py`.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import fnmatch
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MENTION = re.compile("restate", re.I)
# `FixtureState`, `StoreState`: the match straddles a camelCase word boundary.
CAMEL_JOIN = "reState"
# The gate's own name, where CI, pre-commit and the push gate invoke it.
GATE_NAME = re.compile(r"(?:test_)?check[-_]no[-_]restate(?:\.py)?|\bno-restate\b|No Restate mention")
ADR_FILE = re.compile(r"docs/adr/\d{4}-[^/]+\.md")
ADR_README = "docs/adr/README.md"
SETTLED_STATUS = re.compile(r"^(Replaced by\b|Retired:)")


@dataclass(frozen=True)
class Allowance:
    """Paths (fnmatch globs) whose mentions are allowed, optionally only on
    lines matching `line`."""

    paths: tuple[str, ...]
    reason: str
    line: re.Pattern[str] | None = None


PERMANENT = (
    Allowance(("docs/architecture/adr-reset-2026-10.md",),
               "the ADR reset's classification of every decision against ADR 0132"),
    Allowance(("docs/testing/substrate-port-ledger.toml",),
               "the substrate port ledger: the disposition of every deleted test"),
    Allowance(("scripts/check-substrate-port-ledger.py",
                "scripts/test_check_substrate_port_ledger.py"),
               "the ledger's checker and its self-tests name what the ledger accounts for"),
    Allowance(("docs/perf/restate-baseline-2026-10.md",
                "docs/perf/restate-baseline-2026-10.json",
                "docs/perf/durable-vs-restate-2026-10.md"),
               "the pre-deletion performance baseline and the durable engine's comparison with it"),
    Allowance(("CHANGELOG.md", "*/CHANGELOG.md"), "release history"),
    Allowance(("docs/operations/figments-migration-1.0.md",),
              "the Figments migration checklist names the sites in that repository that "
              "still use the deleted engine"),
    Allowance(("scripts/check-no-restate.py", "scripts/test_check_no_restate.py"),
               "this gate and its self-tests"),
    Allowance(("crates/lash-core/src/protocol_copy.rs",
                "crates/lash-protocol-rlm/src/prompt_sections/tests.rs",
                "crates/lash-protocol-standard/src/prompt_tests.rs"),
               "the English verb in model-facing guidance, whose bytes are behaviour",
               re.compile(r"do not restate conclusions")),
)

PENDING = (
    Allowance(("tools/buck2/native-tools-lock.json",
                "tools/buck2/bootstrap_native_tools.py", "tools/buck2/README.md"),
               "the native engine-server pin only the deleted workbench E2E driver built "
               "is unbuilt; it goes with the build-tool pins it sits beside"),
    Allowance(("tools/buck2/test-shard-weights.json",),
               "a measured table nobody hand-edits; the next `tools/buck2/shard_weights.py "
               "--refresh` drops the deleted case's name",
               re.compile(r"foreground_trace_carries_the_enclosing_restate_process_invocation")),
)


@dataclass(frozen=True)
class Mention:
    path: str
    line: int
    text: str

    def render(self) -> str:
        where = f"{self.path}:{self.line}" if self.line else self.path
        text = GATE_NAME.sub("", self.text)
        first = next(m for m in MENTION.finditer(text) if m.group() != CAMEL_JOIN)
        start = max(first.start() - 60, 0)
        return f"{where}: a Restate mention: {text[start:first.end() + 60].strip()}"


def is_mention(text: str) -> bool:
    return any(match.group() != CAMEL_JOIN for match in MENTION.finditer(GATE_NAME.sub("", text)))


def git(root: Path, *args: str) -> str:
    completed = subprocess.run(["git", *args], cwd=root, check=False,
                               capture_output=True, text=True, errors="replace")
    # `git grep` exits 1 when nothing matches.
    if completed.returncode not in (0, 1):
        raise SystemExit(f"git {' '.join(args[:2])} failed: {completed.stderr.strip()}")
    return completed.stdout


def mentions(root: Path) -> list[Mention]:
    """Every mention in a tracked path or a tracked text file's working copy."""
    found = [Mention(path, 0, path)
             for path in git(root, "ls-files", "-z").split("\0") if path and is_mention(path)]
    for record in git(root, "grep", "-I", "-n", "-i", "-z", "-e", "restate").splitlines():
        path, line, text = record.split("\0", 2)
        if is_mention(text):
            found.append(Mention(path, int(line), text))
    return found


def settled_adrs(root: Path) -> set[str]:
    """Replaced or retired ADR files, by the first line of their Status."""
    settled = set()
    for path in sorted((root / "docs/adr").glob("[0-9][0-9][0-9][0-9]-*.md")):
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
        for index, line in enumerate(lines):
            if line.strip() == "## Status":
                status = next((rest.strip() for rest in lines[index + 1:] if rest.strip()), "")
                if SETTLED_STATUS.match(status):
                    settled.add(path.relative_to(root).as_posix())
                break
    return settled


def covers(exception: Allowance, mention: Mention) -> bool:
    if not any(fnmatch.fnmatchcase(mention.path, glob) for glob in exception.paths):
        return False
    return exception.line is None or bool(exception.line.search(mention.text))


def allowed_by_adr(settled: set[str], mention: Mention) -> bool:
    if mention.path in settled:
        return True
    return mention.path == ADR_README and any(
        f"({Path(adr).name})" in mention.text for adr in settled
    )


def check(root: Path, pending: tuple[Allowance, ...] = PENDING) -> list[str]:
    settled = settled_adrs(root)
    used: set[str] = set()
    errors = []
    for mention in mentions(root):
        if allowed_by_adr(settled, mention) or any(covers(a, mention) for a in PERMANENT):
            continue
        owner = next((a for a in pending if covers(a, mention)), None)
        if owner is None:
            errors.append(mention.render())
            continue
        used.update(glob for glob in owner.paths if fnmatch.fnmatchcase(mention.path, glob))
    for allowance in pending:
        for glob in allowance.paths:
            if glob not in used:
                errors.append(f"{glob}: stale pending exception, no mention left; delete it "
                              f"from PENDING ({allowance.reason})")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    errors = check(args.root.resolve())
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        print(f"no-restate: {len(errors)} finding(s); describe the durable engine instead "
              "(ADR 0132)", file=sys.stderr)
        return 1
    print("no-restate: no Restate mention outside the exceptions")
    return 0


if __name__ == "__main__":
    sys.exit(main())
