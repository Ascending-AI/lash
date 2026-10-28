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
import subprocess

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


def postgres_artifacts(view: gate.TreeView, version: int) -> dict[str, str]:
    """The PostgreSQL provisioning and artifact stamps, maintained since FIG-7515."""
    files: dict[str, str] = {}
    path = "crates/lash-postgres-store/schema.sql"
    text = view.content(path)
    if text is not None:
        text, matches = re.subn(
            r"(VALUES\s*\('lash-postgres-store',\s*)\d+(,\s*)\d+(\))",
            lambda m: f"{m[1]}{version}{m[2]}{version}{m[3]}", text,
        )
        if matches != 1:
            raise gate.CheckError("cannot locate PostgreSQL provisioning stamp")
        files[path] = text
    for artifact, pattern, replacement in (
        ("schema.sql", r"(?m)^-- lash-postgres-store schema, component version \d+\.$",
         f"-- lash-postgres-store schema, component version {version}."),
        ("teardown.sql", r"(?m)^-- lash-postgres-store teardown, component version \d+\.$",
         f"-- lash-postgres-store teardown, component version {version}."),
        ("schema-shape.txt", r"(?m)^version \d+$", f"version {version}"),
    ):
        path = f"crates/lash-postgres-store/{artifact}"
        text = files[path] if path in files else view.content(path)
        if text is None:
            raise gate.CheckError(f"cannot read PostgreSQL artifact {path}")
        text, matches = re.subn(pattern, replacement, text)
        if matches != 1:
            raise gate.CheckError(f"cannot locate PostgreSQL artifact stamp in {path}")
        files[path] = text

    return files


def stamped_artifacts(repo: Path) -> dict[Path, str]:
    import release_baseline as baseline

    class Declarations(baseline.WorktreeSources):
        def matching_paths(self, patterns):
            # Artifact values need declared writers only. Allocation already
            # scans every source for unregistered versions; avoid repeating
            # that full Rust inspection for this cheap pre-push comparison.
            return tuple(path for path in super().matching_paths(patterns)
                         if "version_surface" in (self.content(path) or ""))

    tables = (baseline.BLAKE3_DOMAIN_TABLE, baseline.PREDECESSOR_TABLE)
    postgres_schema = repo / "crates/lash-postgres-store/schema.sql"
    has_tables = any((repo / path).exists() for path in tables)
    if not has_tables and not postgres_schema.exists():
        return {}
    try:
        rows = baseline.inventory(repo, view=Declarations(repo))
        files = baseline.generated_tables(rows, baseline.retired_hash_domains(repo)) if has_tables else {}
    except baseline.BaselineError as error:
        raise gate.CheckError(str(error)) from error
    postgres = next((row for row in rows if row["key"].endswith(":POSTGRES_SCHEMA_VERSION")), None)
    if postgres:
        files.update({Path(path): text for path, text in
                      postgres_artifacts(gate.WorktreeView(repo), postgres["default"]).items()})
    return files


def dirty_state(repo: Path) -> dict[str, bytes | None]:
    paths = gate.git(repo, "diff", "HEAD", "--name-only", "-z").stdout.split("\0")
    paths += gate.git(repo, "ls-files", "--others", "--exclude-standard", "-z").stdout.split("\0")
    return {path: (repo / path).read_bytes() if (repo / path).is_file() else None
            for path in paths if path}


def regenerate_artifacts(repo: Path) -> list[str]:
    """Run each existing authority after allocation, including in-place edits."""
    before = dirty_state(repo)
    for path, expected in stamped_artifacts(repo).items():
        if not (repo / path).exists() or (repo / path).read_text(encoding="utf-8") != expected:
            (repo / path).write_text(expected, encoding="utf-8")
    if any((repo / tree).exists() for tree in ("schemas/host", "crates/lash/host-schemas")):
        command = [sys.executable, "scripts/generate-workflow-schemas.py"]
        print("+ " + " ".join(command), flush=True)
        result = subprocess.run(command, cwd=repo)
        if result.returncode:
            raise gate.CheckError("authoritative host schema generation failed")
        after = dirty_state(repo)
        if any(path.startswith(("schemas/host/", "crates/lash/host-schemas/"))
               and (path not in before or path not in after or before[path] != after[path])
               for path in before.keys() | after.keys()):
            result = subprocess.run(["kiln", "sync"], cwd=repo)
            if result.returncode:
                raise gate.CheckError("schema artifact graph synchronization failed")
    after = dirty_state(repo)
    return sorted(path for path in before.keys() | after.keys()
                  if path not in before or path not in after or before[path] != after[path])


def check_artifacts(repo: Path) -> list[str]:
    """Check stamped files and tables without the post-cut migration policy.

    The authoritative JSON/TS comparison is //:schema_checks in land.
    """
    view = gate.WorktreeView(repo)
    expected = {str(path): text for path, text in stamped_artifacts(repo).items()}
    return [f"{path}: stale witness-keyed artifact" for path, text in expected.items()
            if view.content(path) != text]


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

    # The loop's last projection checked the complete planned tree. Derived
    # artifacts follow these witnesses after materialization below.
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
    return sorted(set(written + regenerate_artifacts(repo)))


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--base")
    mode.add_argument("--check-artifacts", action="store_true")
    parser.add_argument("--repo", type=Path, default=gate.ROOT)
    parser.add_argument("--strict", action="store_true", default=gate.STRICT_VERSION_BUMPS)
    args = parser.parse_args()
    try:
        if args.check_artifacts:
            errors = check_artifacts(args.repo)
            if errors:
                raise gate.CheckError("; ".join(errors))
            print("witness-keyed stamps and tables are current")
            return
        for path in apply(args.repo, args.base, strict=args.strict):
            print(path)
    except gate.CheckError as error:
        sys.exit(f"automatic format witnesses: {error}")


if __name__ == "__main__":
    main()
