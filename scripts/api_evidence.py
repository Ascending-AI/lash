#!/usr/bin/env python3
"""Prototype compiler-derived example coverage for the Lash facade.

The prototype deliberately reports only what rustdoc's scrape-examples pass
can prove: a typechecked, direct function or method call.  It never turns
fields, variants, type construction, or concrete trait-implementation
selection into "uncovered" results because rustdoc does not collect those
uses.

Cargo only scrapes targets carrying the unstable `doc-scrape-examples` target
setting.  Lash's examples are ordinary workspace packages, so this script
creates a temporary source snapshot and adds that setting mechanically.  No
tracked manifest or example source is changed, and there is no item-to-example
mapping.
"""

from __future__ import annotations

import argparse
from collections import defaultdict
import html
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
from typing import Any, NamedTuple

import api_surface  # noqa: E402  (sibling script)


REPO = Path(__file__).resolve().parents[1]
DEFAULT_EXAMPLE_PACKAGES = (
    "agent-service",
    "agent-workbench",
    "slack-clone",
)
SCRAPED_MARKER = 'class="docblock scraped-example-list"'
SCRAPED_TITLE = re.compile(r'<div class="scraped-example-title">(.*?) \(<a ', re.S)
METHOD_SECTION = re.compile(r'<section id="(?:ty)?method\.([^"-]+)" class="([^"]+)"')
PAGE = re.compile(r"^(fn|struct|enum|trait|union)\.(.+)\.html$")


class SurfaceItem(NamedTuple):
    symbol: str
    kind: str
    identity: str
    aliases: tuple[str, ...]


class CallEvidence(NamedTuple):
    identity: str
    call_kind: str
    example_paths: tuple[str, ...]


def run(
    command: list[str],
    *,
    cwd: Path = REPO,
    env: dict[str, str] | None = None,
    capture: bool = False,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        check=True,
        text=True,
        stdout=subprocess.PIPE if capture else None,
    )


def cargo_metadata() -> dict[str, Any]:
    result = run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        capture=True,
    )
    return json.loads(result.stdout)


def canonical_surface() -> list[SurfaceItem]:
    # Import the existing oracle rather than reimplementing its re-export and
    # member-identity rules.  The prototype needs one representative feature
    # configuration, so it deliberately avoids the gate's second all-features
    # build and availability merge.
    raw_surface = api_surface.lash_surface(
        api_surface.rustdoc("lash-runtime", "lash", False), False
    )
    items: list[SurfaceItem] = []
    for (identity, kind), paths in api_surface.api_items(raw_surface).items():
        primary = api_surface.primary_path(paths)
        if not primary.startswith("lash::"):
            continue
        items.append(
            SurfaceItem(
                primary,
                kind,
                identity,
                tuple(path for path in paths if path != primary),
            )
        )
    return sorted(items, key=facade_path)


def package_maps(
    metadata: dict[str, Any],
) -> tuple[dict[str, dict[str, Any]], dict[str, str]]:
    packages = {package["name"]: package for package in metadata["packages"]}
    crate_to_package: dict[str, str] = {}
    for package in metadata["packages"]:
        for target in package["targets"]:
            if "lib" in target["kind"]:
                crate_to_package[target["name"]] = package["name"]
    return packages, crate_to_package


def archive_head(destination: Path) -> None:
    archive = subprocess.Popen(
        ["git", "archive", "--format=tar", "HEAD"],
        cwd=REPO,
        stdout=subprocess.PIPE,
    )
    assert archive.stdout is not None
    extract = subprocess.run(
        ["tar", "-xf", "-", "-C", str(destination)],
        stdin=archive.stdout,
        check=False,
    )
    archive.stdout.close()
    archive_status = archive.wait()
    if archive_status != 0 or extract.returncode != 0:
        raise RuntimeError(
            f"could not create temporary HEAD snapshot "
            f"(git={archive_status}, tar={extract.returncode})"
        )


def add_lib_opt_in(manifest: Path) -> None:
    source = manifest.read_text()
    lib_header = re.search(r"(?m)^\[lib\]\s*$", source)
    if lib_header is None:
        source += "\n[lib]\ndoc-scrape-examples = true\n"
    else:
        insertion = lib_header.end()
        source = source[:insertion] + "\ndoc-scrape-examples = true" + source[insertion:]
    manifest.write_text(source)


