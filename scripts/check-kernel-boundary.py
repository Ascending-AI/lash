#!/usr/bin/env python3
"""The kernel crates depend on nothing else in lash (docs/kernel/design.md §9).

Three rules over the workspace members' manifests, each a dependency edge
given by `path`, in the member's manifest or in the `[workspace.dependencies]`
entry it inherits, and two over their Rust sources:

1. One direction. `lash-kernel-*`, `lash-dialect-*` and `lash-ext-*` crates
   depend only on each other and on third-party crates, so the set can move to
   its own repository. The crates that move with the set under their own names
   (the forked engine `lash-regress`, the standalone `kernel-embedder`) are in
   it and held to the same rule. Every dependency table counts: normal, dev,
   build and target-specific.
2. Crate roles (ADR 0139; FIG-3571). A language-neutral crate never reaches a
   dialect: the `lash-kernel-*` crates and lash's core and store crates may
   not link a `lash-dialect-*` crate, through their normal and build
   dependencies (optional ones included) and those of every crate they
   reach. Their tests may drive the facade with a dialect selected.
3. A dialect does not run the machine. A `lash-dialect-*` crate may not have
   a normal or build dependency on `lash-kernel-vm` or `lash-kernel-state`;
   its tests may run documents through a dev-dependency.
4. Kernel internals stay in the kernel (FIG-6688). The paged heap's and the
   machine's implementation are private to their crates; what the other
   kernel crates build on is exported only through a `kernel_internals`
   module, which visibility cannot close to them alone. No Rust source
   outside `crates/lash-kernel-*` names that module, and no kernel crate
   re-exports from it beyond the diagnostic seam that names its own engine's
   counters (`lash_kernel_vm::perf_witness`).

5. Host surfaces stay opaque (FIG-6711). Engine identities, checkpoint roots
   and raw observation records are named only in kernel crates. Hosts use
   opaque handles and semantic readers, including through facade re-exports.

Crates are named by their directory, which is their package name for the
kernel set; lash's own crates publish as `lash-internal-*`.
"""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

KERNEL_SET = re.compile(r"^lash-(kernel|dialect|ext)-")
# Crates that move with the set under their own names: `lash-regress` is the
# ECMAScript matcher under `lash-ext-regex-ecma`, and `kernel-embedder` is the
# minimal embedder outside lash (docs/kernel/design.md §9, rule 8).
MOVES_WITH_SET = frozenset({"lash-regress", "kernel-embedder"})
# Language-neutral: the kernel's own crates, lash's core crates and its stores.
LANGUAGE_NEUTRAL = re.compile(
    r"^lash-(kernel-[a-z-]+|core|core-[a-z-]+|sansio|durable|store-sql|sqlite-store"
    r"|postgres-store|s3-store)$"
)
DIALECT = re.compile(r"^lash-dialect-")
MACHINE = frozenset({"lash-kernel-vm", "lash-kernel-state"})
DEPENDENCY_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")
KERNEL_CRATE = re.compile(r"^lash-kernel-")
REEXPORT = re.compile(r"\bpub(?:\([^)]*\))?\s+use\b[^;]*\bkernel_internals\b", re.S)
# The machine's diagnostic seam names the heap's share of its work counts.
REEXPORT_HOMES = frozenset({"crates/lash-kernel-vm/src/perf_witness.rs"})
LINKED_TABLES = ("dependencies", "build-dependencies")

# Opaque handles own the host surface. Match tokens rather than only qualified
# paths so import aliases and facade re-exports cannot reopen the boundary.
ENGINE_RECORD = re.compile(r"\b(?:CheckpointRoot|KeyedRoot|SequenceRoot|EffectIdentity|TaskIdentity|TaskIdentityRef|SpawnIdentity|LoopIteration|TaskFilter|TaskPage|StatementPath|RanDelta)\b")
RUST_NONCODE = re.compile(r'//[^\n]*|/\*.*?\*/|r(?P<hash>\#*)".*?"(?P=hash)|"(?:\\.|[^"\\])*"', re.S)

