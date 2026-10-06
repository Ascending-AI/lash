#!/usr/bin/env python3
"""Hold the durable format-version registry exhaustive and the manifest true.

Source-declared constants form the inventory.
`discover_version_surfaces.py` rejects unregistered version strings, inline
family counters and constants, including those covered by a class exclusion.
Every ``*_VERSION`` / ``*_EPOCH`` constant defined in non-test Rust under
``crates/`` and ``examples/`` must be one of:

- a source ``version_surface`` declaration;
- a member of a ``[[excluded_class]]`` suffix, whose reason is written once
  for the whole class; or
- an ``[[unregistered]]`` entry naming the constant and why it versions no
  durable format.

An exclusion that no longer names a swept constant fails too, so the list
cannot rot into a blanket allowance.

The format manifest in ``crates/lash/src/formats.rs`` is checked against the
same registry, in both directions: every registered surface either names its
manifest row (``format_manifest = "<DurableFormat variant>"``, or
``format_manifest = "engine:<id>"`` for a row the build's effect engine registers
through ``lash::restate`` — ADR 0104 §2), states why it is outside the
manifest (``format_outside_manifest = "<reason>"``), or belongs to an excluded
class; and every manifest row is exactly one registered surface whose
``manifest`` names it. That is what makes the manifest's exhaustiveness claim
checkable rather than aspirational.

Every ``version_surface = "migrate"`` surface is held to FIG-3802's decoder laws: it
is a row of ``GUARDED_SURFACES`` in
``crates/lash-core-store/src/store/fleet_format.rs``, or it states why it is
not (``version_unguarded = "<reason>"``). Every row names a registered migrate
surface, and the crate it names as owner runs the three guarded-surface laws
under ``const OWNER`` set to its own name, so a row cannot be added without
its decoders being driven through its supported range.

DurableRecord and JournalStep implementations declare guarded roots in code.
A surface may retain ``/// version_guard(..)`` markers for DDL, encoders and
shared closures, or state why it has no shape
(``scripts/check_version_bumps.py`` describes the marker). A surface without
one, or with a guard the tree cannot evaluate, fails here, so the strict bump
gate's inventory cannot go incomplete silently.

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import sys
import tomllib

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_version_bumps  # noqa: E402


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CONFIG = Path(__file__).with_name("versioned-surfaces.toml")
MANIFEST = Path("crates/lash/src/formats.rs")
# An engine contributes its durable formats under its own crate
# (ADR 0104 §2), so the rows are parsed where the engine declares them; a
# surface claims such a row as ``format_manifest = "engine:<id>"``. No engine
# registers one until the durable engine does.
ENGINE_REGISTRIES: tuple[Path, ...] = ()
SWEPT_ROOTS = ("crates", "examples")

VERSION_CONSTANT = re.compile(
    r"^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?const[ \t]+"
    r"(?P<name>[A-Z][A-Z0-9_]*(?:_VERSION|_EPOCH))[ \t]*:",
    re.MULTILINE,
)
MANIFEST_ROW = re.compile(
    r"format:\s*DurableFormat::(?P<variant>\w+),\s*"
    r"version:\s*FormatVersion::\w+\((?P<symbol>\w+)(?:\s+as\s+u32)?\),\s*"
    r"owning_crate:\s*\"[^\"]+\",\s*"
    r"constant:\s*\"(?P<constant>\w+)\""
)
MANIFEST_ENTRY = re.compile(r"\bDurableFormatEntry\s*\{")
UPGRADE_ARM = re.compile(
    r"DurableFormat::(?P<variant>\w+)\s*=>\s*UpgradePolicy::(?P<policy>\w+)"
)
UPGRADE_POLICIES = ("migrate", "drain", "coexist")
RUST_POLICY_NAME = {
    "migrate": "Migrate",
    "drain": "Drain",
    "coexist": "Coexist",
}
ENGINE_ROW = re.compile(
    r"id:\s*\"(?P<id>[^\"]+)\",\s*"
    r"name:\s*\"[^\"]+\",\s*"
    r"version:\s*(?P<symbol>\w+)(?:\s+as\s+u32)?,\s*"
    r"constant:\s*\"(?P<constant>\w+)\",\s*"
    r"upgrade_policy:\s*UpgradePolicy::(?P<policy>\w+)"
)
ENGINE_ENTRY = re.compile(r"\bEngineDurableFormat\s*\{")
GUARDED_REGISTRY = Path("crates/lash-core-store/src/store/fleet_format.rs")
GUARDED_TABLE = re.compile(
    r"pub const GUARDED_SURFACES: &\[GuardedSurface\] = &\[(?P<body>.*?)\n\];",
    re.DOTALL,
)
GUARDED_ROW = re.compile(
    r"GuardedSurface\s*\{\s*constant:\s*\"(?P<constant>\w+)\",\s*"
    r"owner:\s*\"(?P<owner>[\w-]+)\","
)
GUARDED_LAWS = (
    "every_guarded_surface_decodes_its_supported_range",
    "unknown_version_is_refused_with_zero_mutation",
    "upcast_preserves_immutable_bytes_and_hashes",
)
TEST_ATTRIBUTE = re.compile(r"#\[cfg\(test\)\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{")


class RegistryError(Exception):
    """The registry or manifest cannot be read."""


@dataclass(frozen=True)
class Registry:
    surfaces: dict[str, dict]
    classes: dict[str, str]
    unregistered: dict[str, str]
    unregistered_upgrades: dict[str, str]


def is_test_path(relative: str) -> bool:
    parts = relative.split("/")
    name = parts[-1]
    return (
        "tests" in parts[:-1]
        or "benches" in parts[:-1]
        or name in {"tests.rs", "test.rs"}
        or name.endswith("_tests.rs")
    )


def test_module_ranges(text: str) -> list[tuple[int, int]]:
    """Bodies of inline ``#[cfg(test)] mod name { ... }`` modules."""
    ranges: list[tuple[int, int]] = []
    for match in TEST_ATTRIBUTE.finditer(text):
        depth = 1
        index = match.end()
        while index < len(text) and depth:
            if text[index] == "{":
                depth += 1
            elif text[index] == "}":
                depth -= 1
            index += 1
        ranges.append((match.end(), index))
    return ranges


