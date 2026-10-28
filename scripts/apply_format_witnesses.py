#!/usr/bin/env python3
"""Advance pre-1.0 format stamps on the rebased tree, before land checks it.

Integer stamps remain the reader/writer contract. Allocation happens only after
rebase, against that main's values, so parallel authored changes never compete
for the next integer. The 1.0 switch is check_version_bumps.STRICT_VERSION_BUMPS;
after the cut this command validates the strict policy and edits nothing.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import sys

import check_version_bumps as gate


class PlannedView(gate.TreeView):
    def __init__(self, repo: Path, files: dict[str, str]) -> None:
        super().__init__()
        self.inner = gate.WorktreeView(repo)
        self.files = files
        self.label = "automatic format witnesses"

    def matching_paths(self, patterns):
        return self.inner.matching_paths(patterns)

    def content(self, path):
        return self.files[path] if path in self.files else self.inner.content(path)


def advanced_value(value: str, delta: int) -> str:
    if re.fullmatch(r"[0-9][0-9_]*", value):
        return str(int(value.replace("_", "")) + delta)
    tagged = re.fullmatch(r'("[^"\\]*?v)([0-9]+)((?:[/:-][^"\\]*)?")', value)
    if tagged:
        return f"{tagged[1]}{int(tagged[2]) + delta}{tagged[3]}"
    typed = re.fullmatch(
        r"((?:[A-Za-z_][A-Za-z0-9_]*::)*FormatVersion)::"
        r"(?:ONE|new\(([0-9][0-9_]*)\)\.unwrap\(\))", gate.strip_rust_trivia(value),
    )
    if typed:
        at = int((typed[2] or "1").replace("_", ""))
        return f"{typed[1]}::new({at + delta}).unwrap()"
    raise gate.CheckError(f"cannot advance format witness expression {value!r}")


def advance(view: gate.TreeView, surface: gate.Surface, delta: int) -> tuple[str, str]:
    """Update the source of a local alias, and preserve synthetic successors."""
    text = view.content(surface.constant_path)
    if text is None:
        raise gate.CheckError(f"cannot read {surface.constant_path}")
    name = surface.constant
    for _ in range(4):
        value = gate._default_build_value(text, name, surface.key)
        if not re.fullmatch(r"[A-Z][A-Z0-9_]*", value):
            break
        name = value
    pattern = re.compile(
        rf"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?const[ \t]+{name}\b"
        r"\s*:\s*(?P<ty>[^=;]+)=\s*(?P<value>[^;]+);"
    )
    attributes = gate.rust_outer_attribute_ranges(text)
    tests = gate.test_only_module_ranges(text, attributes)
    edits = []
    for match in pattern.finditer(text):
        if any(start <= match.start() < end for start, end in tests):
            continue
        value = advanced_value(match["value"].strip(), delta)
        integer = re.fullmatch(r"([ui])(8|16|32|64|128)", match["ty"].strip())
        if integer and int(value) >= 2 ** (int(integer[2]) - (integer[1] == "i")):
            raise gate.CheckError(f"{surface.key} exhausted {match['ty'].strip()}")
        edits.append((match.start("value"), match.end("value"), value))
    if not edits:
        raise gate.CheckError(f"cannot locate witness definition {surface.key}")
    for start, end, value in reversed(edits):
        text = text[:start] + value + text[end:]
    return surface.constant_path, text


def apply(repo: Path, base: str, *, strict: bool | None = None) -> list[str]:
    strict = gate.STRICT_VERSION_BUMPS if strict is None else strict
    base_view = gate.RevisionView(repo, gate.resolve_revision(repo, base))
    originals = {surface.key: surface for surface in gate.discovered_surfaces(base_view, enforce=False)}
    files: dict[str, str] = {}
    changed: dict[str, tuple[gate.Surface, int | None, int]] = {}
    # A stamp can expose a change to another guarded encoding. Reproject until
    # no obligation remains, without incrementing any stamp twice per landing.
    for _ in range(len(originals) + 1):
        view = PlannedView(repo, files)
        result = gate.check_views(base_view, view, strict=strict)
        if result.errors:
            raise gate.CheckError("; ".join(f.detail for f in result.errors))
        if strict:
            if result.failures:
                raise gate.CheckError("; ".join(f.detail for f in result.failures))
            return []
        pending = result.failures
        if not pending:
            break
        keys = {s.key for s in result.evaluated}
        for finding in pending:
            surface = finding.surface
            previous = gate._base_surface(surface, originals, keys)
            if previous is None or surface.key not in keys:
                raise gate.CheckError(finding.detail)
            at, before = gate.version_at(view, surface), gate.version_at(base_view, previous)
            if at < before:
                raise gate.CheckError(finding.detail)
            if at == before:
                path, text = advance(view, surface, 1)
                files[path] = text
                view = PlannedView(repo, files)
                changed[surface.key] = (surface, before, before + 1)
    else:
        raise gate.CheckError("automatic format witnesses did not converge")

    # Include manually advanced stamps too, so hosts get a complete reset list.
    final = PlannedView(repo, files)
    keys = {s.key for s in result.evaluated}
    for surface in result.registered:
        if surface.upgrade != "coexist":
            changed[surface.key] = (surface, None, gate.version_at(final, surface))
    for surface in result.evaluated:
        previous = gate._base_surface(surface, originals, keys)
        if previous is not None:
            before, after = gate.version_at(base_view, previous), gate.version_at(final, surface)
            if before != after:
                changed[surface.key] = (surface, before, after)

    # Keep PostgreSQL's provisioning stamp identical to its descriptor. It is
    # embedded SQL, whereas SQLite provisions from its Rust descriptor.
    postgres = next((row for row in changed.values() if row[0].constant == "POSTGRES_SCHEMA_VERSION"), None)
    if postgres:
        path = "crates/lash-postgres-store/schema.sql"
        text = PlannedView(repo, files).content(path)
        if text is not None:
            text, matches = re.subn(
                r"(VALUES\s*\('lash-postgres-store',\s*)\d+(,\s*)\d+(\))",
                lambda m: f"{m[1]}{postgres[2]}{m[2]}{postgres[2]}{m[3]}", text,
            )
            if matches != 1:
                raise gate.CheckError("cannot locate PostgreSQL provisioning stamp")
            files[path] = text

    # The loop's last projection already checked the complete planned tree.
    # Only the SQL seed adjustment can have changed it since that check.
    final = PlannedView(repo, files)
    if postgres:
        result = gate.check_views(base_view, final, strict=False)
    if result.errors or result.failures:
        raise gate.CheckError("automatic format witnesses left unresolved obligations")
    if changed:
        subject = gate.git(repo, "log", "-1", "--format=%s").stdout.strip()
        ticket = re.search(r"FIG-\d+", subject)
        identity = ticket.group() if ticket else f"auto-formats-{base_view.revision[:12]}"
        path = f"docs/host-upgrades/unreleased/{identity}.md"
        target = repo / path
        note = target.read_text(encoding="utf-8") if target.exists() else f"# {identity}: Automatic format witnesses\n"
        note = re.sub(r"(?s)\n<!-- BEGIN automatic format witnesses -->.*?<!-- END automatic format witnesses -->\n", "", note)
        lines = ["", "<!-- BEGIN automatic format witnesses -->",
                 "This table is generated by `scripts/land` after rebase.", "",
                 f"Baseline: `{base_view.revision}`.", "",
                 "Before 1.0, changed formats require reset or recreation before deployment.",
                 "No migration, old-shape reader, feature or configuration opt-in is provided.", "",
                 "| Old contract | New contract / host action | Commit and ticket | Missed-change diagnostic or symptom |",
                 "| --- | --- | --- | --- |"]
        for key, (surface, before, after) in sorted(changed.items()):
            source = f"../../../{surface.constant_path}"
            old = "absent" if before is None else str(before)
            lines.append(f"| [`{surface.constant}`]({source}) = {old} | Witness {after}. Reset affected stores or recreate retained bytes. | This commit, {identity} | Version survey reports a changed witness; previous bytes are unsupported. |")
        lines += ["<!-- END automatic format witnesses -->"]
        files[path] = note.rstrip() + "\n" + "\n".join(lines) + "\n"
    written = []
    for path, text in sorted(files.items()):
        target = repo / path
        if target.exists() and target.read_text(encoding="utf-8") == text:
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")
        written.append(path)
    return written


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--base", required=True)
    parser.add_argument("--repo", type=Path, default=gate.ROOT)
    parser.add_argument("--strict", action="store_true", default=gate.STRICT_VERSION_BUMPS)
    args = parser.parse_args()
    try:
        for path in apply(args.repo, args.base, strict=args.strict):
            print(path)
    except gate.CheckError as error:
        sys.exit(f"automatic format witnesses: {error}")


if __name__ == "__main__":
    main()
