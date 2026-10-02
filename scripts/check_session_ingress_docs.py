#!/usr/bin/env python3
"""Keep the ingress glossary and command-lane ADRs on the landed contract.

FIG-4488 and FIG-4490: the Queued Work, Session Model and Parked Turn
glossary entries in CONTEXT.md, ADR 0030 and ADR 0101 §4 must describe the
config, batching and parking contracts the code already implements —
`ConfigTransaction`/`ConfigWrite` submitted through `SessionConfigAdmin` and
settling `Applied`/`Stale`/`Refused` (ADR 0126), durable per-root `RunSpec`
overrides (ADR 0101 §A5), merge key and authority as per-item data for the
host's `QueuedDrainPolicy` (ADR 0101 §5.2,
`crates/lash-core-store/src/queued_drain_policy.rs`), and implemented root
parks (`crates/lash/src/send.rs`,
`crates/lash-core-execution/src/runtime/park.rs`).

Each covered passage must carry none of the retired assertions the tickets
enumerate, must cite its owning ADRs and name the real APIs, and every code
path or line anchor it names must resolve.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re


ROOT = Path(__file__).resolve().parents[1]
CONTEXT = "CONTEXT.md"
ADR_0030 = "docs/adr/0030-the-session-profile-is-resolved-once-at-open.md"
ADR_0101 = "docs/adr/0101-one-session-ingress-carries-every-admitted-item.md"
WAKE_DOC = "crates/lash-core-store/src/queued_work_vocabulary.rs"


@dataclass(frozen=True)
class Passage:
    """One current-contract passage the gate covers.

    `start` and `end` are regexes; the passage runs from `start` to the next
    `end` (or to file end when `end` is None). A passage that cannot be found
    is itself a violation: the contract text must exist to be checked.
    """

    key: str
    file: str
    start: str
    end: str | None
    required: tuple[str, ...] = ()


def glossary_entry(name: str) -> tuple[str, str]:
    return rf"^- \*\*{name}\*\*:", r"\n(?:- \*\*|#)|\Z"


ADR_0126 = r"\bADR 0126\b"
ADR_0101_A5 = r"\bADR 0101\b[^.\n]{0,40}§?\s*A5\b"
OUTCOMES = (r"\bApplied\b", r"\bStale\b", r"\bRefused\b")
TRANSACTION_API = (r"\bConfigTransaction\b", r"\bConfigWrite\b", r"\bSessionConfigAdmin\b")

PASSAGES = (
    Passage(
        "queued-work",
        CONTEXT,
        *glossary_entry("Queued Work"),
        required=(r"\bADR 0101\b", r"\bQueuedDrainPolicy\b|\bDrain Policy\b"),
    ),
    Passage(
        "session-model",
        CONTEXT,
        *glossary_entry("Session Model"),
        required=(ADR_0126, ADR_0101_A5, *TRANSACTION_API, *OUTCOMES, r"\bRunSpec\b"),
    ),
    Passage(
        "parked-turn",
        CONTEXT,
        *glossary_entry("Parked Turn"),
        required=(r"\bADR 0101\b", r"\bimplemented\b", r"\bParked\b"),
    ),
    Passage(
        "adr-0030",
        ADR_0030,
        r"^## Decision",
        None,
        required=(ADR_0126, ADR_0101_A5, r"\bConfigTransaction\b", r"\bRunSpec\b"),
    ),
    Passage(
        "adr-0101-section-4",
        ADR_0101,
        r"^### 4\.",
        r"^### 5\.",
        required=(ADR_0126, *TRANSACTION_API, *OUTCOMES),
    ),
    Passage(
        "wake-merge-key-doc",
        WAKE_DOC,
        r"^/// Constant producer-selected merge key",
        r"^pub const PROCESS_WAKE_MERGE_KEY",
    ),
)


# Retired assertions the covered passages may no longer state. Each name is
# the ruling, not the denial: the listed pattern is what must not appear.
DENIED = (
    ("parked turns are implemented", r"\bnot\s+yet\s+implemented\b"),
    (
        "the patch API is retired",
        r"\bSessionConfigPatch\b|\bApplyConfigPatch\b|\bpatch_session_config\b"
        r"|\bresolve_session_config\b|\bSessionConfigAdmin\s*::\s*update\b",
    ),
    (
        "config changes are typed transactions, not patches",
        r"\bconfig[ -]patch\w*\b|\bpatch(?:es|ed|ing)?\b",
    ),
    ("each config transaction applies alone", r"\bcoalesc\w*"),
    (
        "merge key and authority are per-item policy data, not kernel gates",
        r"(?:merge[ -]keys?|authorit\w+|principals?|elevation|row\s+count|rendered\s+(?:context\s+)?(?:reserve|size))"
        r"[^.\n]{0,140}\bgates?\b"
        r"|\bgates?\b[^.\n]{0,140}"
        r"(?:merge[ -]keys?|authorit\w+|principals?|elevation|row\s+count|rendered\s+(?:context\s+)?(?:reserve|size))",
    ),
    (
        "durable per-root RunSpec overrides are admitted",
        r"\bno\s+turn[ -]level|\bturn[ -]level\s+(?:model\s+)?(?:overlay|override)s?\b"
        r"|\b(?:overrides?|overlays?)\s+(?:are|is|be)\s+(?:forbidden|prohibited|banned|not\s+(?:allowed|permitted|supported))",
    ),
    (
        "a refused route settles Refused; no queued turn fails",
        r"\bfails?\s+the\s+turn\s+queued\s+behind\b"
        r"|\bfail(?:s|ed|ure|ing)\b[^.\n]{0,50}\b(?:next|following|subsequent)\s+turn\b"
        r"|\b(?:next|following|subsequent)\s+turn\b[^.\n]{0,50}\bfail(?:s|ed|ure|ing)\b",
    ),
    (
        "route validation runs at recorded resolution, not at submission",
        r"\broute\b[^.\n]{0,60}\bvalidat\w*[^.\n]{0,60}"
        r"(?:when\s+(?:the\s+\w+\s+){0,3}(?:is\s+)?sent|at\s+submi\w+|send[ -]time|submit[ -]time)"
        r"|(?:when\s+(?:the\s+\w+\s+){0,3}(?:is\s+)?sent|at\s+submi\w+|send[ -]time|submit[ -]time)"
        r"[^.\n]{0,60}\broute\b[^.\n]{0,60}\bvalidat\w*",
    ),
)


# Code references inside covered passages must resolve: backticked paths under
# a known root directory (with an optional :line or ::symbol anchor) and
# relative Markdown link targets.
CODE_REF = re.compile(
    r"`((?:crates|docs|scripts|tools|examples|runbooks|schemas|deploy|fuzz)/[^\s`()]+)`"
)
MD_LINK = re.compile(r"\]\(\s*([^)\s#]+?)\s*\)")


def extract(path: Path, passage: Passage) -> tuple[str, int] | None:
    """The passage text and the number of file lines that precede it."""
    text = path.read_text(encoding="utf-8")
    start = re.search(passage.start, text, re.M)
    if start is None:
        return None
    base = text.count("\n", 0, start.start())
    if passage.end is None:
        return text[start.start() :], base
    end = re.search(passage.end, text[start.end() :], re.M)
    if end is None:
        return None
    return text[start.start() : start.end() + end.start()], base


def check_denied(
    path: Path, passage: Passage, text: str, violations: list[str], base: int = 0
) -> None:
    # Flattening newlines to spaces keeps offsets while letting a wrapped
    # sentence match, since the patterns never cross a period either way.
    flat = text.replace("\n", " ")
    for ruling, pattern in DENIED:
        for match in re.finditer(pattern, flat, re.IGNORECASE):
            line = base + text.count("\n", 0, match.start()) + 1
            violations.append(
                f"{path}:{passage.key}:{line}: retired assertion ({ruling}): {match.group().strip()!r}"
            )


def check_required(path: Path, passage: Passage, text: str, violations: list[str]) -> None:
    for pattern in passage.required:
        if not re.search(pattern, text):
            violations.append(f"{path}:{passage.key}: missing required {pattern!r}")


def check_links(
    root: Path, path: Path, passage: Passage, text: str, violations: list[str], base: int = 0
) -> None:
    def resolve(target: str, offset: int) -> None:
        line = base + text.count("\n", 0, offset) + 1
        target_path = target.split("::", 1)[0]
        file_part, _, anchor = target_path.rpartition(":")
        if not anchor.isdigit():
            file_part, anchor = target_path, ""
        if re.match(r"^(?:crates|docs|scripts|tools|examples|runbooks|schemas|deploy|fuzz)/", file_part):
            resolved = root / file_part
        else:
            resolved = path.parent / file_part
        if not resolved.is_file():
            violations.append(f"{path}:{passage.key}:{line}: unresolved code link {target!r}")
            return
        if anchor and int(anchor) > len(resolved.read_text(encoding="utf-8").splitlines()):
            violations.append(f"{path}:{passage.key}:{line}: {target!r} names a line past end of file")

    for match in CODE_REF.finditer(text):
        resolve(match.group(1), match.start())
    for match in MD_LINK.finditer(text):
        target = match.group(1)
        if "://" in target:
            continue
        resolve(target, match.start())


def check(root: Path) -> list[str]:
    violations: list[str] = []
    for passage in PASSAGES:
        path = root / passage.file
        if not path.is_file():
            violations.append(f"{passage.file}:{passage.key}: file missing")
            continue
        found = extract(path, passage)
        if found is None:
            violations.append(f"{passage.file}:{passage.key}: passage not found")
            continue
        text, base = found
        relative = Path(passage.file)
        check_denied(relative, passage, text, violations, base)
        check_required(relative, passage, text, violations)
        check_links(root.resolve(), root / passage.file, passage, text, violations, base)
    return violations


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    violations = check(args.root.resolve())
    if violations:
        print("\n".join(violations))
        print(f"session ingress docs: {len(violations)} retired assertions or contract gaps")
        return 1
    print(f"session ingress docs: {len(PASSAGES)} passages match the landed contract")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
