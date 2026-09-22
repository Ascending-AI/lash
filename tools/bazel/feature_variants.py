#!/usr/bin/env python3
"""Cargo's per-command feature resolution, expressed as Bazel variant targets.

`scripts/feature-coverage.toml` declares lanes of exact Cargo commands --
`cargo check -p X --lib --no-default-features --features Y` and friends. Each
command is a *different* resolution of X's first-party closure from the one
`cargo metadata --locked` reports for the workspace: every workspace dependency
receives only what X's request implies, so the crate's `cfg(feature = ...)`
arms compile in a shape the ordinary workspace graph never builds.

This module reimplements that resolution over `cargo metadata --locked` so the
generator can emit one Bazel target per distinct `(package, resolved features,
target kind)` unit, deduplicated across every command of every lane. The
resolution is Cargo's own algorithm restricted to the facts this workspace
actually exhibits, each of which is asserted by `--check` rather than assumed:

* No first-party dependency is target-conditional, so there is no `cfg()`
  dependency edge to evaluate and the single `x86_64-unknown-linux-gnu`
  platform of `MODULE.bazel` is the only one in play.
* Every optional first-party dependency is referenced explicitly as `dep:name`
  by some feature, so no package acquires an *implicit* optional-dependency
  feature whose name shadows a dependency.
* Feature unification is resolver v2: a feature enabled through a
  build-dependency edge does not unify with the normal edge, and the two
  first-party build-dependency edges in the workspace are resolved in their own
  context.

Third-party crates are deliberately NOT re-resolved; see the limitation
recorded in `docs/agents/hermetic-build.md`.
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass, field


# `cargo metadata` reports the dependency's real package name plus the name the
# dependent actually writes; a feature string like `lash-core-ids/otel-trace`
# names the latter.
def dependency_alias(dependency: dict) -> str:
    return dependency.get("rename") or dependency["name"]


@dataclass(frozen=True)
class Workspace:
    """The first-party half of `cargo metadata`, keyed by package name."""

    packages: dict[str, dict]

    @staticmethod
    def from_metadata(metadata: dict) -> "Workspace":
        members = set(metadata["workspace_members"])
        packages = {
            package["name"]: package
            for package in metadata["packages"]
            if package["id"] in members
        }
        return Workspace(packages=packages)

    def is_member(self, name: str) -> bool:
        return name in self.packages


@dataclass
class Resolution:
    """One command's resolved feature set for every first-party package."""

    features: dict[str, set[str]] = field(default_factory=dict)
    # Dependency aliases activated per package, so `dep?/feature` can be
    # answered without re-walking.
    activated: dict[str, set[str]] = field(default_factory=dict)

    def sorted_features(self) -> dict[str, list[str]]:
        return {name: sorted(values) for name, values in sorted(self.features.items())}


class _Resolver:
    def __init__(self, workspace: Workspace, build_context: bool = False) -> None:
        self.workspace = workspace
        self.build_context = build_context
        self.resolution = Resolution()
        self.deferred: dict[tuple[str, str], set[str]] = {}

    # -- graph walk ---------------------------------------------------------

    def activate_package(self, name: str, with_dev: bool = False) -> None:
        if not self.workspace.is_member(name):
            return
        first_visit = name not in self.resolution.features
        if not first_visit and not with_dev:
            return
        if first_visit:
            self.resolution.features[name] = set()
            self.resolution.activated[name] = set()
        package = self.workspace.packages[name]
        kinds = {None} if first_visit else set()
        if with_dev:
            kinds.add("dev")
        for dependency in package["dependencies"]:
            if dependency["kind"] not in kinds:
                continue
            if dependency["optional"]:
                continue
            self.activate_dependency(name, dependency)

    def activate_dependency(self, owner: str, dependency: dict) -> None:
        alias = dependency_alias(dependency)
        target = dependency["name"]
        self.resolution.activated.setdefault(owner, set()).add(alias)
        if not self.workspace.is_member(target):
            return
        self.activate_package(target)
        if dependency.get("uses_default_features", True):
            self.activate_feature(target, "default")
        for feature in dependency.get("features", []):
            self.activate_feature(target, feature)
        for feature in sorted(self.deferred.pop((owner, alias), set())):
            self.activate_feature(target, feature)

    def dependency_named(self, owner: str, alias: str) -> dict | None:
        package = self.workspace.packages[owner]
        for dependency in package["dependencies"]:
            if dependency["kind"] is not None:
                continue
            if dependency_alias(dependency) == alias:
                return dependency
        return None

    # -- feature activation -------------------------------------------------

    def activate_feature(self, name: str, feature: str) -> None:
        if not self.workspace.is_member(name):
            return
        self.activate_package(name)
        enabled = self.resolution.features[name]
        if feature in enabled:
            return
        package = self.workspace.packages[name]
        if feature not in package["features"]:
            # `default` on a package that declares none is a no-op for Cargo.
            return
        enabled.add(feature)
        for value in package["features"][feature]:
            self.activate_feature_value(name, value)

    def activate_feature_value(self, owner: str, value: str) -> None:
        if value.startswith("dep:"):
            alias = value[4:]
            dependency = self.dependency_named(owner, alias)
            if dependency is not None:
                self.activate_dependency(owner, dependency)
            return
        if "/" in value:
            head, feature = value.split("/", 1)
            weak = head.endswith("?")
            alias = head[:-1] if weak else head
            dependency = self.dependency_named(owner, alias)
            if dependency is None:
                return
            already = alias in self.resolution.activated.get(owner, set())
            if weak and not already:
                self.deferred.setdefault((owner, alias), set()).add(feature)
                return
            if not already:
                self.activate_dependency(owner, dependency)
            self.activate_feature(dependency["name"], feature)
            return
        self.activate_feature(owner, value)


