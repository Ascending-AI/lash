#!/usr/bin/env python3
"""Collect and validate curated release notes from commit messages.

Convention: a commit that should contribute release notes carries one or more
`Release-Notes:` lines in its body. A note may start on the marker line or the
following line. It ends at the next `Release-Notes:` line, the next squash
subject line (`* `), or the end of the message.

Notes may start with `Breaking:`, `Added:`, `Fixed:`, `Changed:`, or
`Internal:` (case-insensitive). The legacy `Fixed - ` and `Changed - ` forms
remain accepted. Publication groups categorized notes in that order, followed
by uncategorized historical notes under `Other`.

The `check-pr` command requires a categorized note when a range changes
anything under `crates/`, and rejects uncategorized notes in every PR. Use an
`Internal:` note, with a short explanation, as the escape hatch for a product
change that intentionally has no public-facing release note.

The release pipeline uses two entry points:
  - the manually dispatched release workflow runs `collect --require` for the
    selected green main commit before anything is published.
  - the publish job runs `collect --end <sha> --out <file>` and feeds the file
    to the GitHub release body before tagging that SHA (the auto-generated
    commit list is appended below it).

The automated post-release `docs: stamp release <version>` commit carries the
required categorized trailer for repository history, but is excluded from
collection so it cannot satisfy the next release's curated-notes gate by itself.

Uses only the Python standard library, like the sibling release scripts.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

MARKER_RE = re.compile(r"^Release-Notes:(.*)$")
SQUASH_SUBJECT_RE = re.compile(r"^\*\s")
RECORD_SEPARATOR = "\x1e"
FIELD_SEPARATOR = "\x1f"
AUTOMATED_DOCS_STAMP_SUBJECT_RE = re.compile(r"^docs: stamp release \S+$")
CATEGORIES = ("Breaking", "Added", "Fixed", "Changed", "Internal")
CATEGORY_COLON_RE = re.compile(
    r"^(Breaking|Added|Fixed|Changed|Internal):\s*(.*)$", re.IGNORECASE
)
LEGACY_CATEGORY_RE = re.compile(r"^(Fixed|Changed)\s+-\s+(.*)$", re.IGNORECASE)
MALFORMED_CATEGORY_ERROR = (
    "every release note must start with one of: "
    "Breaking:, Added:, Fixed:, Changed:, Internal:"
)


def previous_tag(end: str) -> str | None:
    """The nearest release tag reachable from (and not equal to) `end`.

    Graph ancestry, not version sorting: the repo's tag namespace contains
    tags from unrelated history lines, so "previous release" means the
    nearest `v*` ancestor on this line.
    """
    result = subprocess.run(
        ["git", "describe", "--tags", "--abbrev=0", "--match", "v*", f"{end}^"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        return None
    tag = result.stdout.strip()
    return tag or None


def end_is_released(end: str) -> bool:
    """True when `end` resolves to a commit that already carries a release tag.

    Keeps `--require` reruns idempotent: a re-run on an already-tagged commit
    has an empty range by definition and must not fail the gate.
    """
    result = subprocess.run(
        ["git", "tag", "--list", "v*", "--points-at", end],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    return result.returncode == 0 and bool(result.stdout.strip())


def extract_notes(body: str) -> list[str]:
    """Extract every bounded release-note block from one commit body."""
    lines = body.splitlines()
    notes: list[str] = []
    index = 0
    while index < len(lines):
        marker = MARKER_RE.match(lines[index])
        if marker is None:
            index += 1
            continue

        note_lines: list[str] = []
        inline = marker.group(1).strip()
        if inline:
            note_lines.append(inline)
        index += 1

        while index < len(lines):
            line = lines[index]
            if MARKER_RE.match(line) or SQUASH_SUBJECT_RE.match(line):
                break
            note_lines.append(line)
            index += 1

        note = "\n".join(note_lines).strip()
        if note:
            notes.append(note)

    return notes


def categorize_note(note: str) -> tuple[str | None, str]:
    """Return the normalized category and note text without its prefix."""
    first_line, separator, remainder = note.partition("\n")
    match = CATEGORY_COLON_RE.match(first_line)
    if match is None:
        match = LEGACY_CATEGORY_RE.match(first_line)
    if match is None:
        return None, note

    category = match.group(1).capitalize()
    first_text = match.group(2).strip()
    text = "\n".join(
        part for part in (first_text, remainder if separator else "") if part
    ).strip()
    return category, text


def render_notes(notes: list[str]) -> str:
    """Group notes by category and render the publication Markdown."""
    grouped: dict[str, list[str]] = {category: [] for category in CATEGORIES}
    grouped["Other"] = []
    for note in notes:
        category, text = categorize_note(note)
        grouped[category or "Other"].append(text)

    sections: list[str] = []
    for category in (*CATEGORIES, "Other"):
        category_notes = grouped[category]
        if category_notes:
            sections.append(f"## {category}\n\n" + "\n\n".join(category_notes))
    return "\n\n".join(sections).strip()


def commit_messages(range_spec: str) -> list[tuple[str, str]]:
    """Return (subject, body) pairs in chronological order for a git range."""
    result = subprocess.run(
        [
            "git",
            "log",
            "--reverse",
            f"--format=%s{FIELD_SEPARATOR}%B{RECORD_SEPARATOR}",
            range_spec,
        ],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    messages: list[tuple[str, str]] = []
    for record in result.stdout.split(RECORD_SEPARATOR):
        if not record.strip():
            continue
        subject, separator, body = record.partition(FIELD_SEPARATOR)
        if not separator:
            raise ValueError("git log record is missing its subject/body separator")
        messages.append((subject.strip(), body.strip("\n")))
    return messages


def commit_bodies(range_spec: str) -> list[str]:
    """Return commit bodies in chronological order for an arbitrary range."""
    return [body for _, body in commit_messages(range_spec)]


def is_automated_docs_stamp(subject: str) -> bool:
    """Whether a commit is the release workflow's mechanical docs-pin update."""
    return AUTOMATED_DOCS_STAMP_SUBJECT_RE.fullmatch(subject) is not None