def sweep(repo: Path) -> set[str]:
    """Every non-test ``path:CONSTANT`` key whose name is version-shaped."""
    keys: set[str] = set()
    for root in SWEPT_ROOTS:
        base = repo / root
        if not base.is_dir():
            continue
        for path in sorted(base.rglob("*.rs")):
            relative = path.relative_to(repo).as_posix()
            if "/target/" in f"/{relative}" or is_test_path(relative):
                continue
            text = path.read_text(encoding="utf-8", errors="surrogateescape")
            excluded = test_module_ranges(text)
            for match in VERSION_CONSTANT.finditer(text):
                if any(start <= match.start() < end for start, end in excluded):
                    continue
                keys.add(f"{relative}:{match.group('name')}")
    return keys


def _reason(raw: dict, field: str, location: str) -> str:
    value = raw.get(field)
    if not isinstance(value, str) or not value.strip():
        raise RegistryError(f"{location} needs a non-empty {field}")
    return value


def load_registry(path: Path) -> Registry:
    try:
        document = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise RegistryError(f"cannot read {path}: {error}") from error

    if "surface" in document:
        raise RegistryError("[[surface]] declarations are unsupported; declare version_surface in source")

    classes: dict[str, str] = {}
    for index, raw in enumerate(document.get("excluded_class", []), start=1):
        location = f"{path}: excluded_class {index}"
        suffix = raw.get("suffix")
        if not isinstance(suffix, str) or not suffix:
            raise RegistryError(f"{location} needs a suffix")
        classes[suffix] = _reason(raw, "reason", location)

    unregistered: dict[str, str] = {}
    unregistered_upgrades: dict[str, str] = {}
    for index, raw in enumerate(document.get("unregistered", []), start=1):
        location = f"{path}: unregistered {index}"
        constant = raw.get("constant")
        constant_path = raw.get("constant_path")
        if not isinstance(constant, str) or not isinstance(constant_path, str):
            raise RegistryError(f"{location} needs constant and constant_path")
        key = f"{constant_path}:{constant}"
        if key in unregistered:
            raise RegistryError(f"{location} duplicates {key}")
        upgrade = raw.get("upgrade")
        if upgrade is not None and upgrade not in UPGRADE_POLICIES:
            raise RegistryError(
                f"{location} ({key}) upgrade must be one of "
                + "|".join(UPGRADE_POLICIES)
            )
        if upgrade is not None:
            unregistered_upgrades[key] = upgrade
        unregistered[key] = _reason(raw, "reason", location)
    return Registry({}, classes, unregistered, unregistered_upgrades)


