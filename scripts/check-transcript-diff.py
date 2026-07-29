#!/usr/bin/env python3
"""Surface durable-semantics changes in inline insta snapshots without blocking CI."""

from __future__ import annotations

import os
from pathlib import Path
import re
import subprocess
import sys
from itertools import zip_longest
from typing import NamedTuple


DEFAULT_RANGE = "origin/main...HEAD"
SNAPSHOT_OPEN = re.compile(r'@r(?P<hashes>#+)?"')
REV_TRANSITION = re.compile(r"\brev=\S+->\S+")
DURABLE_MARKERS = ("Checkpoint", "DurableEffect", "stored logical=", "ref (unchanged)")
HUNK_HEADER = re.compile(r"^@@ -(?P<old>\d+)(?:,\d+)? \+(?P<new>\d+)(?:,\d+)? @@")
# All Rust files: the `@r` snapshot-body requirement already suppresses noise,
# and any path allowlist is a silent-decay surface — a snapshot added in an
# excluded file (inline `#[cfg(test)]` modules live throughout src/) would be
# invisible forever and indistinguishable from "nothing to report".
RUST_TEST_PATHS = ("*.rs", ":(glob)**/*.rs")


class ChangedLine(NamedTuple):
    line_number: int
    text: str
    body: str


class Finding(NamedTuple):
    path: str
    old: ChangedLine | None
    new: ChangedLine | None


def snapshot_body(line: str, delimiter: str | None) -> tuple[str | None, str | None]:
    """Return the snapshot-body portion of a source line and the next delimiter."""
    if delimiter is None:
        opener = SNAPSHOT_OPEN.search(line)
        if opener is None:
            return None, None
        hashes = opener.group("hashes") or ""
        delimiter = '"' + hashes
        remainder = line[opener.end() :]
    else:
        remainder = line

    closing = remainder.find(delimiter)
    if closing >= 0:
        return remainder[:closing], None
    return remainder, delimiter


def is_durable(body: str) -> bool:
    return any(marker in body for marker in DURABLE_MARKERS) or bool(
        REV_TRANSITION.search(body)
    )


def classify_patch(patch: str) -> list[Finding]:
    """Classify a full-context unified diff."""
    findings: list[Finding] = []
    path = ""
    old_line = 0
    new_line = 0
    old_delimiter: str | None = None
    new_delimiter: str | None = None
    removed: list[ChangedLine] = []
    added: list[ChangedLine] = []

    def flush_changes() -> None:
        for old, new in zip_longest(removed, added):
            if (old and is_durable(old.body)) or (new and is_durable(new.body)):
                findings.append(Finding(path, old, new))
        removed.clear()
        added.clear()

    for diff_line in patch.splitlines():
        if diff_line.startswith("diff --git "):
            flush_changes()
            path = ""
            old_delimiter = None
            new_delimiter = None
            continue
        if diff_line.startswith("+++ "):
            candidate = diff_line[4:]
            path = candidate[2:] if candidate.startswith("b/") else candidate
            continue

        hunk = HUNK_HEADER.match(diff_line)
        if hunk:
            flush_changes()
            old_line = int(hunk.group("old"))
            new_line = int(hunk.group("new"))
            old_delimiter = None
            new_delimiter = None
            continue
        if not path or diff_line.startswith("--- "):
            continue

        prefix = diff_line[:1]
        text = diff_line[1:]
        if prefix == " ":
            flush_changes()
            _, old_delimiter = snapshot_body(text, old_delimiter)
            _, new_delimiter = snapshot_body(text, new_delimiter)
            old_line += 1
            new_line += 1
        elif prefix == "-":
            body, old_delimiter = snapshot_body(text, old_delimiter)
            if body is not None:
                removed.append(ChangedLine(old_line, text, body))
            old_line += 1
        elif prefix == "+":
            body, new_delimiter = snapshot_body(text, new_delimiter)
            if body is not None:
                added.append(ChangedLine(new_line, text, body))
            new_line += 1

    flush_changes()
    return findings


def render_findings(findings: list[Finding]) -> str:
    if not findings:
        return "No durable transcript snapshot lines changed."

    lines = [
        "Durable transcript snapshot lines changed; add a `Transcript:` "
        "justification sentence to the PR body (docs/agents/pr-style.md):"
    ]
    for finding in findings:
        old_location = (
            str(finding.old.line_number) if finding.old is not None else "none"
        )
        new_location = (
            str(finding.new.line_number) if finding.new is not None else "none"
        )
        lines.append(f"{finding.path}:{old_location}->{new_location}")
        lines.append(f"  - {finding.old.text if finding.old else '(none)'}")
        lines.append(f"  + {finding.new.text if finding.new else '(none)'}")
    return "\n".join(lines)


def append_summary(summary_path: str, findings: list[Finding]) -> None:
    if not findings:
        return
    with Path(summary_path).open("a", encoding="utf-8") as summary:
        summary.write(
            "## Durable transcript lines changed — needs a justification "
            "sentence in the PR body (pr-style.md)\n\n"
        )
        summary.write("```diff\n")
        for finding in findings:
            summary.write(f"# {finding.path}\n")
            summary.write(f"- {finding.old.text if finding.old else '(none)'}\n")
            summary.write(f"+ {finding.new.text if finding.new else '(none)'}\n")
        summary.write("```\n")


def requested_range(argv: list[str]) -> str:
    if not argv:
        return DEFAULT_RANGE
    if len(argv) == 1:
        return argv[0]
    if len(argv) == 2 and argv[0] == "--range":
        return argv[1]
    raise ValueError("usage: check-transcript-diff.py [--range] [REV_RANGE]")


def main(argv: list[str]) -> int:
    try:
        revision_range = requested_range(argv)
        result = subprocess.run(
            [
                "git",
                "diff",
                "--unified=100000",
                "--no-color",
                "--no-ext-diff",
                revision_range,
                "--",
                *RUST_TEST_PATHS,
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        findings = classify_patch(result.stdout)
        print(render_findings(findings))
        if summary_path := os.environ.get("GITHUB_STEP_SUMMARY"):
            append_summary(summary_path, findings)
    except Exception as error:
        print(
            f"warning: durable transcript classifier could not run: {error}",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