def collect_notes_for_range(range_spec: str) -> list[str]:
    """Collect all notes from an arbitrary git range, oldest first."""
    notes: list[str] = []
    for subject, body in commit_messages(range_spec):
        if is_automated_docs_stamp(subject):
            continue
        notes.extend(extract_notes(body))
    return notes


def collect_notes(end: str) -> list[str]:
    """Collect notes for the previous-release-tag-to-end range."""
    prev = previous_tag(end)
    range_spec = f"{prev}..{end}" if prev else end
    return collect_notes_for_range(range_spec)


def changed_paths(range_spec: str) -> list[str]:
    """Return paths changed in an arbitrary git range."""
    result = subprocess.run(
        ["git", "diff", "--name-only", "--diff-filter=ACDMRTUXB", range_spec],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    )
    return [line for line in result.stdout.splitlines() if line]


def validate_pr_notes(paths: list[str], notes: list[str]) -> list[str]:
    """Validate the product-note requirement and every note's category."""
    errors: list[str] = []
    if any(path.startswith("crates/") for path in paths) and not notes:
        errors.append(
            "product changes under `crates/` require at least one categorized "
            "release note"
        )
    if any(
        category is None or not text
        for category, text in (categorize_note(note) for note in notes)
    ):
        errors.append(MALFORMED_CATEGORY_ERROR)
    return errors


def marker_count(bodies: list[str]) -> int:
    """Count marker lines, including empty or otherwise malformed notes."""
    return sum(
        1
        for body in bodies
        for line in body.splitlines()
        if MARKER_RE.match(line) is not None
    )


def write_pr_summary(path: Path, rendered: str) -> None:
    """Append the exact PR release-note preview to a GitHub step summary."""
    preview = rendered or "No release notes in this PR."
    with path.open("a", encoding="utf-8") as summary:
        summary.write(f"## Release notes preview (this PR)\n\n{preview}\n")


def write_check_pr_failure(range_spec: str, errors: list[str]) -> None:
    """Write an actionable category-and-shape guide for a failed PR check."""
    sys.stderr.write(f"release notes check failed for {range_spec}:\n")
    for error in errors:
        sys.stderr.write(f"- {error}\n")
    sys.stderr.write(
        "\nAdd a categorized note to a commit body in this PR.\n"
        "Accepted categories: Breaking, Added, Fixed, Changed, Internal.\n"
        "Use Internal for a product change with no public-facing release note.\n"
        "\nAccepted inline shape:\n"
        "Release-Notes: Fixed: Describe the user-visible change.\n"
        "\nAccepted block shape:\n"
        "Release-Notes:\n"
        "Fixed: Describe the user-visible change.\n"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    collect = sub.add_parser("collect", help="collect notes for a release range")
    collect.add_argument(
        "--end",
        default="HEAD",
        help="end of the release range: a release tag or HEAD (default)",
    )
    collect.add_argument(
        "--out",
        type=Path,
        help="write collected notes to this file (always written; may be empty)",
    )
    collect.add_argument(
        "--require",
        action="store_true",
        help="fail when the range contains no Release-Notes sections",
    )

    check_pr = sub.add_parser(
        "check-pr", help="validate and preview notes for an arbitrary PR range"
    )
    check_pr.add_argument(
        "--range",
        dest="range_spec",
        required=True,
        help="git range from the PR merge base through its head",
    )
    check_pr.add_argument(
        "--summary",
        type=Path,
        help="append the rendered preview to this GitHub step-summary file",
    )

    args = parser.parse_args()

    if args.command == "check-pr":
        bodies = commit_bodies(args.range_spec)
        notes = [note for body in bodies for note in extract_notes(body)]
        rendered = render_notes(notes)
        if args.summary:
            write_pr_summary(args.summary, rendered)
        errors = validate_pr_notes(changed_paths(args.range_spec), notes)
        if marker_count(bodies) != len(notes) and MALFORMED_CATEGORY_ERROR not in errors:
            errors.append(MALFORMED_CATEGORY_ERROR)
        if errors:
            write_check_pr_failure(args.range_spec, errors)
            return 2
        if rendered and not args.summary:
            sys.stdout.write(rendered + "\n")
        return 0

    notes = collect_notes(args.end)
    rendered = render_notes(notes)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(rendered + "\n" if rendered else "", encoding="utf-8")
    if args.require and not rendered and args.end == "HEAD" and end_is_released("HEAD"):
        sys.stderr.write("HEAD already carries a release tag; nothing new to gate.\n")
        return 0
    if args.require and not rendered:
        prev = previous_tag(args.end)
        sys.stderr.write(
            "no release notes found in "
            f"{prev or 'history start'}..{args.end}.\n"
            "Add a `Release-Notes:` section to at least one commit body.\n"
        )
        return 2
    if rendered and not args.out:
        sys.stdout.write(rendered + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