def class_of(registry: Registry, key: str) -> str | None:
    constant = key.rsplit(":", 1)[1]
    for suffix in registry.classes:
        if constant.endswith(suffix):
            return suffix
    return None


def upgrade_arms(text: str) -> dict[str, str]:
    """``DurableFormat`` variant -> ``migrate|drain|coexist``, parsed from the
    exhaustive ``DurableFormat::upgrade_policy()`` match in the manifest."""
    toml_of = {rust: toml for toml, rust in RUST_POLICY_NAME.items()}
    arms: dict[str, str] = {}
    for match in UPGRADE_ARM.finditer(text):
        variant = match.group("variant")
        policy = toml_of.get(match.group("policy"))
        if policy is None:
            raise RegistryError(
                f"{MANIFEST}: DurableFormat::{variant} has an upgrade_policy() "
                f"arm of {match.group('policy')}, which is not one of "
                + "|".join(UPGRADE_POLICIES)
            )
        if variant in arms:
            raise RegistryError(
                f"{MANIFEST}: DurableFormat::{variant} has two upgrade_policy() arms"
            )
        arms[variant] = policy
    if not arms:
        raise RegistryError(
            f"{MANIFEST}: no upgrade_policy() arms found; keep each arm in the "
            "form `DurableFormat::<Variant> => UpgradePolicy::<Policy>`"
        )
    return arms


def manifest_rows(text: str) -> dict[str, str]:
    """``constant -> DurableFormat variant`` for every manifest row."""
    rows: dict[str, str] = {}
    entries = len(MANIFEST_ENTRY.findall(text)) - 1  # minus the struct itself
    matches = list(MANIFEST_ROW.finditer(text))
    if len(matches) != entries:
        raise RegistryError(
            f"{MANIFEST}: parsed {len(matches)} rows but found {entries} "
            "DurableFormatEntry literals; keep each row's fields in the order "
            "format, version, owning_crate, constant"
        )
    for match in matches:
        constant = match.group("constant")
        if match.group("symbol") != constant:
            raise RegistryError(
                f"{MANIFEST}: row {match.group('variant')} reports "
                f"{match.group('symbol')} but names constant {constant}"
            )
        if constant in rows:
            raise RegistryError(f"{MANIFEST}: {constant} is listed twice")
        rows[constant] = match.group("variant")
    return rows