def add_bin_opt_in(
    manifest: Path, target: dict[str, Any], original_manifest: Path
) -> None:
    source = manifest.read_text()
    target_name = target["name"]
    table = re.compile(
        r"(?ms)^\[\[bin\]\]\s*\n(?P<body>.*?)(?=^\[|\Z)"
    )
    for match in table.finditer(source):
        name = re.search(r'(?m)^name\s*=\s*"([^"]+)"', match.group("body"))
        if name is None or name.group(1) != target_name:
            continue
        insertion = match.start("body")
        source = source[:insertion] + "doc-scrape-examples = true\n" + source[insertion:]
        manifest.write_text(source)
        return

    relative_source = Path(
        os.path.relpath(target["src_path"], original_manifest.parent)
    )
    source += (
        f'\n[[bin]]\nname = "{target_name}"\n'
        f'path = "{relative_source.as_posix()}"\n'
        "doc-scrape-examples = true\n"
    )
    manifest.write_text(source)


def opt_in_example_package(snapshot: Path, package: dict[str, Any]) -> str:
    original_manifest = Path(package["manifest_path"])
    relative_manifest = original_manifest.relative_to(REPO)
    manifest = snapshot / relative_manifest
    targets = package["targets"]
    library = next((target for target in targets if "lib" in target["kind"]), None)
    if library is not None:
        add_lib_opt_in(manifest)
        return library["name"]

    preferred = next(
        (
            target
            for target in targets
            if "bin" in target["kind"] and target["name"] == package["name"]
        ),
        None,
    )
    if preferred is None:
        preferred = next((target for target in targets if "bin" in target["kind"]), None)
    if preferred is None:
        raise RuntimeError(f"{package['name']} has no lib or bin target to scrape")
    add_bin_opt_in(manifest, preferred, original_manifest)
    return preferred["name"]


def facade_package(crate_to_package: dict[str, str]) -> str:
    try:
        return crate_to_package["lash"]
    except KeyError as error:
        raise RuntimeError("cargo metadata has no library target named lash") from error


def docs_target(environment: dict[str, str]) -> Path:
    configured = Path(environment.get("CARGO_TARGET_DIR", "target"))
    if not configured.is_absolute():
        configured = REPO / configured
    return configured / "fig2090-api-evidence"


def rustdoc_scrape(
    snapshot: Path,
    target: Path,
    facade: str,
    examples: list[str],
    environment: dict[str, str],
) -> float:
    command = [
        "cargo",
        "+nightly",
        "doc",
        "-Zunstable-options",
        "-Zrustdoc-scrape-examples",
        "--no-deps",
    ]
    for package in sorted({facade, *examples}):
        command += ["-p", package]

    scrape_environment = environment.copy()
    scrape_environment["CARGO_TARGET_DIR"] = str(target)
    started = time.monotonic()
    run(command, cwd=snapshot, env=scrape_environment)
    return time.monotonic() - started


def clean_title(raw: str, snapshot: Path) -> str:
    title = html.unescape(re.sub(r"<[^>]+>", "", raw)).strip()
    candidate = Path(title)
    if candidate.is_absolute():
        try:
            return candidate.relative_to(snapshot).as_posix()
        except ValueError:
            return candidate.as_posix()
    return title.removeprefix("./")


def page_identity(crate: str, relative: Path, item_name: str) -> str:
    modules = list(relative.parent.parts)
    return "::".join([crate, *modules, item_name])


def calls_from_page(
    crate: str, crate_docs: Path, page: Path, snapshot: Path
) -> list[CallEvidence]:
    relative = page.relative_to(crate_docs)
    page_match = PAGE.match(page.name)
    if page_match is None:
        return []
    page_kind, item_name = page_match.groups()
    document = page.read_text()
    positions = [match.start() for match in re.finditer(SCRAPED_MARKER, document)]
    evidence: list[CallEvidence] = []
    base_identity = page_identity(crate, relative, item_name)

    for index, position in enumerate(positions):
        end = positions[index + 1] if index + 1 < len(positions) else len(document)
        block = document[position:end]
        paths = tuple(
            sorted(
                {
                    clean_title(title, snapshot)
                    for title in SCRAPED_TITLE.findall(block)
                }
            )
        )
        if not paths:
            continue
        if page_kind == "fn":
            identity = base_identity
            call_kind = "plain-function"
        else:
            previous = list(METHOD_SECTION.finditer(document, 0, position))
            if not previous:
                continue
            method_name = previous[-1].group(1)
            identity = f"{base_identity}::{method_name}"
            if page_kind == "trait":
                call_kind = "trait-method"
            elif method_name == "new" or method_name.startswith("from_"):
                call_kind = "constructor"
            else:
                call_kind = "inherent-method"
        evidence.append(CallEvidence(identity, call_kind, paths))
    return evidence


