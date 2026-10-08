#!/usr/bin/env python3
"""Enforce the canonical transcript projection boundary (FIG-1530).

FIG-972: the UI owns its input rows; correlation uses typed turn provenance.
FIG-984: every settled turn has one committed reply, and its owning writer
depends on the termination kind. Lash decodes typed history (FIG-5430); hosts
render it and do not select replies.

Two independent scrapes inventory committed-truth reads and even single
turn-output accessor calls. Each function's exact counts and disposition are
reviewed in transcript-projection-sites.toml; the checker derives their totals
from [sites], so the file stores none. Render assets must participate in
the same production-JavaScript row harness.
"""

from __future__ import annotations

import argparse
import ast
from collections import Counter
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
READS = {"messages", "chronological_projection", "message_tree"}
OUTPUTS = {"assistant_message", "final_value", "tool_value"}
CALL = re.compile(r"(?:\.|\b(?:TurnOutput|TurnOutputReport)::)\s*(messages|chronological_projection|message_tree|assistant_message|final_value|tool_value)\s*\(")
RAW_STRING = re.compile(r'(?:br|r)(#+)?"')
FUNCTION = re.compile(r"\bfn\s+([A-Za-z_]\w*)\s*(?:<|\()")
# The decoded vocabulary every rendering surface handles: what a committed
# node decodes to, and the roles a decoded message speaks in.
DECODED = {"items": {"Message", "Cell", "Suppressed"}, "roles": {"User", "Assistant", "Tool", "Event"}}
DECODED_ENUMS = {"items": "TranscriptItem", "roles": "TranscriptRole"}
DISPOSITIONS = {"projection-core", "evidence-read", "output-read"}