def engine_manifest_rows(repo: Path) -> tuple[dict[str, str], dict[str, str]]:
    """``(constant -> engine:<id>, engine:<id> -> policy)`` over every engine
    format registry.

    Engine-registered rows carry the fields the facade table needs, in the
    order ``id, name, version, constant, upgrade_policy``; the check reads
    them with the same strictness as the facade's own literals so a malformed
    row cannot silently un-claim a surface. An engine row's ``upgrade_policy``
    field stands in for the ``upgrade_policy()`` arm a facade variant would
    carry: the source's ``version_surface`` is held equal to it all the same.
    """
    toml_of = {rust: toml for toml, rust in RUST_POLICY_NAME.items()}
    rows: dict[str, str] = {}
    policies: dict[str, str] = {}
    for relative in ENGINE_REGISTRIES:
        path = repo / relative
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8", errors="surrogateescape")
        entries = len(ENGINE_ENTRY.findall(text)) - 1  # minus the struct itself
        matches = list(ENGINE_ROW.finditer(text))
        if len(matches) != entries:
            raise RegistryError(
                f"{relative}: parsed {len(matches)} rows but found {entries} "
                "EngineDurableFormat literals; keep each row's fields in the "
                "order id, name, version, constant, upgrade_policy"
            )
        for match in matches:
            constant = match.group("constant")
            if match.group("symbol") != constant:
                raise RegistryError(
                    f"{relative}: row {match.group('id')} reports "
                    f"{match.group('symbol')} but names constant {constant}"
                )
            if constant in rows:
                raise RegistryError(f"{relative}: {constant} is listed twice")
            policy = toml_of.get(match.group("policy"))
            if policy is None:
                raise RegistryError(
                    f"{relative}: row {match.group('id')} declares "
                    f"UpgradePolicy::{match.group('policy')}, which is not "
                    "one of " + "|".join(UPGRADE_POLICIES)
                )
            claim = f"engine:{match.group('id')}"
            rows[constant] = claim
            policies[claim] = policy
    return rows, policies


def guarded_rows(text: str) -> dict[str, str]:
    """``GUARDED_SURFACES`` as constant -> owner crate."""
    table = GUARDED_TABLE.search(text)
    if table is None:
        raise RegistryError(f"{GUARDED_REGISTRY} declares no GUARDED_SURFACES table")
    body = table.group("body")
    matches = list(GUARDED_ROW.finditer(body))
    entries = len(re.findall(r"\bGuardedSurface\s*\{", body))
    if len(matches) != entries:
        raise RegistryError(
            f"{GUARDED_REGISTRY}: {entries} GUARDED_SURFACES rows but {len(matches)} "
            "parse as `constant: \"...\", owner: \"...\"`"
        )
    rows: dict[str, str] = {}
    for match in matches:
        constant = match.group("constant")
        if constant in rows:
            raise RegistryError(f"{GUARDED_REGISTRY}: {constant} is guarded twice")
        rows[constant] = match.group("owner")
    return rows


def runs_guarded_laws(repo: Path, owner: str) -> bool:
    """Whether crate ``owner`` runs every guarded-surface law as itself."""
    sources = repo / "crates" / owner / "src"
    declaration = f'const OWNER: &str = "{owner}";'
    for path in sorted(sources.rglob("*.rs")) if sources.is_dir() else []:
        text = path.read_text(encoding="utf-8")
        if declaration in text and all(f"laws::{law}(OWNER" in text for law in GUARDED_LAWS):
            return True
    return False


