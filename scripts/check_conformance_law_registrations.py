#!/usr/bin/env python3
"""Check runtime persistence law registrations against the Rust law enum."""

from __future__ import annotations

from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
ENUM = Path("crates/lash-conformance/src/conformance/runtime_persistence/suite_and_receipts.rs")
MACROS = Path("crates/lash-conformance/src/macros.rs")
SITES = (
    ("in-memory", Path("crates/lash-conformance/src/in_memory/persistence.rs"), "runtime_persistence_tests"),
    ("SQLite", Path("crates/lash-sqlite-store/tests/conformance.rs"), "runtime_persistence_reopenable_tests"),
    ("PostgreSQL", Path("crates/lash-postgres-store/tests/conformance.rs"), "runtime_persistence_reopenable_tests"),
)
IDENT = r"[a-z_][a-z0-9_]*"
REGISTRATION = re.compile(
    rf"^\s*async fn (?P<test>{IDENT})\(\)\s*\{{\s*"
    rf"\$runner\(\$crate::RuntimePersistenceLaw::(?P<law>{IDENT})\)\.await;\s*\}}\s*$",
    re.MULTILINE,
)
INVOCATION = re.compile(
    rf"(?:{IDENT}::)*(?P<macro>runtime_persistence(?:_reopenable)?_tests)!\s*\("
)
DISABLED_CFG = re.compile(r"#\s*\[\s*cfg\s*\(\s*any\s*\(\s*\)\s*\)\s*\]")


def block(source: str, marker: str, label: str) -> str:
    start = source.find(marker)
    if start < 0:
        raise ValueError(f"missing {label} declaration")
    end = re.search(r"(?m)^}$", source[start:])
    if end is None:
        raise ValueError(f"unterminated {label} declaration")
    return source[start : start + end.end()]


def without_comments(source: str) -> str:
    source = re.sub(r"/\*.*?\*/", "", source, flags=re.DOTALL)
    return re.sub(r"//[^\n]*", "", source)


def enum_laws(source: str) -> tuple[str, ...]:
    region = block(source, "pub enum RuntimePersistenceLaw {", "RuntimePersistenceLaw")
    laws = []
    for line in region.splitlines()[1:]:
        item = line.strip()
        if not item or item == "}" or item.startswith(("//", "#")):
            continue
        match = re.fullmatch(rf"({IDENT}),", item)
        if match is None:
            raise ValueError(f"unrecognized RuntimePersistenceLaw entry: {item!r}")
        laws.append(match.group(1))
    if not laws:
        raise ValueError("RuntimePersistenceLaw inventory is empty")
    return tuple(laws)


def runner_special_laws(source: str, *, reopenable: bool) -> set[str]:
    marker = (
        "pub async fn runtime_persistence_reopenable<F>("
        if reopenable
        else "pub async fn runtime_persistence<F>("
    )
    end_marker = (
        "pub(super) fn assert_two_session_resolution_errors("
        if reopenable
        else "/// Run one independent durable reopen or runtime persistence vector."
    )
    start = source.find(marker)
    end = source.find(end_marker, start + len(marker))
    if start < 0 or end < 0:
        raise ValueError(f"missing {'reopenable' if reopenable else 'baseline'} runner")
    laws = set(re.findall(rf"RuntimePersistenceLaw::({IDENT})", without_comments(source[start:end])))
    if not laws:
        raise ValueError(f"{('reopenable' if reopenable else 'baseline')} runner has no explicit law vectors")
    return laws


def macro_laws(source: str, name: str) -> tuple[tuple[str, str], ...]:
    region = without_comments(block(source, f"macro_rules! {name} {{", name))
    pairs = tuple((m.group("test"), m.group("law")) for m in REGISTRATION.finditer(region))
    if not pairs:
        raise ValueError(f"{name} has no registrations")
    if sum("async fn " in line for line in region.splitlines()) != len(pairs):
        raise ValueError(f"unrecognized {name} registration form")
    return pairs


def disabled_invocation(source: str, match: re.Match[str]) -> bool:
    lines = source[:match.start()].rstrip().splitlines()
    return bool(lines) and DISABLED_CFG.fullmatch(lines[-1]) is not None


def check_repository(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    try:
        enum_source = (root / ENUM).read_text(encoding="utf-8")
        macro_source = (root / MACROS).read_text(encoding="utf-8")
        inventory = set(enum_laws(enum_source))
        baseline_special = runner_special_laws(enum_source, reopenable=False)
        reopenable_special = runner_special_laws(enum_source, reopenable=True)
        if baseline_special & reopenable_special:
            errors.append("baseline and reopenable runner-specific law identities overlap")
        for name, expected in (
            ("runtime_persistence_tests", inventory - reopenable_special),
            ("runtime_persistence_reopenable_tests", inventory - baseline_special),
        ):
            pairs = macro_laws(macro_source, name)
            laws = tuple(law for _, law in pairs)
            duplicates = sorted({law for law in laws if laws.count(law) > 1})
            if duplicates:
                errors.append(f"{name} has duplicate law identities: {', '.join(duplicates)}")
            unknown = sorted(set(laws) - inventory)
            if unknown:
                errors.append(f"{name} has unrecognized law identities: {', '.join(unknown)}")
            missing = sorted(expected - set(laws))
            if missing:
                errors.append(f"{name} is missing law identities: {', '.join(missing)}")
            unexpected = sorted(set(laws) - expected)
            if unexpected:
                errors.append(f"{name} has unexpected law identities: {', '.join(unexpected)}")
            mismatches = sorted(test for test, law in pairs if test != law)
            if mismatches:
                errors.append(f"{name} has test/law identity mismatches: {', '.join(mismatches)}")
    except (OSError, ValueError) as error:
        errors.append(str(error))

    discovered: list[tuple[Path, str]] = []
    for path in sorted(root.rglob("*.rs")):
        source = without_comments(path.read_text(encoding="utf-8"))
        discovered.extend((path.relative_to(root), match.group("macro")) for match in INVOCATION.finditer(source))
    known_paths = {path for _, path, _ in SITES}
    for label, path, expected_macro in SITES:
        if not (root / path).is_file():
            errors.append(f"{label} registration site is missing: {path}")
            continue
        invocations = [macro for candidate, macro in discovered if candidate == path]
        site_source = without_comments((root / path).read_text(encoding="utf-8"))
        site_invocations = list(INVOCATION.finditer(site_source))
        if any(disabled_invocation(site_source, match) for match in site_invocations):
            errors.append(f"{label} registration site disables its runtime persistence macro")
        if len(invocations) != 1:
            errors.append(f"{label} registration site must contain exactly one runtime persistence macro invocation")
        elif invocations[0] != expected_macro:
            errors.append(f"{label} registration site uses {invocations[0]}, expected {expected_macro}")
    for path, name in discovered:
        if path not in known_paths:
            errors.append(f"unexpected runtime persistence macro invocation {name} in {path}")
    return errors


if __name__ == "__main__":
    root = Path(sys.argv[1]).resolve() if len(sys.argv) == 2 else ROOT
    if len(sys.argv) > 2:
        raise SystemExit(f"usage: {Path(sys.argv[0]).name} [repository-root]")
    failures = check_repository(root)
    for failure in failures:
        print(f"conformance law registration check: {failure}", file=sys.stderr)
    if failures:
        raise SystemExit(1)
    print("runtime persistence law registrations match the inventory for all backends")