def host_surface_stays_opaque(workspace: Workspace) -> list[str]:
    found = []
    for directory in sorted(workspace.names):
        if KERNEL_CRATE.match(directory.name):
            continue
        for path in rust_sources(directory):
            source = path.read_text(encoding="utf-8")
            code = RUST_NONCODE.sub(lambda match: "\n" * match[0].count("\n"), source)
            for match in [*ENGINE_RECORD.finditer(code), *kernel_paths(code, "Observe|conformance")]:
                line = code.count("\n", 0, match.start()) + 1
                found.append(
                    f"{path.relative_to(workspace.root)}:{line}: names engine record `{match[0]}`; "
                    "hosts use opaque identities, checkpoint control headers and semantic observations"
                )
    return found


def in_kernel_set(package: str, directory: str) -> bool:
    return bool(
        KERNEL_SET.match(package)
        or KERNEL_SET.match(directory)
        or package in MOVES_WITH_SET
        or directory in MOVES_WITH_SET
    )


def dependency_tables(manifest: dict, names: tuple[str, ...] = DEPENDENCY_TABLES) -> list[tuple[str, dict]]:
    tables = [(name, manifest.get(name, {})) for name in names]
    for target, body in manifest.get("target", {}).items():
        tables.extend((f"target.{target}.{name}", body.get(name, {})) for name in names)
    return tables


class Workspace:
    def __init__(self, root: Path) -> None:
        self.root = root.resolve()
        workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]
        self.shared = workspace.get("dependencies", {})
        self.names: dict[Path, str] = {}
        self.manifests: dict[Path, dict] = {}
        for member in workspace.get("members", []):
            directory = (root / member).resolve()
            manifest = tomllib.loads((directory / "Cargo.toml").read_text(encoding="utf-8"))
            self.names[directory] = manifest["package"]["name"]
            self.manifests[directory] = manifest

    def edges(self, directory: Path, names: tuple[str, ...] = DEPENDENCY_TABLES) -> list[tuple[str, str, Path]]:
        """(table, key, target directory) for every path dependency."""
        found = []
        for table, dependencies in dependency_tables(self.manifests[directory], names):
            for key, spec in sorted(dependencies.items()):
                spec = spec if isinstance(spec, dict) else {}
                base = self.root
                if spec.get("workspace"):
                    spec = self.shared.get(key, {})
                    spec = spec if isinstance(spec, dict) else {}
                else:
                    base = directory
                if "path" in spec:
                    found.append((table, key, (base / spec["path"]).resolve()))
        return found

    def relative(self, directory: Path) -> Path:
        return directory.relative_to(self.root)


def one_direction(workspace: Workspace) -> list[str]:
    found = []
    for directory, package in sorted(workspace.names.items()):
        if not in_kernel_set(package, directory.name):
            continue
        for table, key, target in workspace.edges(directory):
            name = workspace.names.get(target, key)
            if in_kernel_set(name, target.name):
                continue
            found.append(
                f"{workspace.relative(directory)}/Cargo.toml: [{table}] `{key}` is the lash crate "
                f"`{name}`; `{package}` may depend only on lash-kernel-*, lash-dialect-*, "
                "lash-ext-* and third-party crates"
            )
    return found


def dialect_reach(workspace: Workspace, directory: Path) -> list[tuple[str, list[str]]]:
    """(first table, path of crate directory names) to each dialect reached."""
    reached = []
    seen: set[Path] = set()
    pending = [
        (table, target, [directory.name, target.name])
        for table, _, target in workspace.edges(directory, LINKED_TABLES)
    ]
    while pending:
        table, target, path = pending.pop(0)
        if target in seen:
            continue
        seen.add(target)
        if DIALECT.match(target.name):
            reached.append((table, path))
            continue
        if target in workspace.manifests:
            pending.extend(
                (table, next_target, [*path, next_target.name])
                for _, _, next_target in workspace.edges(target, LINKED_TABLES)
            )
    return reached


def crate_roles(workspace: Workspace) -> list[str]:
    found = []
    for directory in sorted(workspace.names):
        if not LANGUAGE_NEUTRAL.match(directory.name):
            continue
        for table, path in dialect_reach(workspace, directory):
            found.append(
                f"{workspace.relative(directory)}/Cargo.toml: [{table}] reaches the dialect "
                f"`{path[-1]}` through {' -> '.join(path)}; a language-neutral crate never "
                "depends on a dialect"
            )
    return found