def guarded_problems(repo: Path, registry: Registry) -> list[str]:
    problems: list[str] = []
    rows = guarded_rows((repo / GUARDED_REGISTRY).read_text(encoding="utf-8"))
    migrate: dict[str, list[str]] = {}
    for key, raw in sorted(registry.surfaces.items()):
        unguarded = raw.get("unguarded")
        if raw.get("upgrade") != "migrate":
            if unguarded is not None:
                problems.append(f"{key} states unguarded but is not a migrate surface")
            continue
        constant = raw.get("constant")
        migrate.setdefault(constant, []).append(key)
        if constant in rows and unguarded is not None:
            problems.append(f"{key} is a GUARDED_SURFACES row and also states unguarded")
        elif constant not in rows and unguarded is None:
            problems.append(
                f"{key} is a migrate surface outside GUARDED_SURFACES: add its row "
                f"to {GUARDED_REGISTRY} and run the guarded-surface laws over its "
                "decoders, or state version_unguarded = \"<reason>\""
            )
        elif unguarded is not None and (
            not isinstance(unguarded, str) or not unguarded.strip()
        ):
            problems.append(f"{key} unguarded must state a reason")
    for constant, owner in sorted(rows.items()):
        keys = migrate.get(constant, [])
        if len(keys) != 1:
            problems.append(
                f"GUARDED_SURFACES row {constant} must name exactly one registered "
                f"migrate surface; names {len(keys)}"
            )
        if not runs_guarded_laws(repo, owner):
            problems.append(
                f"GUARDED_SURFACES row {constant} is owned by {owner}, which does not "
                f"run the guarded-surface laws: crates/{owner}/src needs a test "
                f"module with const OWNER: &str = \"{owner}\" calling each of "
                + ", ".join(GUARDED_LAWS)
            )
    return problems


def guard_marker_problems(repo: Path, registry: Registry) -> list[str]:
    """Surfaces whose constant declares no guard, or one that cannot be read."""
    try:
        surfaces = [
            check_version_bumps.surface_of(raw, key)
            for key, raw in sorted(registry.surfaces.items())
        ]
    except check_version_bumps.CheckError as error:
        raise RegistryError(str(error)) from error
    from durable_surfaces import problems
    return (problems(check_version_bumps.WorktreeView(repo), {s.constant for s in surfaces})
            + check_version_bumps.worktree_problems(repo, surfaces))


def row_label(row: str) -> str:
    """How a table row prints in a finding: a facade variant or an engine id."""
    if row.startswith("engine:"):
        return f"engine format `{row[len('engine:'):]}`"
    return f"DurableFormat::{row}"


