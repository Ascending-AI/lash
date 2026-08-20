#!/usr/bin/env python3
"""Keep the documented durable-format versions equal to the ones the build writes.

Lash's durable formats fail closed (ADR 0055/0061/0064): there is no migration
decoder at any of these boundaries, so a version integer is not trivia.  It is
the number an operator compares before a deploy and the number a refusal names
afterwards.  A docs page that states a stale one is worse than a page that
states none, because it reads as an answer.

`crates/lash/src/formats.rs` is the manifest a host reads at runtime, and it
re-exports rather than copies, so drift between the runtime and the manifest is
not reachable.  Prose has no compiler, which is what this check is for.

The shape follows from one ruling and one observation.

**The ruling: ratified ADRs never become churn files.**  An ADR records a
decision at a moment; rewriting its numbers every time a format moves would
falsify the record and would make every format bump touch a dozen ratified
documents.  So an ADR does not restate current values -- it marks its numbers
historical, once, with the exact marker line in ``HISTORICAL_MARKER``, and the
check then leaves its claims alone.  The exemption is per-file and explicit:
an ADR on ``HISTORICAL_ADRS`` that has lost its marker fails, so the exemption
cannot be inherited by an edit that quietly drops the sentence, and an ADR that
is *not* listed may not state a format version at all.

**The observation: live pages must state the numbers, and prose rots
silently.**  A live page therefore writes a current claim as

    <span data-format-version="RLM_SNAPSHOT_VERSION">13</span>

which the check compares against the constant in the source tree.  A bare
number in prose is unverifiable by construction, so any bare number that reads
as a current claim about a known format is a failure telling the author to tag
it.  That is deliberately the strict direction: the cost of a false positive is
one tag, and the cost of a false negative is a docs page that lies for a year.

What the bare-claim scan does *not* flag is changelog narration -- "Version 40
adds the ``AssistantResponseHooks`` runtime-effect kind".  A sentence led by its
version number describes what one specific version did, and it stays true as the
build moves past it; a sentence led by the format's name asserts what this build
writes, and rots.  ``docs/remote-protocol.html`` is mostly the former by design,
so the patterns below require the format's name adjacent to the number.

Constant values are read out of the source tree by regex rather than by
building, so the check runs before the Rust toolchain exists.  A constant that
is missing, or that matches more than once, is a hard error: a renamed constant
must break this gate loudly rather than silently un-guard every page that cites
it.

``scripts/lint_docs.py`` keeps a narrower, older guard over five store schema
claims: it requires each to be *present* exactly once on its page. That is a
different property from the one here -- presence versus correctness of every
claim anywhere -- so both stay. It reads those pages through
``unwrap_format_version_spans`` so the tag this check requires does not hide the
number from it.

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
import html
from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
MANIFEST_PATH = "crates/lash/src/formats.rs"
DOCS_DIR = "docs"

# The one sentence a ratified ADR carries instead of current numbers. It is
# compared literally -- a paraphrase does not grant the exemption, because the
# point of the marker is that a reader who lands on the ADR's numbers is told,
# in a fixed form they will recognize from the other ADRs, that they are reading
# history and where the live table is.
HISTORICAL_MARKER = (
    "> **Historical versions.** The version numbers in this ADR record the "
    "state at ratification. The current values live in `lash::formats`; see "
    "`scripts/check_format_versions.py`."
)


@dataclass(frozen=True)
class Constant:
    """One version constant, where it lives, and how docs name its format."""

    name: str
    """The gate's key for this format.

    It is also the value a ``data-format-version`` attribute carries. For every
    manifest constant it is the Rust symbol verbatim. The two store constants
    named plain ``SCHEMA_VERSION`` in their own crates are keyed
    ``SQLITE_SCHEMA_VERSION`` and ``POSTGRES_SCHEMA_VERSION`` here, because one
    flat namespace cannot hold two ``SCHEMA_VERSION``s and a docs tag has to say
    which store it means.
    """

    path: str
    symbol: str
    manifest: bool
    phrases: tuple[str, ...]
    pattern: str | None = None

    def value_pattern(self) -> str:
        if self.pattern is not None:
            return self.pattern
        return (
            r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?const\s+"
            + re.escape(self.symbol)
            + r"\s*:[^=\n]+=\s*([0-9][0-9_]*)\s*;"
        )


# Every format version this repository documents anywhere.
#
# `manifest` says whether the constant is part of the runtime manifest in
# `crates/lash/src/formats.rs`. The three `manifest = False` groups are excluded
# there on purpose and that module explains why: the store schema versions are
# read from a deployment rather than from the build, and the trace schema and
# remote protocol versions gate a reader or a live peer rather than parked
# durable bytes. Listing them here anyway is the point -- they are documented,
# so they can rot, so they are checked.
#
# `phrases` are the names docs actually use for the format. They are matched
# case-insensitively and are the only vocabulary the bare-claim scan knows, so
# adding a format to the docs means adding it here.
CONSTANTS: tuple[Constant, ...] = (
    Constant(
        name="SESSION_CHECKPOINT_SCHEMA_VERSION",
        path="crates/lash-core/src/store/checkpoint.rs",
        symbol="SESSION_CHECKPOINT_SCHEMA_VERSION",
        manifest=True,
        phrases=("session checkpoint", "checkpoint manifest", "checkpoint record"),
    ),
    Constant(
        name="CHECKPOINT_COMPONENT_ENCODING_VERSION",
        path="crates/lash-core/src/store/checkpoint.rs",
        symbol="CHECKPOINT_COMPONENT_ENCODING_VERSION",
        manifest=True,
        phrases=("checkpoint-component encoding", "checkpoint component encoding"),
    ),
    Constant(
        name="SESSION_HEAD_META_SCHEMA_VERSION",
        path="crates/lash-core/src/store/mod.rs",
        symbol="SESSION_HEAD_META_SCHEMA_VERSION",
        manifest=True,
        phrases=("session head meta", "session-head meta"),
    ),
    Constant(
        name="PROCESS_WAKE_DELIVERY_FORMAT_VERSION",
        path="crates/lash-core/src/runtime/process/events.rs",
        symbol="PROCESS_WAKE_DELIVERY_FORMAT_VERSION",
        manifest=True,
        phrases=("wake delivery", "wake-delivery"),
    ),
    Constant(
        name="SESSION_NODE_BODY_SCHEMA_VERSION",
        path="crates/lash-core/src/session_graph.rs",
        symbol="SESSION_NODE_BODY_SCHEMA_VERSION",
        manifest=True,
        phrases=("session node body", "node body", "node-body generation"),
    ),
    Constant(
        name="BYTECODE_FORMAT_VERSION",
        path="crates/lashlang/src/lib.rs",
        symbol="BYTECODE_FORMAT_VERSION",
        manifest=True,
        phrases=("bytecode",),
    ),
    Constant(
        name="VM_CONTINUATION_FORMAT_VERSION",
        path="crates/lashlang/src/runtime/vm/continuation.rs",
        symbol="VM_CONTINUATION_FORMAT_VERSION",
        manifest=True,
        phrases=("vm continuation", "continuation"),
    ),
    Constant(
        name="LASHLANG_SNAPSHOT_VERSION",
        path="crates/lashlang/src/runtime/state.rs",
        symbol="LASHLANG_SNAPSHOT_VERSION",
        manifest=True,
        phrases=("lashlang snapshot", "snapshot"),
    ),
    Constant(
        name="HEAP_SIZE_SCHEDULE_VERSION",
        path="crates/lashlang/src/runtime/heap.rs",
        symbol="HEAP_SIZE_SCHEDULE_VERSION",
        manifest=True,
        phrases=("heap size schedule", "heap-size schedule", "size-schedule"),
    ),
    Constant(
        name="LASHLANG_SEGMENT_STATE_VERSION",
        path="crates/lash-lashlang-runtime/src/process.rs",
        symbol="LASHLANG_SEGMENT_STATE_VERSION",
        manifest=True,
        phrases=(
            "lashlang segment handover",
            "segment handover",
            "lashlang segment state",
            "segment state",
        ),
    ),
    Constant(
        name="RLM_SNAPSHOT_VERSION",
        path="crates/lash-protocol-rlm/src/executor/snapshot.rs",
        symbol="RLM_SNAPSHOT_VERSION",
        manifest=True,
        phrases=("rlm snapshot envelope", "rlm snapshot"),
    ),
    Constant(
        name="LASHLANG_VM_ABI_VERSION",
        path="crates/lashlang/src/artifact.rs",
        symbol="LASHLANG_VM_ABI_VERSION",
        manifest=True,
        phrases=("vm abi",),
        pattern=(
            r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?const\s+LASHLANG_VM_ABI_VERSION"
            r"\s*:[^=\n]+=\s*\"([^\"]+)\"\s*;"
        ),
    ),
    Constant(
        name="LASHLANG_SEMANTIC_HASH_VERSION",
        path="crates/lashlang/src/artifact.rs",
        symbol="LASHLANG_SEMANTIC_HASH_VERSION",
        manifest=True,
        phrases=("module artifact", "semantic hash"),
        pattern=(
            r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?const\s+"
            r"LASHLANG_SEMANTIC_HASH_VERSION\s*:[^=\n]+="
            r"\s*\"([^\"]+)\"\s*;"
        ),
    ),
    # Not in the manifest: a reader's gate, not parked durable bytes.
    Constant(
        name="TRACE_SCHEMA_VERSION",
        path="crates/lash-trace/src/lib.rs",
        symbol="TRACE_SCHEMA_VERSION",
        manifest=False,
        phrases=("trace schema", "trace record"),
    ),
    # Not in the manifest: a live peer's gate, not parked durable bytes.
    Constant(
        name="REMOTE_PROTOCOL_VERSION",
        path="crates/lash-remote-protocol/src/lib.rs",
        symbol="REMOTE_PROTOCOL_VERSION",
        manifest=False,
        phrases=("remote protocol",),
    ),
    # Not in the manifest: store schema stamps are read from the deployment.
    Constant(
        name="SQLITE_SCHEMA_VERSION",
        path="crates/lash-sqlite-store/src/schema.rs",
        symbol="SCHEMA_VERSION",
        manifest=False,
        phrases=("sqlite durable-core", "durable-core", "durable core"),
    ),
    Constant(
        name="SQLITE_PROCESS_SCHEMA_VERSION",
        path="crates/lash-sqlite-store/src/schema.rs",
        symbol="PROCESS_SCHEMA_VERSION",
        manifest=False,
        phrases=("process-registry", "process registry"),
    ),
    Constant(
        name="SQLITE_TRIGGER_SCHEMA_VERSION",
        path="crates/lash-sqlite-store/src/schema.rs",
        symbol="TRIGGER_SCHEMA_VERSION",
        manifest=False,
        phrases=("trigger",),
    ),
    Constant(
        name="SQLITE_EFFECT_SCHEMA_VERSION",
        path="crates/lash-sqlite-store/src/schema.rs",
        symbol="EFFECT_SCHEMA_VERSION",
        manifest=False,
        phrases=("effect",),
    ),
    Constant(
        name="POSTGRES_SCHEMA_VERSION",
        path="crates/lash-postgres-store/src/lib.rs",
        symbol="SCHEMA_VERSION",
        manifest=False,
        phrases=(
            "postgres component",
            "postgresql component",
            "postgres schema",
            "postgresql schema",
        ),
    ),
)


# ADRs whose numbers are history. Each must carry HISTORICAL_MARKER verbatim.
#
# An entry is a statement that the ADR's numbers were true at ratification and
# are not maintained. It is not a licence to add new numbers: a fresh claim
# about a current value belongs on a live page, tagged.
HISTORICAL_ADRS: tuple[str, ...] = (
    "docs/adr/0025-bounded-journals-are-an-effect-controller-obligation.md",
    "docs/adr/0042-tool-attempts-are-atomic.md",
    "docs/adr/0048-checkpoint-component-identity-is-a-backend-contract.md",
    "docs/adr/0049-session-ids-are-used-once.md",
    "docs/adr/0055-lashlang-execution-bounds-span-durable-process-lifetimes.md",
    "docs/adr/0056-checkpoint-components-generalize-to-a-keyed-set.md",
    "docs/adr/0060-the-lashlang-vm-is-a-heap-substrate-with-dialect-lowered-value-semantics.md",
    "docs/adr/0064-the-typescript-dialect-is-broad-and-every-gap-is-an-explicit-ruling.md",
)


# Narrowly scoped false positives: (path, substring of the matched claim).
#
# Every entry names the prose that forced it. These are exemptions for text that
# is not a version claim at all, never for a claim someone would rather not
# maintain -- a real claim gets tagged instead.
#
# Deliberately empty: the patterns above were tuned against the whole docs tree
# and produced no false positive on it, so nothing has earned a carve-out yet.
# The list exists so that the first one has to be written down with its prose.
CLAIM_EXEMPTIONS: tuple[tuple[str, str, str], ...] = ()


class CheckError(RuntimeError):
    """A source-shape, manifest, or configuration error."""


@dataclass
class Finding:
    path: str
    line: int
    claim: str
    detail: str

    def render(self) -> str:
        return f"{self.path}:{self.line}: {self.detail}\n    claim: {self.claim.strip()}"


@dataclass
class Report:
    findings: list[Finding] = field(default_factory=list)

    def add(self, path: str, line: int, claim: str, detail: str) -> None:
        self.findings.append(Finding(path, line, claim, detail))


# ---------------------------------------------------------------------------
# (a) live constant values


def read_constants(root: Path) -> dict[str, str]:
    values: dict[str, str] = {}
    for constant in CONSTANTS:
        source = root / constant.path
        try:
            text = source.read_text(encoding="utf-8")
        except OSError as error:
            raise CheckError(f"cannot read {constant.path}: {error}") from error
        matches = re.findall(constant.value_pattern(), text)
        if len(matches) != 1:
            raise CheckError(
                f"{constant.path}: expected exactly one definition of "
                f"{constant.symbol}, found {len(matches)}. A renamed or moved "
                f"constant must break this gate rather than silently un-guard "
                f"the docs that cite it; update CONSTANTS in "
                f"scripts/check_format_versions.py."
            )
        values[constant.name] = matches[0].replace("_", "")
    return values


# ---------------------------------------------------------------------------
# (b) manifest completeness

RE_EXPORT = re.compile(r"pub use\s+([^;]+);")
SCREAMING = re.compile(r"\b([A-Z][A-Z0-9_]{2,})\b")


def manifest_exports(root: Path) -> set[str]:
    source = root / MANIFEST_PATH
    try:
        text = source.read_text(encoding="utf-8")
    except OSError as error:
        raise CheckError(f"cannot read {MANIFEST_PATH}: {error}") from error
    exports: set[str] = set()
    for statement in RE_EXPORT.findall(text):
        exports.update(SCREAMING.findall(statement))
    return exports


def check_manifest(root: Path, report: Report) -> None:
    exports = manifest_exports(root)
    expected = {c.symbol for c in CONSTANTS if c.manifest}
    for missing in sorted(expected - exports):
        report.add(
            MANIFEST_PATH,
            0,
            missing,
            f"{missing} is marked as a manifest constant here but is not "
            f"re-exported by {MANIFEST_PATH}. Either re-export it or drop the "
            f"manifest mark.",
        )
    for extra in sorted(exports - expected):
        report.add(
            MANIFEST_PATH,
            0,
            extra,
            f"{extra} is re-exported by {MANIFEST_PATH} but is not in CONSTANTS "
            f"in scripts/check_format_versions.py, so no docs claim about it is "
            f"checked. Add it.",
        )


# ---------------------------------------------------------------------------
# (c) + (d) doc claims

TAGGED_CLAIM = re.compile(
    r"<span\s+data-format-version=\"([A-Za-z0-9_]+)\"\s*>(.*?)</span>",
    re.DOTALL,
)
HTML_TAG = re.compile(r"<[^>]+>")
# A version claim is "vN" or "N" but never a fragment of a longer token, and
# never the hyphenated `version-53` / `component-50` forms, which read as names
# of past shapes rather than as this build's value.
NUMBER = r"v?(\d+)"


def _phrase_alternation() -> str:
    phrases = sorted(
        (phrase for constant in CONSTANTS for phrase in constant.phrases),
        key=len,
        reverse=True,
    )
    return "|".join(re.escape(phrase) for phrase in phrases)


def _constant_alternation() -> str:
    names = {constant.symbol for constant in CONSTANTS}
    return "|".join(sorted(names, key=len, reverse=True))


_PHRASE = _phrase_alternation()
# Words that may sit between a format's name and its number.
_QUALIFIER = (
    r"(?:format|schema|component|envelope|encoding|record|state|handover|"
    r"snapshot|catalog|generation)\s+"
)
# A claim may put a copula between the format's name and its number: "the
# session node body is generation 8" asserts this build's value exactly as
# "session node body 8" does, and only the second shape was caught before.
_COPULA = r"(?:is\s+|sits\s+at\s+|at\s+)?"

# The four shapes a *current* claim takes. Each requires the format's own name
# adjacent to the number, which is what separates an assertion about this build
# ("the RLM snapshot envelope is 13") from changelog narration about one past
# version ("Version 40 adds ..."), the latter staying true forever and filling
# docs/remote-protocol.html by design.
CLAIM_PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    # bytecode format version 9 / effect version 11 / trace schema version 7
    (
        "name-then-version",
        re.compile(
            rf"\b(?:{_PHRASE})\s+(?:{_QUALIFIER})?versions?\s+"
            rf"(?:is\s+|of\s+|at\s+|from\s+|to\s+)?{NUMBER}\b",
            re.IGNORECASE,
        ),
    ),
    # version 9 of the bytecode format
    (
        "version-then-name",
        re.compile(
            rf"\bversions?\s+{NUMBER}\s+(?:of\s+)?(?:the\s+)?(?:{_PHRASE})\b",
            re.IGNORECASE,
        ),
    ),
    # bytecode format 9 / RLM snapshot envelope 13 / segment handover 3
    (
        "name-then-number",
        re.compile(
            rf"\b(?:{_PHRASE})\s+{_COPULA}(?:{_QUALIFIER})?{NUMBER}\b",
            re.IGNORECASE,
        ),
    ),
    # TRACE_SCHEMA_VERSION = 7 / REMOTE_PROTOCOL_VERSION == 41 / (currently 37)
    (
        "constant-assertion",
        re.compile(
            rf"\b(?:{_constant_alternation()})\b[^.<\n]{{0,40}}?"
            rf"(?:==|=|is|currently)\s+{NUMBER}\b",
        ),
    ),
    # the v1-to-v2 bytecode cutover -- a cutover narrative that names the
    # versions it spans is a claim about where the boundary sits now, and it
    # rots the moment a third version ships
    (
        "cutover-narrative",
        re.compile(rf"\bv\d+-to-v\d+\s+(?:{_PHRASE})\b", re.IGNORECASE),
    ),
    # the VM ABI identity, which is a build string rather than a counter
    ("vm-abi-identity", re.compile(r"lashlang-vm-abi-v\d+")),
)


def constants_by_name() -> dict[str, Constant]:
    return {constant.name: constant for constant in CONSTANTS}


def strip_markup(text: str) -> str:
    return html.unescape(HTML_TAG.sub(" ", text))


def is_exempt(path: str, claim: str) -> bool:
    return any(
        path == exempt_path and needle in claim
        for exempt_path, needle, _ in CLAIM_EXEMPTIONS
    )


def check_line(
    path: str,
    number: int,
    line: str,
    values: dict[str, str],
    report: Report,
    *,
    scan_bare: bool,
) -> None:
    known = constants_by_name()

    # (c) tagged claims must equal the live value.
    for match in TAGGED_CLAIM.finditer(line):
        name, raw = match.group(1), strip_markup(match.group(2)).strip()
        if name not in known:
            report.add(
                path,
                number,
                match.group(0),
                f"unknown format constant {name!r} in data-format-version; "
                f"known: {', '.join(sorted(known))}",
            )
            continue
        expected = values[name]
        if raw != expected:
            report.add(
                path,
                number,
                match.group(0),
                f"{name} is {expected} in {known[name].path}, but this page "
                f"states {raw!r}",
            )

    if not scan_bare:
        return

    # (d) any remaining bare claim is unverifiable and must be tagged. Tagged
    # spans are removed first, so a tagged claim never trips this scan.
    residual = strip_markup(TAGGED_CLAIM.sub(" ", line))
    for kind, pattern in CLAIM_PATTERNS:
        for match in pattern.finditer(residual):
            claim = match.group(0)
            if is_exempt(path, claim):
                continue
            report.add(
                path,
                number,
                claim,
                f"untagged format version claim ({kind}). A current value on a "
                f"live page must be written as "
                f"<span data-format-version=\"CONSTANT\">value</span> so this "
                f"check can compare it against the source tree; a ratified ADR "
                f"instead carries the historical-versions marker and is listed "
                f"in HISTORICAL_ADRS.",
            )


def doc_files(root: Path) -> list[Path]:
    docs = root / DOCS_DIR
    return sorted(
        path
        for pattern in ("**/*.html", "**/*.md")
        for path in docs.glob(pattern)
        if path.is_file()
    )


def check_docs(root: Path, values: dict[str, str], report: Report) -> None:
    historical = set(HISTORICAL_ADRS)
    for adr in sorted(historical):
        source = root / adr
        if not source.is_file():
            report.add(adr, 0, adr, "listed in HISTORICAL_ADRS but does not exist")
            continue
        if HISTORICAL_MARKER not in source.read_text(encoding="utf-8"):
            report.add(
                adr,
                0,
                adr,
                "listed in HISTORICAL_ADRS but does not carry the marker line. "
                "Restore it verbatim, or remove the ADR from the list and stop "
                "stating format versions in it:\n    " + HISTORICAL_MARKER,
            )

    for source in doc_files(root):
        relative = source.relative_to(root).as_posix()
        try:
            text = source.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            raise CheckError(f"cannot read {relative}: {error}") from error
        scan_bare = relative not in historical
        for number, line in enumerate(text.splitlines(), start=1):
            check_line(
                relative, number, line, values, report, scan_bare=scan_bare
            )


def run(root: Path) -> Report:
    report = Report()
    values = read_constants(root)
    check_manifest(root, report)
    check_docs(root, values, report)
    return report


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])
    try:
        report = run(args.repo)
    except CheckError as error:
        print(f"format-version check error: {error}", file=sys.stderr)
        return 2

    if report.findings:
        print(
            f"format-version check failed with {len(report.findings)} finding(s):",
            file=sys.stderr,
        )
        for finding in report.findings:
            print(f"- {finding.render()}", file=sys.stderr)
        return 1

    print(
        f"format-version check passed: {len(CONSTANTS)} constants, "
        f"{len(HISTORICAL_ADRS)} historical ADRs"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
