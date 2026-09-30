#!/usr/bin/env python3
"""Check ADR narration, numbered citations, and the live README index.

This gate uses the Python standard library. Every rule enforces on main;
the explicit flags keep the report-only mode reachable for staged rollouts.
Self-tests exercise both modes, so warning mode does not weaken the
regression evidence.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
import os
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
ENFORCE_NARRATION = True
ENFORCE_CITATIONS = True
SCAN_ROOTS = ("crates", "scripts", "runbooks", "examples", "docs", "tests")
SKIP_DIRS = {".git", "node_modules", "target", ".tgt", ".venv", "__pycache__"}
INDEX_START = "<!-- adr-index:start -->"
INDEX_END = "<!-- adr-index:end -->"
README = "docs/adr/README.md"
CONVENTION_BAN = (
    "- Do not add amendment sections or blocks, superseded-by or supersedes notes, "
    "previously/formerly/used-to/no-longer/was-retired/originally narration, "
    "dated change notes, or ticket-by-ticket history."
)

# Each exception names the exact present-tense use and its reason. README's
# rule names the prohibited vocabulary; it does not narrate a design change.
# A size comparison says nothing about design history.
NARRATION_ALLOWLIST = (
    (README, re.compile(re.escape(CONVENTION_BAN)), "the convention names its bans"),
    (None, re.compile(r"\bno longer than\b", re.I), "a present-tense size bound"),
)
HISTORY = re.compile(
    r"\b(?:amendments?|amended|superseded|supersedes|previously|formerly|originally)\b"
    r"|\b(?:no[\s-]+longer|used[\s-]+to|was[\s-]+retired)\b", re.I
)
DATED_NOTE = re.compile(
    r"(?:^\s*(?:#{1,6}\s+|[-*>]\s*|\*\*)?(?:date\s*:\s*)?\d{4}-\d{2}-\d{2}\b"
    r"|\b(?:updated|changed|added|removed|implemented|adopted|revised|landed)\b"
    r"[^\n]{0,100}\b\d{4}-\d{2}-\d{2}\b)", re.I | re.M
)
ADR_FILE = re.compile(r"^(\d{4})-[^/]+\.md$")
ADR_CITATION = re.compile(r"\bADR(?:[ \t]|\r?\n[ \t]*(?://[/!]?|#|\*)?[ \t]*)+(\d{4})\b")
SECTION = re.compile(r"§\s*(\d+(?:\.\d+)*)")
HEADING = re.compile(r"^ {0,3}#{1,6}\s+(.+?)\s*#*\s*$")
SECTION_HEADING = re.compile(r"^(?:§\s*)?(\d+(?:\.\d+)*)(?=[.):\s]|$)")
# Formatting between a citation and its section, including a Markdown link
# and comment leaders when a source comment wraps onto the next line.
SECTION_GAP = re.compile(r"^(?:\]\([^\n)]*\)|[\s*`_(\[\]:,]|//[/!]?|#)*$")


@dataclass
class Adr:
    path: Path
    title: str
    sections: set[str]


@dataclass
class Result:
    narration: list[str] = field(default_factory=list)
    citations: list[str] = field(default_factory=list)
    index: list[str] = field(default_factory=list)
    adr_count: int = 0
    citation_count: int = 0
    section_count: int = 0


def headings(text: str) -> list[str]:
    found = []
    fence = None
    for line in text.splitlines():
        marker = re.match(r"^\s*(`{3,}|~{3,})", line)
        if marker:
            token = marker.group(1)
            if fence is None:
                fence = token
            elif token[0] == fence[0] and len(token) >= len(fence):
                fence = None
            continue
        if fence is None and (match := HEADING.match(line)):
            found.append(match.group(1))
    return found


def load_adrs(root: Path, result: Result) -> dict[str, Adr]:
    adrs = {}
    for path in sorted((root / "docs/adr").glob("*.md")):
        if path.name == "README.md":
            continue
        match = ADR_FILE.fullmatch(path.name)
        if not match:
            result.index.append(f"{path.relative_to(root)}:1: expected NNNN-title.md")
            continue
        number = match.group(1)
        if number in adrs:
            result.index.append(f"{path.relative_to(root)}:1: duplicate ADR number {number}")
        titles = headings(path.read_text(encoding="utf-8"))
        if not titles:
            result.index.append(f"{path.relative_to(root)}:1: ADR has no heading")
        title = re.sub(r"^(?:ADR\s+)?" + number + r"\s*[:.-]\s*", "", titles[0] if titles else path.stem)
        sections = {m.group(1) for heading in titles if (m := SECTION_HEADING.match(heading))}
        adrs[number] = Adr(path, title, sections)
    result.adr_count = len(adrs)
    return adrs


def index_text(adrs: dict[str, Adr]) -> str:
    rows = ["| Number | Decision |", "| --- | --- |"]
    for number, adr in sorted(adrs.items()):
        title = adr.title.replace("|", "\\|")
        rows.append(f"| {number} | [{title}]({adr.path.name}) |")
    return "\n".join(rows)


def check_index(root: Path, adrs: dict[str, Adr], result: Result, write: bool) -> None:
    path = root / README
    if not path.exists():
        result.index.append(f"{README}:1: missing convention and index")
        return
    text = path.read_text(encoding="utf-8")
    if text.count(INDEX_START) != 1 or text.count(INDEX_END) != 1:
        result.index.append(f"{README}:1: expected exactly one pair of index markers")
        return
    start = text.index(INDEX_START) + len(INDEX_START)
    end = text.index(INDEX_END)
    if end < start:
        result.index.append(f"{README}:1: index markers are out of order")
        return
    expected = "\n" + index_text(adrs) + "\n"
    if text[start:end] != expected:
        if write:
            path.write_text(text[:start] + expected + text[end:], encoding="utf-8")
        else:
            line = text.count("\n", 0, start) + 1
            result.index.append(f"{README}:{line}: live ADR index differs; run python3 scripts/check_adr_current.py --write-index")


def text_files(root: Path):
    for directory in SCAN_ROOTS:
        for base, dirs, files in os.walk(root / directory):
            dirs[:] = sorted(d for d in dirs if d not in SKIP_DIRS and not d.startswith("."))
            for name in sorted(files):
                path = Path(base) / name
                if path.is_symlink():
                    continue
                raw = path.read_bytes()
                if b"\0" in raw:
                    continue
                try:
                    yield path, raw.decode("utf-8")
                except UnicodeDecodeError:
                    continue


def check_narration(relative: str, text: str, result: Result) -> None:
    # Mask only allowlisted spans, preserving offsets and line numbers.
    masked = text
    for path, pattern, reason in NARRATION_ALLOWLIST:
        assert reason
        if path is None or path == relative:
            masked = pattern.sub(lambda m: re.sub(r"[^\n]", " ", m.group()), masked)
    for pattern in (HISTORY, DATED_NOTE):
        for match in pattern.finditer(masked):
            line = masked.count("\n", 0, match.start()) + 1
            result.narration.append(f"{relative}:{line}: history narration {match.group().strip()!r}")


def check_citations(root: Path, path: Path, text: str, adrs: dict[str, Adr], result: Result) -> None:
    relative = path.relative_to(root).as_posix()
    references = list(ADR_CITATION.finditer(text))
    associated = set()

    def check_section(number: str, section: str, offset: int) -> None:
        result.section_count += 1
        if number in adrs and section not in adrs[number].sections:
            line = text.count("\n", 0, offset) + 1
            result.citations.append(f"{relative}:{line}: ADR {number} has no section §{section}")

    for index, match in enumerate(references):
        number = match.group(1)
        result.citation_count += 1
        line = text.count("\n", 0, match.start()) + 1
        if number not in adrs:
            result.citations.append(f"{relative}:{line}: ADR {number} has no file")
        end = references[index + 1].start() if index + 1 < len(references) else len(text)
        tail = text[match.end():end]
        section = SECTION.search(tail)
        if section and SECTION_GAP.fullmatch(tail[:section.start()]):
            offset = match.end() + section.start()
            associated.add(offset)
            check_section(number, section.group(1), offset)
            # An explicit list or range after the first section shares its ADR.
            cursor = section.end()
            more = re.compile(r"\s*(?:\([^)]*\)\s*)?(?:,|and|to|/|[-–])\s*§?\s*(\d+(?:\.\d+)*)")
            while following := more.match(tail, cursor):
                associated.add(match.end() + following.start() + following.group().find("§"))
                check_section(number, following.group(1), match.end() + following.start())
                cursor = following.end()

    # Bare sections name the current ADR, except an explicit "its", "there"
    # or "of that" referring to a nearby ADR in the same paragraph.
    own = ADR_FILE.fullmatch(path.name) if path.parent == root / "docs/adr" else None
    reference_index = -1
    for match in SECTION.finditer(text):
        if match.start() in associated:
            continue
        while reference_index + 1 < len(references) and references[reference_index + 1].end() <= match.start():
            reference_index += 1
        target = own.group(1) if own else None
        if reference_index >= 0:
            reference = references[reference_index]
            gap = text[reference.end():match.start()]
            if "\n\n" not in gap and (
                re.search(r"\bits\s*$", gap)
                or re.match(r"\s+(?:there|of that)\b", text[match.end():])
            ):
                target = reference.group(1)
        if target is not None:
            check_section(target, match.group(1), match.start())


def check(root: Path, *, write_index: bool = False) -> Result:
    result = Result()
    adrs = load_adrs(root, result)
    check_index(root, adrs, result, write_index)
    for path, text in text_files(root):
        relative = path.relative_to(root).as_posix()
        if relative.startswith("docs/adr/"):
            check_narration(relative, text, result)
        check_citations(root, path, text, adrs, result)
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--write-index", action="store_true")
    parser.add_argument("--enforce-narration", action="store_true", default=ENFORCE_NARRATION)
    parser.add_argument("--enforce-citations", action="store_true", default=ENFORCE_CITATIONS)
    args = parser.parse_args(argv)
    result = check(args.root.resolve(), write_index=args.write_index)
    failed = False
    for label, findings, enforce in (
        ("narration", result.narration, args.enforce_narration),
        ("citations", result.citations, args.enforce_citations),
        ("index", result.index, True),
    ):
        mode = "enforcing" if enforce else "report-only"
        print(f"ADR {label}: {len(findings)} findings ({mode})")
        for finding in findings:
            print(f"{'ERROR' if enforce else 'WARN'} {finding}")
        failed |= enforce and bool(findings)
    print(f"Checked {result.adr_count} ADRs, {result.citation_count} citations, {result.section_count} section references")
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