def dialects_off_the_machine(workspace: Workspace) -> list[str]:
    found = []
    for directory in sorted(workspace.names):
        if not DIALECT.match(directory.name):
            continue
        for table, key, target in workspace.edges(directory, LINKED_TABLES):
            if target.name in MACHINE:
                found.append(
                    f"{workspace.relative(directory)}/Cargo.toml: [{table}] `{key}` is the machine "
                    f"crate `{target.name}`; a dialect lowers to documents and may run them only "
                    "from its tests ([dev-dependencies])"
                )
    return found


def rust_sources(directory: Path):
    for path in sorted(directory.rglob("*.rs")):
        if "target" not in path.relative_to(directory).parts:
            yield path


def kernel_paths(text: str, members: str):
    """Resolve crate aliases before looking for engine-owned path members."""
    tail = rf"(?:{members})\b|\{{[^;]*?\b(?:{members})\b"
    direct = re.compile(rf"\b(?:lash_kernel_\w+|lash\s*::\s*dialect)\s*::\s*(?:{tail})")
    yield from direct.finditer(text)
    namespaces = set(re.findall(r"\blash_kernel_\w+\b", text)) | {"lash::dialect"}
    original = namespaces.copy()
    path = r"[A-Za-z_]\w*(?:\s*::\s*[A-Za-z_]\w*)*"
    edges = re.findall(rf"\b({path})\s+as\s+([A-Za-z_]\w*)", text)
    for group in re.finditer(rf"\buse\s+({path})\s*::\s*\{{([^;]+)\}}", text):
        prefix, body = group.groups()
        edges.extend((prefix, alias) for alias in re.findall(r"\bself\s+as\s+(\w+)", body))
        if prefix == "lash":
            edges.extend(("lash::dialect", alias) for alias in re.findall(r"\bdialect\s+as\s+(\w+)", body))
    while True:
        resolved = {alias for namespace, alias in edges
                    if re.sub(r"\s*::\s*", "::", namespace) in namespaces}
        if resolved <= namespaces:
            break
        namespaces.update(resolved)
    aliases = namespaces - original
    if aliases:
        prefix = "|".join(re.escape(alias) for alias in sorted(aliases))
        pattern = re.compile(rf"\b(?:{prefix})\s*::\s*(?:{tail})")
        yield from pattern.finditer(text)


def kernel_internals_stay_inside(workspace: Workspace) -> list[str]:
    found = []
    for directory in sorted(workspace.names):
        kernel = bool(KERNEL_CRATE.match(directory.name))
        for path in rust_sources(directory):
            relative = path.relative_to(workspace.root).as_posix()
            text = RUST_NONCODE.sub(lambda match: "\n" * match[0].count("\n"), path.read_text(encoding="utf-8"))
            if not kernel:
                for match in kernel_paths(text, "kernel_internals"):
                    line = text.count("\n", 0, match.start()) + 1
                    found.append(
                        f"{relative}:{line}: names a kernel crate's `kernel_internals`; code outside "
                        "crates/lash-kernel-* uses the kernel's embedding API and its semantic "
                        "readers, never its internals"
                    )
            elif relative not in REEXPORT_HOMES:
                for match in REEXPORT.finditer(text):
                    line = text.count("\n", 0, match.start()) + 1
                    found.append(
                        f"{relative}:{line}: re-exports from `kernel_internals`; a kernel crate "
                        "uses another's internals but never hands them on"
                    )
    return found


def violations(root: Path) -> list[str]:
    workspace = Workspace(root)
    return (
        one_direction(workspace)
        + crate_roles(workspace)
        + dialects_off_the_machine(workspace)
        + kernel_internals_stay_inside(workspace)
        + host_surface_stays_opaque(workspace)
    )


def main() -> int:
    root = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[1]
    found = violations(root)
    for violation in found:
        print(violation, file=sys.stderr)
    if found:
        print(
            f"check-kernel-boundary: {len(found)} violation(s); see docs/kernel/design.md §9",
            file=sys.stderr,
        )
        return 1
    print("check-kernel-boundary: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