def check(repo: Path, registry: Registry, manifest_text: str) -> list[str]:
    problems: list[str] = []

    from discover_version_surfaces import discover
    try:
        discovered, discovery_problems = discover(
            check_version_bumps.WorktreeView(repo), unregistered=registry.unregistered, enforce=False,
        )
    except check_version_bumps.CheckError as error:
        raise RegistryError(str(error)) from error
    problems.extend(sorted(discovery_problems))
    for row in discovered:
        key = f'{row["constant_path"]}:{row["constant"]}'
        if class_of(registry, key) is not None:
            row.pop("outside_manifest", None)
    registry = Registry(
        {f'{r["constant_path"]}:{r["constant"]}': r for r in discovered},
        registry.classes, registry.unregistered, registry.unregistered_upgrades,
    )
    swept = sweep(repo)
    for key in sorted(swept):
        if key in registry.surfaces or key in registry.unregistered:
            continue
        if class_of(registry, key) is not None:
            continue
        if any(problem.startswith((key + " ", key + ":")) for problem in discovery_problems):
            continue
        problems.append(
            f"{key} is a version-shaped constant the registry does not know: "
            "declare version_surface and version_guard in source, or "
            "add an [[unregistered]] entry stating why it versions no durable format"
        )
    for key in sorted(registry.unregistered):
        if key not in swept:
            problems.append(
                f"[[unregistered]] {key} names no swept constant; delete the stale "
                "exclusion"
            )
        elif key in registry.surfaces:
            problems.append(f"{key} is both a registered surface and [[unregistered]]")

    rows = manifest_rows(manifest_text)
    engine_rows, engine_policies = engine_manifest_rows(repo)
    for constant, row in engine_rows.items():
        if constant in rows:
            problems.append(
                f"an engine format registry reports {constant}, which the "
                f"facade's own manifest already reports; a constant names one row"
            )
            continue
        rows[constant] = row
    claimed: dict[str, str] = {}
    for key, raw in sorted(registry.surfaces.items()):
        constant = raw.get("constant")
        manifest = raw.get("manifest")
        outside = raw.get("outside_manifest")
        in_class = class_of(registry, key) is not None
        declared = [
            label
            for label, present in (
                ("manifest", manifest is not None),
                ("outside_manifest", outside is not None),
                ("an excluded class", in_class),
            )
            if present
        ]
        if len(declared) != 1:
            detail = ", ".join(declared) if declared else "none"
            problems.append(
                f"{key} must have exactly one manifest disposition (manifest, "
                f"outside_manifest, or an excluded class); has {detail}"
            )
            continue
        if outside is not None and (not isinstance(outside, str) or not outside.strip()):
            problems.append(f"{key} outside_manifest must state a reason")
        if manifest is None:
            continue
        if not isinstance(manifest, str) or not manifest:
            problems.append(
                f"{key} manifest must name a DurableFormat variant or an "
                "engine:<id> row"
            )
            continue
        if constant in claimed:
            problems.append(
                f"{key}: {constant} is claimed by more than one surface "
                f"({claimed[constant]}); manifest rows are keyed by constant name"
            )
            continue
        claimed[constant] = key
        row = rows.get(constant)
        if row is None:
            problems.append(
                f"{key} declares manifest = {manifest!r} but the format table "
                f"has no row reporting {constant}"
            )
        elif row != manifest:
            problems.append(
                f"{key} declares manifest = {manifest!r} but the format table "
                f"reports {constant} as {row_label(row)}"
            )
    for constant, row in sorted(rows.items()):
        if constant not in claimed:
            problems.append(
                f"format-table row {row_label(row)} reports {constant}, which "
                f"no registered surface claims with manifest = {row!r}"
            )

    # The source's `version_surface` declares the policy;
    # the Rust `upgrade_policy()` arm for the same variant must answer the
    # same policy, and a variant with an arm but no manifest row is a hole in
    # the manifest's exhaustiveness claim.
    arms = upgrade_arms(manifest_text)
    row_variants = set(rows.values())
    for key, raw in sorted(registry.surfaces.items()):
        variant = raw.get("manifest")
        if not isinstance(variant, str) or not variant:
            continue
        if variant.startswith("engine:"):
            policy = engine_policies.get(variant)
            if policy is not None and policy != raw.get("upgrade"):
                problems.append(
                    f"{key} declares upgrade = {raw.get('upgrade')!r} but "
                    f"engine format `{variant[len('engine:'):]}` declares "
                    f"{policy!r}"
                )
            continue
        arm = arms.get(variant)
        if arm is None:
            problems.append(
                f"{key} declares manifest = {variant!r} but {MANIFEST} "
                "upgrade_policy() has no arm for it"
            )
        elif arm != raw.get("upgrade"):
            problems.append(
                f"{key} declares upgrade = {raw.get('upgrade')!r} but "
                f"DurableFormat::{variant}.upgrade_policy() answers {arm!r}"
            )
    for variant in sorted(arms):
        if variant not in row_variants:
            problems.append(
                f"DurableFormat::{variant} has an upgrade_policy() arm but no "
                f"manifest row; every durable format must be in {MANIFEST}"
            )
    problems.extend(guarded_problems(repo, registry))
    problems.extend(guard_marker_problems(repo, registry))
    return problems


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        registry = load_registry(args.config)
        manifest_text = (args.repo / MANIFEST).read_text(encoding="utf-8")
        problems = check(args.repo, registry, manifest_text)
    except (OSError, RegistryError) as error:
        print(f"format-registry check error: {error}", file=sys.stderr)
        return 2
    if problems:
        print("format-registry check failed:", file=sys.stderr)
        for problem in problems:
            print(f"- {problem}", file=sys.stderr)
        return 1
    from discover_version_surfaces import discover
    discovered, _ = discover(check_version_bumps.WorktreeView(args.repo))
    print(
        f"format-registry check passed: {len(discovered)} surfaces, "
        f"{len(registry.unregistered)} stated exclusions, "
        f"{len(registry.classes)} excluded classes"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
