#!/usr/bin/env python3
"""Reject retired mechanisms in current documentation and source comments."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import subprocess


@dataclass(frozen=True)
class RetiredTerm:
    term: str
    pattern: str
    commit: str

    @property
    def marker(self) -> str:
        return f"Retired in {self.commit}"


# The canonical retirement inventory and the replacement matrix live in
# docs/agents/test-backends.md. A marker applies to its paragraph, or to a
# Markdown section whose heading explicitly says Historical. ADRs describe
# current design and have no history exemption. A status elsewhere in a file
# never grants a whole-file exception.
TERMS = (
    RetiredTerm("in-memory store", r"\bin[ -]memory\s+stores?\b", "60e0e86b2a"),
    RetiredTerm("local process registry", r"\b(?:local|in[ -]memory)\s+process\s+registr(?:y|ies)\b", "60e0e86b2a"),
    RetiredTerm("native effect host", r"\bnative\s+effect\s+host\b|\bNativeEffectHost\b|\blash-core-memory\b", "60e0e86b2a"),
    RetiredTerm("store-delegated turn control", r"\bstore[ -]delegated\s+turn\s+control\b", "60e0e86b2a"),
    RetiredTerm("store-journal host", r"\bstore[ -](?:journal(?:ed)?|backed)\s+(?:turn\s+)?host\b|\bstore\s+journal\b", "476264fbea"),
    RetiredTerm("PostgreSQL effect engine", r"\bpostgres(?:ql)?\s+effect\s+engine\b", "4f03596847"),
    RetiredTerm("SQLite effect engine", r"\b(?:sqlite|sql)\s+effect[ -]engine\b", "476264fbea"),
    RetiredTerm("runtime-owned tool-intent admission", r"\bruntime[ -]owned\s+tool[ -]intent\s+(?:admission|first[ -]submission)\b", "0417b7f48b"),
    RetiredTerm("local rewind", r"\blocal\s+rewind\b", "9c1bbd2189"),
    RetiredTerm("runtime-operation effect journal", r"\bruntime[ -]operation\s+effect\s+journals?\b", "caa1f7efe3"),
    RetiredTerm("orchestrating tool", r"\borchestrating\s+tools?\b", "501f323f61"),
    RetiredTerm("stopped partial", r"\bstopped[ -]partials?\b", "dddace81f0"),
    RetiredTerm("exclusively-owned copies", r"\bexclusively[ -]owned[ -](?:recursive[ -])?cop(?:y|ies)\b", "c51e616528"),
)


def prose(line: str) -> str:
    # Link destinations and source paths can retain historical identifiers.
    line = re.sub(r"\]\([^)]*\)", "]", line)
    line = re.sub(r"`[^`\n]*[/][^`\n]*`", "", line)
    return line.replace("`", "").replace("**", "")


def check_text(path: Path, body: str) -> list[str]:
    markdown = path.suffix == ".md"
    adr = path.parts[:2] == ("docs", "adr")
    lines = body.splitlines()
    violations: list[str] = []
    sections: list[tuple[int, str]] = []
    paragraph: list[tuple[int, str]] = []

    def flush() -> None:
        if not paragraph:
            return
        text = prose("\n".join(line for _, line in paragraph))
        context = text + "\n" + "\n".join(marker for _, marker in sections)
        for term in TERMS:
            for match in re.finditer(term.pattern, text, re.IGNORECASE):
                # SQLite must qualify this occurrence, not another store in
                # the sentence or a neighbouring paragraph.
                if term.term == "in-memory store" and re.search(
                    r"\bSQLite\s+(?:named\s+)?$", text[:match.start()], re.IGNORECASE,
                ):
                    continue
                if not adr and term.marker.casefold() in " ".join(context.split()).casefold():
                    continue
                line_number = paragraph[0][0] + text[:match.start()].count("\n")
                correction = "use today's matrix; ADRs describe current design" if adr else f"use today's matrix or mark history '{term.marker}'"
                violations.append(f"{path}:{line_number}: {term.term}: {correction}")
        paragraph.clear()

    for number, line in enumerate(lines, 1):
        heading = re.match(r"^(#{1,6})\s+(.*)", line) if markdown else None
        if heading:
            flush()
            level, title = len(heading[1]), prose(heading[2])
            sections[:] = [(depth, marker) for depth, marker in sections if depth < level]
            if level >= 2 and re.search(r"\bHistorical\b", title, re.IGNORECASE):
                sections.append((level, title))
        if markdown:
            selected = line
        else:
            # Only prose comments, never executable identifiers, messages or
            # planted test strings. Rust block comments are included too.
            comment = re.match(r"\s*(?://[/!]?|#|/\*|\*(?!/))\s?(.*)", line)
            selected = comment[1] if comment else ""
            if not comment and path.suffix == ".rs":
                # Discard quoted code before finding a trailing Rust comment.
                # This keeps planted strings and URL literals out of prose.
                code = re.sub(r'"(?:\\.|[^"\\])*"', '""', line)
                trailing = re.search(r"//[/!]?\s?(.*)", code)
                selected = trailing[1] if trailing else ""
        if not selected.strip():
            flush()
        else:
            paragraph.append((number, selected))
    flush()
    return violations


def check_repository(root: Path) -> list[str]:
    tracked = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=root
    ).decode().split("\0")
    violations: list[str] = []
    for name in sorted(set(tracked)):
        path = Path(name)
        if path.suffix not in {".md", ".rs", ".py", ".sh", ".toml", ".yml", ".yaml"}:
            continue
        if not (root / path).is_file():
            continue
        violations.extend(check_text(path, (root / path).read_text(encoding="utf-8")))
    return violations


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    violations = check_repository(args.root)
    if violations:
        print("\n".join(violations))
        print(f"retired terms: {len(violations)} unmarked uses")
        return 1
    print(f"retired terms: {len(TERMS)} retirement rules passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