def code_only(source: str) -> str:
    """Mask comments and literals, retaining offsets for function ownership."""
    result = list(source)
    index = 0
    while index < len(source):
        start = index
        if source.startswith("//", index):
            end = source.find("\n", index)
            index = len(source) if end < 0 else end
        elif source.startswith("/*", index):
            depth = 1
            index += 2
            while index < len(source) and depth:
                if source.startswith("/*", index):
                    depth += 1
                    index += 2
                elif source.startswith("*/", index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
        elif source[index] in "br" and (match := RAW_STRING.match(source, index)):
            delimiter = '"' + (match[1] or "")
            end = source.find(delimiter, index + len(match[0]))
            index = len(source) if end < 0 else end + len(delimiter)
        elif source[index] == '"':
            index += 1
            while index < len(source):
                if source[index] == "\\":
                    index += 2
                elif source[index] == '"':
                    index += 1
                    break
                else:
                    index += 1
        else:
            index += 1
            continue
        result[start:index] = ["\n" if char == "\n" else " " for char in source[start:index]]
    return "".join(result)


def scrape(root: Path) -> dict[str, dict[str, int]]:
    sites: dict[str, Counter] = {}
    for scope in ("crates", "examples"):
        for path in sorted((root / scope).rglob("*.rs")):
            code = code_only(path.read_text())
            functions = list(FUNCTION.finditer(code))
            for call in CALL.finditer(code):
                owner = next((function[1] for function in reversed(functions) if function.start() < call.start()), "module")
                key = f"{path.relative_to(root).as_posix()}#{owner}"
                counts = sites.setdefault(key, Counter(reads=0, outputs=0))
                counts["reads" if call[1] in READS else "outputs"] += 1
    return {key: dict(counts) for key, counts in sites.items()}


def check(root: Path) -> list[str]:
    registry = tomllib.loads((root / "scripts/transcript-projection-sites.toml").read_text())
    found = scrape(root)
    registered = registry["sites"]
    errors = []
    for key in sorted(found.keys() | registered.keys()):
        site = registered.get(key)
        if site is None:
            errors.append(f"unregistered transcript read or output-selection site: {key}")
            continue
        if key not in found:
            errors.append(f"deleted transcript site remains registered: {key}")
            continue
        if {name: site[name] for name in ("reads", "outputs")} != found[key]:
            errors.append(f"transcript scrape counts changed at {key}: {found[key]}")
        if site["kind"] not in DISPOSITIONS or not site.get("reason", "").strip():
            errors.append(f"missing reviewed disposition or reason: {key}")
        if site["kind"] == "evidence-read" and found[key]["outputs"]:
            errors.append(f"evidence-read cannot select turn output: {key}")
    if "cardinality" in registry:
        errors.append("[cardinality] totals are derived from [sites], not stored: transcript-projection-sites.toml")
    for scrape_name in ("reads", "outputs"):
        cardinality = sum(counts[scrape_name] > 0 for counts in found.values())
        expected = sum(site[scrape_name] > 0 for site in registered.values())
        if cardinality != expected:
            errors.append(f"{scrape_name} cardinality changed: {cardinality}")
    kind_source = (root / "crates/lash-core-store/src/transcript/mod.rs").read_text()
    for vocabulary, enum in DECODED_ENUMS.items():
        body = re.search(rf"pub enum {enum}\s*\{{([^}}]+)\}}", kind_source)
        actual = set(re.findall(r"^\s*(\w+)\s*[,(]", body[1], re.MULTILINE)) if body else set()
        if actual != DECODED[vocabulary] or set(registry.get(vocabulary, ())) != DECODED[vocabulary]:
            errors.append(f"decoded {vocabulary} coverage changed: {sorted(actual)}")
    harness = root / "examples/agent-workbench/tests/transcript_projection_harness.mjs"
    if not harness.is_file():
        errors.append("shared production renderer harness is missing")
    harness_source = harness.read_text() if harness.is_file() else ""
    coverage = re.search(r"export const SURFACES = (\[[^;]+\]);", harness_source)
    harness_surfaces = set(ast.literal_eval(coverage[1])) if coverage else set()
    if harness_surfaces != {surface["name"] for surface in registry["surfaces"]}:
        errors.append("render registry and shared harness surfaces differ")
    assets = {surface["asset"] for surface in registry["surfaces"]}
    for path in sorted((root / "examples").rglob("*")):
        if path.is_file() and (path.suffix in {".html", ".js"} or path.name == "ui.rs"):
            if "transcriptRowId" in path.read_text() and path.relative_to(root).as_posix() not in assets:
                errors.append(f"unregistered rendered surface: {path.relative_to(root)}")
    for surface in registry["surfaces"]:
        if surface["harness"] != "examples/agent-workbench/tests/transcript_projection_harness.mjs":
            errors.append(f"surface lacks the shared row harness: {surface['name']}")
        if any(set(surface.get(vocabulary, ())) != kinds for vocabulary, kinds in DECODED.items()):
            errors.append(f"surface decoded coverage is incomplete: {surface['name']}")
        asset = root / surface["asset"]
        if not asset.is_file():
            errors.append(f"deleted rendered asset: {surface['asset']}")
            continue
        source = asset.read_text()
        begin, end = surface["begin"], surface["end"]
        if source.count(begin) != 1 or source.count(end) != 1:
            errors.append(f"production renderer marker missing or ambiguous: {surface['asset']}")
        for relative in surface["sources"]:
            path = root / relative
            if not path.is_file():
                errors.append(f"deleted render source: {relative}")
                continue
            code = code_only(path.read_text())
            if re.search(r"\b(?:MessageRole::Assistant|PartKind::Reasoning|decode_rlm_protocol_event|is_rlm_protocol_output|RLM_PROTOCOL_PLUGIN_ID)\b|\bpart\s*\.\s*content\s*\(", code):
                errors.append(f"host re-derives committed classification: {relative}")
            if re.search(r"\b(?:id|row_id|message_id)\s*(?:\(\))?\s*\.\s*(?:strip_prefix|starts_with|contains)\s*\(", code):
                errors.append(f"host parses transcript identity: {relative}")
        block = source.split(begin, 1)[-1].split(end, 1)[0]
        if re.search(r"\b(?:id|row_id|message_id)\s*\.\s*(?:startsWith|includes|substring|slice)\s*\(", block):
            errors.append(f"renderer parses transcript identity: {surface['asset']}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    errors = check(args.root)
    for error in errors:
        print(f"transcript-projection: {error}", file=sys.stderr)
    if not errors:
        print("transcript-projection: both scrapes, decoded coverage and render boundaries passed")
    return int(bool(errors))


if __name__ == "__main__":
    raise SystemExit(main())
