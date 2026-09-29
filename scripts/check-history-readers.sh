#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
from collections import Counter
from pathlib import Path
import re
import sys

root = Path.cwd()
allowlist = root / "scripts/history-reader-allowlist.txt"
count_file = root / "scripts/history-reader-allowlist.count"
read = re.compile(r"\.(load_session_window|load_ancestors|load_usage_ledger_page|load_failure_evidence_page)\s*\(")
deleted = re.compile(
    r"\b(?:load_session|load_session_at|load_node|message_tree|load_session_graph|"
    r"open_unbound_store|select_readable_to_generation|load_whole_graph_tx)\s*\("
)
valid_contexts = {"turn", "reopen", "refresh", "read-view"}


def test_code(path: str) -> bool:
    name = Path(path).name
    return (
        "/tests/" in path
        or "/testing/" in path
        or path.startswith("crates/lash-conformance/")
        or name == "tests.rs"
        or name.endswith("_tests.rs")
    )


def normalize(line: str) -> str:
    return " ".join(line.strip().split())


actual = Counter()
for base in ("crates", "examples", "runbooks"):
    for source in (root / base).rglob("*.rs"):
        path = source.relative_to(root).as_posix()
        for number, line in enumerate(source.read_text(encoding="utf-8").splitlines(), 1):
            stripped = line.lstrip()
            if stripped.startswith(("//", "*", "#")):
                continue
            if deleted.search(line):
                # All old APIs must leave the cutover, including tests and trait impls.
                actual[("DELETED", path, number, normalize(line))] += 1
            if not test_code(path):
                for match in read.finditer(line):
                    tag = "WINDOW" if match.group(1) == "load_session_window" else "PAGED"
                    actual[(tag, path, normalize(line))] += 1

allowed = Counter()
errors = []
for number, line in enumerate(allowlist.read_text(encoding="utf-8").splitlines(), 1):
    if not line.strip() or line.startswith("#"):
        continue
    try:
        body, annotation = line.split("  # ", 1)
        path, source, count = body.split("  |  ")
        tag, *reason = annotation.split()
        count = int(count)
    except ValueError:
        errors.append(f"allowlist:{number}: invalid entry")
        continue
    if tag not in {"WINDOW", "PAGED"} or count < 1:
        errors.append(f"allowlist:{number}: invalid tag or count")
    if tag == "WINDOW" and (not reason or reason[0] not in valid_contexts):
        errors.append(f"allowlist:{number}: WINDOW needs turn, reopen, refresh or read-view context")
    allowed[(tag, path, source)] += count

try:
    pinned = {}
    for line in count_file.read_text(encoding="utf-8").splitlines():
        if line.strip() and not line.startswith("#"):
            tag, value = line.split("=", 1)
            pinned[tag] = int(value)
    if set(pinned) != {"WINDOW", "PAGED"}:
        raise ValueError("expected WINDOW and PAGED counts")
except (ValueError, OSError) as exc:
    errors.append(f"count file: {exc}")
    pinned = {}

found = Counter()
for key, count in actual.items():
    tag = key[0]
    found[tag] += count
    if tag == "DELETED":
        errors.append(f"deleted {key[1]}:{key[2]}: {key[3]}")
    elif allowed[key] != count:
        errors.append(f"{tag} {key[1]}: {key[2]}: found {count}, allowlist {allowed[key]}")
for key, count in allowed.items():
    if actual[key] == 0:
        errors.append(f"stale {key[0]} {key[1]}: {key[2]} ({count})")
for tag in ("WINDOW", "PAGED"):
    if found[tag] != pinned.get(tag):
        errors.append(f"{tag} count: found {found[tag]}, pinned {pinned.get(tag)}")

print(f"history readers: WINDOW={found['WINDOW']} PAGED={found['PAGED']} deleted={found['DELETED']}")
if errors:
    for error in errors[:50]:
        print(error, file=sys.stderr)
    if len(errors) > 50:
        print(f"... {len(errors) - 50} more violations", file=sys.stderr)
    sys.exit(1)
PY