def parse_calls(
    target: Path, crate_roots: set[str], snapshot: Path
) -> dict[str, CallEvidence]:
    merged: dict[str, tuple[str, set[str]]] = {}
    docs = target / "doc"
    for crate in sorted(crate_roots):
        crate_docs = docs / crate
        if not crate_docs.is_dir():
            continue
        for page in crate_docs.rglob("*.html"):
            for evidence in calls_from_page(crate, crate_docs, page, snapshot):
                kind, paths = merged.setdefault(
                    evidence.identity, (evidence.call_kind, set())
                )
                paths.update(evidence.example_paths)
    return {
        identity: CallEvidence(identity, kind, tuple(sorted(paths)))
        for identity, (kind, paths) in merged.items()
    }


def facade_path(item: SurfaceItem) -> str:
    return item.symbol


def selected_example_path(path: str, package: dict[str, Any]) -> bool:
    manifest = Path(package["manifest_path"])
    package_dir = manifest.parent.relative_to(REPO).as_posix()
    return path == package_dir or path.startswith(f"{package_dir}/")


def print_report(
    surface: list[SurfaceItem],
    calls: dict[str, CallEvidence],
    example_packages: list[dict[str, Any]],
    elapsed: float,
    sample_limit: int,
) -> int:
    surface_functions = [item for item in surface if item.kind == "function"]
    functions_by_path = {
        path: item
        for item in surface_functions
        for path in (item.symbol, *item.aliases)
    }
    covered_by_identity: dict[str, tuple[SurfaceItem, str, set[str]]] = {}
    for observed_path, evidence in calls.items():
        item = functions_by_path.get(observed_path)
        if item is None:
            continue
        matching_paths = tuple(
            path
            for path in evidence.example_paths
            if any(selected_example_path(path, package) for package in example_packages)
        )
        if matching_paths:
            existing = covered_by_identity.setdefault(
                item.identity, (item, evidence.call_kind, set())
            )
            existing[2].update(matching_paths)
    covered = [
        (item, CallEvidence(item.identity, call_kind, tuple(sorted(paths))))
        for item, call_kind, paths in covered_by_identity.values()
    ]
    covered.sort(key=lambda pair: pair[0].symbol)

    contributors: dict[str, int] = {}
    for package in example_packages:
        contributors[package["name"]] = sum(
            1
            for _, evidence in covered
            if any(selected_example_path(path, package) for path in evidence.example_paths)
        )

    covered_identities = {item.identity for item, _ in covered}
    uncovered = sorted(
        (item for item in surface_functions if item.identity not in covered_identities),
        key=facade_path,
    )
    non_derivable = defaultdict(list)
    for item in surface:
        if item.kind != "function":
            non_derivable[item.kind].append(item)

    print("FIG-2090 compiler-derived facade call evidence")
    print(f"scrape wall time: {elapsed:.2f}s")
    print(f"default-feature canonical facade inventory: {len(surface)} item(s)")
    print("example contributors:")
    for package in example_packages:
        name = package["name"]
        print(f"  {name}: {contributors[name]} facade call item(s)")
    print()
    print(f"COVERED direct calls ({len(covered)} total; sampled by call kind):")
    covered_groups: dict[str, list[tuple[SurfaceItem, CallEvidence]]] = defaultdict(list)
    for pair in covered:
        covered_groups[pair[1].call_kind].append(pair)
    for call_kind in (
        "plain-function",
        "constructor",
        "inherent-method",
        "trait-method",
    ):
        items = covered_groups[call_kind]
        print(f"  {call_kind}: {len(items)} item(s)")
        for item, evidence in items[:sample_limit]:
            alias = " re-export" if not item.identity.startswith("lash::") else ""
            joined = ", ".join(evidence.example_paths)
            print(
                f"    [{call_kind}{alias}] {item.symbol} "
                f"(identity {item.identity}) <- {joined}"
            )

    print()
    print(
        f"UNCOVERED direct-call candidates ({len(uncovered)}; "
        f"first {sample_limit} shown):"
    )
    for item in uncovered[:sample_limit]:
        print(f"  [function] {item.symbol} (identity {item.identity}) <- none")

    print()
    print("NOT DERIVABLE from rustdoc scrape-examples:")
    explanations = {
        "field": "field reads/writes and struct literals are not Call/MethodCall HIR",
        "variant": "variant construction and patterns are not generally function calls",
        "struct": "type construction is not recorded unless a named function is called",
        "enum": "type/variant use is not a direct call",
        "trait": "trait use and the selected concrete impl are absent from the payload",
        "type_alias": "type-level use is not a direct call",
        "constant": "constant path use is not a direct call",
        "assoc_const": "associated-constant use is not a direct call",
        "assoc_type": "associated-type use is not a direct call",
    }
    for kind in (
        "field",
        "variant",
        "struct",
        "enum",
        "trait",
        "type_alias",
        "constant",
        "assoc_const",
        "assoc_type",
    ):
        items = sorted(non_derivable[kind], key=facade_path)
        print(f"  {kind}: {len(items)} item(s) -- {explanations[kind]}")
        for item in items[: min(2, sample_limit)]:
            print(f"    {item.symbol} (identity {item.identity})")
    print(
        "  concrete trait impl/method: not an inventory kind and not present in "
        "the scrape payload; a trait call resolves to its associated-item identity"
    )
    print(
        "  doctest: Cargo's scrape path compiles opted-in targets, not extracted "
        "rustdoc doctest crates"
    )

    missing_contributors = [name for name, count in contributors.items() if count == 0]
    if missing_contributors:
        print(
            "error: no matched facade call was derived for: "
            + ", ".join(missing_contributors),
            file=sys.stderr,
        )
        return 1
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--example-package",
        action="append",
        dest="example_packages",
        help="workspace example package to scrape (repeatable)",
    )
    parser.add_argument(
        "--sample-limit",
        type=int,
        default=8,
        help="maximum uncovered/non-derivable samples to print per group",
    )
    parser.add_argument(
        "--target-dir",
        type=Path,
        help="artifact directory (default: CARGO_TARGET_DIR/fig2090-api-evidence)",
    )
    args = parser.parse_args()
    requested = args.example_packages or list(DEFAULT_EXAMPLE_PACKAGES)
    if len(set(requested)) < 3:
        parser.error("select at least three distinct example packages")
    if args.sample_limit < 1:
        parser.error("--sample-limit must be positive")

    environment = os.environ.copy()
    environment["RUSTC_BOOTSTRAP"] = "1"
    run(["cargo", "+nightly", "--version"], env=environment, capture=True)
    metadata = cargo_metadata()
    packages, crate_to_package = package_maps(metadata)
    unknown = [name for name in requested if name not in packages]
    if unknown:
        parser.error("unknown workspace package(s): " + ", ".join(unknown))
    example_rows = [packages[name] for name in requested]
    outside_examples = [
        package["name"]
        for package in example_rows
        if not Path(package["manifest_path"]).relative_to(REPO).parts[0] == "examples"
    ]
    if outside_examples:
        parser.error("not under examples/: " + ", ".join(outside_examples))

    print("Deriving default-feature canonical facade identities...", file=sys.stderr)
    surface = canonical_surface()
    facade = facade_package(crate_to_package)

    target = (args.target_dir or docs_target(environment)).resolve()
    target.mkdir(parents=True, exist_ok=True)
    print(
        "Scraping " + ", ".join(requested) + " with compiler-coordinated rustdoc...",
        file=sys.stderr,
    )
    with tempfile.TemporaryDirectory(prefix="fig2090-api-evidence-") as temporary:
        snapshot = Path(temporary)
        archive_head(snapshot)
        for package in example_rows:
            opt_in_example_package(snapshot, package)
        elapsed = rustdoc_scrape(
            snapshot, target, facade, requested, environment
        )
        calls = parse_calls(target, {"lash"}, snapshot)
        return print_report(
            surface, calls, example_rows, elapsed, args.sample_limit
        )


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as error:
        command = " ".join(error.cmd) if isinstance(error.cmd, list) else str(error.cmd)
        print(f"command failed ({error.returncode}): {command}", file=sys.stderr)
        raise SystemExit(error.returncode) from error
    except (OSError, RuntimeError, ValueError) as error:
        print(f"api evidence failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