# -- the public entry point -------------------------------------------------


def resolve_request(
    workspace: Workspace,
    package_name: str,
    *,
    default_features: bool,
    requested: list[str],
    with_dev: bool,
) -> Resolution:
    """Resolve one `-p package [--no-default-features] [--features ...]`."""

    resolver = _Resolver(workspace)
    resolver.activate_package(package_name, with_dev=with_dev)
    if default_features:
        resolver.activate_feature(package_name, "default")
    for token in requested:
        for value in token.split(","):
            value = value.strip()
            if value:
                resolver.activate_feature_value(package_name, value)
    return resolver.resolution


# -- command parsing --------------------------------------------------------

# The Cargo target selectors a lane command may carry, mapped to the set of
# Cargo target kinds the command compiles. `cargo test` and `cargo check
# --tests` differ in what they RUN, never in what they compile.
SELECTOR_KINDS = {
    "--lib": ("lib",),
    "--tests": ("lib", "test", "bin"),
    "--all-targets": ("lib", "test", "bin", "example", "bench"),
}
DEV_SELECTORS = {"--tests", "--all-targets"}


@dataclass(frozen=True)
class Command:
    package: str
    subcommand: str
    default_features: bool
    features: tuple[str, ...]
    selector: str | None
    argv: tuple[str, ...]
    test_names: tuple[str, ...] = ()

    @property
    def with_dev(self) -> bool:
        if self.subcommand == "test" or self.test_names:
            return True
        return self.selector in DEV_SELECTORS

    @property
    def kinds(self) -> tuple[str, ...]:
        if self.test_names:
            return ("test", "bin")
        if self.selector is not None:
            return SELECTOR_KINDS[self.selector]
        # A bare `cargo test -p X` compiles the library, its unit tests, the
        # integration tests and the binaries under test.
        if self.subcommand == "test":
            return ("lib", "test", "bin")
        return ("lib",)


def parse_command(argv: list[str]) -> Command:
    package = None
    features: list[str] = []
    test_names: list[str] = []
    default_features = True
    selector = None
    index = 0
    while index < len(argv):
        token = argv[index]
        if token == "-p" or token == "--package":
            package = argv[index + 1]
            index += 2
            continue
        if token == "--test":
            test_names.append(argv[index + 1])
            index += 2
            continue
        if token == "--features":
            features.append(argv[index + 1])
            index += 2
            continue
        if token == "--no-default-features":
            default_features = False
        elif token in SELECTOR_KINDS:
            selector = token
        index += 1
    if package is None:
        raise ValueError(f"lane command selects no package: {argv}")
    return Command(
        package=package,
        subcommand=argv[1],
        default_features=default_features,
        features=tuple(features),
        selector=selector,
        argv=tuple(argv),
        test_names=tuple(test_names),
    )


def variant_hash(package_name: str, features: list[str]) -> str:
    """A stable short name for one resolved feature set of one package.

    Keyed by the package as well as the features so two packages that happen to
    resolve to the same feature names never share a suffix in a diff.
    """
    digest = hashlib.blake2b(
        ("\n".join([package_name, *features])).encode("utf-8"), digest_size=4
    )
    return digest.hexdigest()
