#!/usr/bin/env python3
"""Reject retired mechanisms in current prose and backend/host identifiers."""

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
    RetiredTerm("retryable failed intent", r"\bretryable[ -]failed\s+intents?\b", "02ffbca4fd"),
)


# These are actual memory-only implementations, not the retired persistence
# backend. Upstream APIs and recording/reference models keep their real names.
LEGITIMATE_MEMORY_IDENTIFIERS = frozenset({
    "InMemoryLiveReplayStore", "InMemoryLiveReplayStoreConfig",
    "InMemoryDriveEpochs", "InMemoryRootLedger", "InMemoryRoots",
    "InMemoryLashlangArtifactStore", "InMemoryArtifactState",
    "InMemoryMetricExporter", "InMemorySpanExporter", "InMemory",
    "open_in_memory",
    "s3_attachment_store_satisfies_conformance_with_in_memory_object_store",
})
IDENTIFIER_RULES = (
    ("unqualified memory store", r"memory_store"),
    ("retired memory backend law", r"_in_memory"),
    ("retired memory backend type", r"InMemory"),
    ("retired commit host", r"JournaledCommit"),
    ("retired store journal host", r"store[_-]journal"),
)


# Queue-drain ownership is deleted (FIG-4489): every driver-run logical turn
# is owned by Turn(logical root), and a host command or a drive admission by a
# session operation. The names are refused wherever they are declared,
# constructed, matched, stored or documented: source, SQL, schemas and docs,
# comments and tests included. Nothing is exempt, and no marker excuses one.
QUEUE_DRAIN = re.compile(
    r"QueueDrain"
    r"|(?<![A-Za-z0-9_])(?:session_)?queue_drain(?:_scope|_encoding_range)?(?![A-Za-z0-9_])"
)
QUEUE_DRAIN_ROOTS = frozenset({"crates", "schemas", "docs", "examples", "runbooks", "scripts"})
QUEUE_DRAIN_SUFFIXES = frozenset({".rs", ".sql", ".json", ".md", ".py", ".sh", ".toml", ".ts"})
GATE_FILES = frozenset({
    Path("scripts/check_retired_terms.py"), Path("scripts/test_check_retired_terms.py"),
})


def check_queue_drain(path: Path, body: str) -> list[str]:
    if not path.parts or path.parts[0] not in QUEUE_DRAIN_ROOTS:
        return []
    if path.suffix not in QUEUE_DRAIN_SUFFIXES or path in GATE_FILES:
        return []
    return [
        f"{path}:{number}: retired queue-drain ownership: '{match[0]}' is deleted; "
        "a driver-run turn is owned by Turn(logical root), a host command by a session operation"
        for number, line in enumerate(body.splitlines(), 1)
        for match in QUEUE_DRAIN.finditer(line)
    ]


def check_identifiers(path: Path, body: str) -> list[str]:
    if not path.parts or path.parts[0] not in {"crates", "scripts", "runbooks", "examples"}:
        return []
    # The gate and its planted self-test fixtures must spell the denied names.
    if path in {Path("scripts/check_retired_terms.py"), Path("scripts/test_check_retired_terms.py")}:
        return []
    violations: list[str] = []
    for number, line in enumerate(body.splitlines(), 1):
        # Historical comments remain subject to the prose rules, not identifier
        # rules. Executable strings still include test filters and feature names.
        if re.match(r"\s*(?://|#(?!\[)|/\*|\*(?!/))", line):
            continue
        if not re.search(r"memory_store|_in_memory|InMemory|JournaledCommit|store[_-]journal", line):
            continue
        for match in re.finditer(r"[A-Za-z_][A-Za-z_0-9-]*", line):
            identifier = match[0]
            if identifier in LEGITIMATE_MEMORY_IDENTIFIERS:
                continue
            # This existing retirement gate denies removed types explicitly.
            if path == Path("scripts/check-substrate-boundary.sh") and identifier in {
                "InMemorySessionStore", "InMemorySessionStoreFactory",
            }:
                continue
            if path == Path("scripts/check-guarded-transactions.py") and identifier == "_in_memory":
                continue  # SQLite API suffix in the raw-connection denial regex.
            for term, pattern in IDENTIFIER_RULES:
                if term == "unqualified memory store" and re.search(r"(?:^|_)sqlite_memory_store", identifier):
                    continue
                if re.search(pattern, identifier):
                    violations.append(f"{path}:{number}: {term}: rename '{identifier}' to its current backend or host")
                    break
    return violations


def prose(line: str) -> str:
    # Link destinations and source paths can retain historical identifiers.
    line = re.sub(r"\]\([^)]*\)", "]", line)
    line = re.sub(r"`[^`\n]*[/][^`\n]*`", "", line)
    return line.replace("`", "").replace("**", "")


def check_text(path: Path, body: str) -> list[str]:
    markdown = path.suffix == ".md"
    adr = path.parts[:2] == ("docs", "adr")
    lines = body.splitlines()
    violations = check_identifiers(path, body) + check_queue_drain(path, body)
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
        if not (root / path).is_file():
            continue
        if path.suffix not in {".md", ".rs", ".py", ".sh", ".toml", ".yml", ".yaml"}:
            # Schemas, SQL and generated bindings carry no prose rules, only
            # the queue-drain denial.
            if path.suffix in QUEUE_DRAIN_SUFFIXES:
                violations.extend(
                    check_queue_drain(path, (root / path).read_text(encoding="utf-8"))
                )
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
    print(
        f"retired terms: {len(TERMS)} prose and {len(IDENTIFIER_RULES)} identifier rules "
        "and the queue-drain denial passed"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
