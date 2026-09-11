#!/usr/bin/env python3
"""Validate and execute the workspace's declared Cargo feature coverage plan."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
FEATURE_REF = re.compile(r'feature\s*=\s*"([A-Za-z0-9_.-]+)"')
RUST_COMMENT_OR_STRING = re.compile(
    r"//[^\n]*|/\*.*?\*/|\"(?:\\.|[^\"\\])*\"", re.DOTALL
)
@dataclass(frozen=True)
class Package:
    name: str
    path: Path
    features: dict[str, list[str]]
    dependencies: dict[str, str]
    dev_self_features: frozenset[str]


def load_toml(path: Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def dependency_sections(manifest: dict[str, Any]) -> list[tuple[str, dict[str, Any]]]:
    sections = [
        (name, manifest.get(name, {}))
        for name in ("dependencies", "dev-dependencies", "build-dependencies")
    ]
    for target in manifest.get("target", {}).values():
        if not isinstance(target, dict):
            continue
        sections.extend(
            (name, target.get(name, {}))
            for name in ("dependencies", "dev-dependencies", "build-dependencies")
        )
    return sections


def workspace_packages(root: Path) -> dict[str, Package]:
    workspace = load_toml(root / "Cargo.toml")
    workspace_dependencies = workspace["workspace"].get("dependencies", {})
    packages: dict[str, Package] = {}

    for member in workspace["workspace"]["members"]:
        path = root / member
        manifest = load_toml(path / "Cargo.toml")
        package_name = manifest["package"]["name"]
        features = {
            name: list(definition)
            for name, definition in manifest.get("features", {}).items()
        }
        dependencies: dict[str, str] = {}
        dev_self_features: set[str] = set()

        for section_name, section in dependency_sections(manifest):
            for alias, raw_spec in section.items():
                spec = raw_spec if isinstance(raw_spec, dict) else {}
                inherited = workspace_dependencies.get(alias, {}) if spec.get("workspace") else {}
                inherited = inherited if isinstance(inherited, dict) else {}
                dependency_name = spec.get("package", inherited.get("package", alias))
                if section_name == "dependencies":
                    dependencies[alias] = dependency_name
                elif section_name == "dev-dependencies" and dependency_name == package_name:
                    dev_self_features.update(spec.get("features", inherited.get("features", [])))

                if spec.get("optional", inherited.get("optional", False)):
                    explicit = any(
                        f"dep:{alias}" in definition
                        for definition in features.values()
                    )
                    if not explicit:
                        features.setdefault(alias, [f"dep:{alias}"])

        packages[package_name] = Package(
            name=package_name,
            path=path,
            features=features,
            dependencies=dependencies,
            dev_self_features=frozenset(dev_self_features),
        )

    return packages


def masked_rust(source: str) -> str:
    """Mask comments and literals while preserving offsets for brace matching."""
    return RUST_COMMENT_OR_STRING.sub(
        lambda match: "".join("\n" if char == "\n" else " " for char in match.group()),
        source,
    )


def balanced_end(source: str, start: int, opening: str, closing: str) -> int | None:
    depth = 0
    for index in range(start, len(source)):
        if source[index] == opening:
            depth += 1
        elif source[index] == closing:
            depth -= 1
            if depth == 0:
                return index
    return None


def cfg_requirements(package: Package) -> dict[str, dict[str, set[str]]]:
    found: dict[str, dict[str, set[str]]] = {}
    for source in package.path.rglob("*.rs"):
        text = source.read_text(encoding="utf-8")
        masked = masked_rust(text)
        attributes: list[tuple[int, int, str]] = []
        for match in re.finditer(r"#\s*\[\s*cfg\s*\(", masked):
            paren = masked.find("(", match.start())
            end = balanced_end(masked, paren, "(", ")")
            if end is None:
                continue
            attributes.append((match.start(), end + 1, text[paren + 1 : end]))

        test_regions: list[tuple[int, int]] = []
        for _, end, predicate in attributes:
            if re.fullmatch(r"\s*test\s*", predicate) is None:
                continue
            brace = masked.find("{", end)
            semicolon = masked.find(";", end)
            if brace == -1 or (semicolon != -1 and semicolon < brace):
                continue
            close = balanced_end(masked, brace, "{", "}")
            if close is not None:
                test_regions.append((brace, close))

        for start, _, predicate in attributes:
            compact = re.sub(r"\s+", "", predicate)
            enclosed_by_test = any(
                region_start < start < region_end for region_start, region_end in test_regions
            )
            if enclosed_by_test or compact.startswith("all(test,"):
                contexts = ("test",)
            elif compact.startswith("any(test,"):
                # With cfg(test) false, the feature selects the normal-library arm.
                # With cfg(test) true, the predicate no longer depends on the feature.
                contexts = ("normal",)
            elif re.search(r"(?:^|[,(])test(?:[,)]|$)", compact):
                contexts = ("test",)
            else:
                contexts = ("normal",)
            for name in FEATURE_REF.findall(predicate):
                state = (
                    "off"
                    if re.search(
                        rf"not\(feature\s*=\s*\"{re.escape(name)}\"\)", compact
                    )
                    else "on"
                )
                requirements = found.setdefault(name, {"normal": set(), "test": set()})
                for context in contexts:
                    if context == "normal":
                        requirements[context].update(("on", "off"))
                    else:
                        requirements[context].add(state)
    return found


def split_coverage(token: str) -> tuple[str, str]:
    feature_id, separator, state = token.rpartition(":")
    if separator != ":" or state not in {"on", "off"} or "/" not in feature_id:
        raise ValueError(f"invalid feature coverage token: {token!r}")
    return feature_id, state


def feature_id(package: str, feature: str) -> str:
    return f"{package}/{feature}"


def command_feature_set(command: list[str]) -> set[str]:
    if "--features" not in command:
        return set()
    index = command.index("--features")
    if index + 1 >= len(command):
        return set()
    return {item for item in command[index + 1].split(",") if item}


def command_package(command: list[str]) -> str | None:
    if "-p" not in command:
        return None
    index = command.index("-p")
    return command[index + 1] if index + 1 < len(command) else None


def cargo_subcommand(command: list[str]) -> str | None:
    if len(command) < 2 or command[0] != "cargo":
        return None
    return command[1]


def command_compiles_tests(command: list[str]) -> bool:
    return cargo_subcommand(command) == "test" or any(
        target in command
        for target in ("--all-targets", "--tests", "--test", "--benches", "--bench")
    )


def command_compiles_normal_library(command: list[str]) -> bool:
    return cargo_subcommand(command) == "check" and any(
        target in command for target in ("--lib", "--all-targets")
    )


def effective_command_features(command: list[str], package: Package) -> set[str]:
    enabled = command_feature_set(command)
    if "--all-features" in command:
        enabled.update(package.features)
    if command_compiles_tests(command):
        enabled.update(package.dev_self_features)
    return enabled


def workflow_job_block(workflow: str, job: str) -> str:
    marker = f"  {job}:\n"
    start = workflow.find(marker)
    if start == -1:
        return ""
    next_job = re.search(
        r"^  [A-Za-z0-9_-]+:\n", workflow[start + len(marker) :], re.MULTILINE
    )
    if next_job is None:
        return workflow[start:]
    return workflow[start : start + len(marker) + next_job.start()]


def validate(root: Path) -> tuple[dict[str, Package], dict[str, Any]]:
    packages = workspace_packages(root)
    plan = load_toml(root / "scripts" / "feature-coverage.toml")
    failures: list[str] = []

    if plan.get("schema") != 1:
        failures.append("scripts/feature-coverage.toml must declare schema = 1")

    declared = {
        feature_id(package.name, feature)
        for package in packages.values()
        for feature in package.features
    }
    package_requirements = {
        package.name: cfg_requirements(package) for package in packages.values()
    }
    non_normal_forwards = {
        f"{feature_id(package.name, name)}->{edge}"
        for package in packages.values()
        for name, definition in package.features.items()
        for edge in definition
        if "/" in edge
        and edge.split("/", 1)[0].removesuffix("?") not in package.dependencies
    }
    planned_non_normal = set(plan.get("non_normal_forwards", []))
    for edge in sorted(non_normal_forwards - planned_non_normal):
        failures.append(f"unmapped non-normal feature forward: {edge}")
    for edge in sorted(planned_non_normal - non_normal_forwards):
        failures.append(f"stale non-normal feature forward: {edge}")
    baseline = set(plan.get("baseline", {}).get("features", []))
    baseline_command = plan.get("baseline", {}).get("command")
    if baseline_command != ["cargo", "check", "--workspace", "--all-targets", "--locked"]:
        failures.append("baseline command must be the locked workspace all-targets check")
    for current_feature in sorted(baseline):
        if not current_feature.endswith("/default"):
            failures.append(f"baseline may own only explicit default features: {current_feature}")
    unresolved_rows = plan.get("unresolved", [])
    unresolved = {row.get("feature") for row in unresolved_rows}
    if None in unresolved:
        failures.append("every [[unresolved]] row needs a feature")
        unresolved.discard(None)

    lanes = plan.get("lane", [])
    lane_names = [lane.get("name") for lane in lanes]
    if None in lane_names or len(set(lane_names)) != len(lane_names):
        failures.append("every [[lane]] needs a unique name")

    on_owners: dict[str, list[str]] = {}
    off_owners: dict[str, list[str]] = {}
    for lane in lanes:
        name = lane.get("name", "<unnamed>")
        commands = lane.get("commands", [])
        if not commands:
            failures.append(f"lane {name!r} has no executable commands")
        for command in commands:
            if not isinstance(command, list) or not all(isinstance(arg, str) for arg in command):
                failures.append(f"lane {name!r} has a non-argv command")
                continue
            subcommand = cargo_subcommand(command)
            if subcommand not in {"check", "test"}:
                failures.append(f"lane {name!r} has a non-Cargo check/test command")
            if "--locked" not in command:
                failures.append(f"lane {name!r} has an unlocked Cargo command")
            if command_package(command) not in packages:
                failures.append(f"lane {name!r} command does not name one workspace package")
            if "--all-features" in command:
                failures.append(f"lane {name!r} command may not use --all-features")
            if subcommand == "check" and not any(
                target in command for target in ("--lib", "--all-targets", "--tests")
            ):
                failures.append(f"lane {name!r} check command lacks an explicit target context")
        for token in lane.get("features", []):
            try:
                current_feature, state = split_coverage(token)
            except ValueError as error:
                failures.append(str(error))
                continue
            owners = on_owners if state == "on" else off_owners
            owners.setdefault(current_feature, []).append(name)

    for current_feature in sorted(declared):
        package_name, name = current_feature.split("/", 1)
        owners = int(current_feature in baseline) + len(on_owners.get(current_feature, []))
        if current_feature in unresolved:
            if owners:
                failures.append(f"unresolved feature {current_feature} must not claim green ownership")
            continue
        if owners == 0:
            failures.append(f"unowned declared feature: {current_feature}")
        elif owners > 1:
            failures.append(f"multiply owned declared feature: {current_feature}")

        requirements = package_requirements[package_name].get(name, {})
        if name != "default" and requirements:
            if not off_owners.get(current_feature):
                failures.append(f"cfg feature lacks an OFF witness: {current_feature}")

    for current_feature in sorted((baseline | set(on_owners) | set(off_owners) | unresolved) - declared):
        failures.append(f"coverage plan names undeclared feature: {current_feature}")

    for current_feature, owners in sorted(off_owners.items()):
        if len(owners) > 1:
            failures.append(f"feature has multiple OFF witnesses: {current_feature}")

    for lane in lanes:
        name = lane.get("name", "<unnamed>")
        commands = lane.get("commands", [])
        for token in lane.get("features", []):
            try:
                current_feature, state = split_coverage(token)
            except ValueError:
                continue
            package_name, name_part = current_feature.split("/", 1)
            package = packages[package_name]
            package_commands = [
                command for command in commands if command_package(command) == package_name
            ]
            candidates = package_commands
            if state == "on":
                candidates = [
                    command
                    for command in candidates
                    if command_feature_set(command) == {name_part}
                ]
            else:
                candidates = [
                    command
                    for command in candidates
                    if "--no-default-features" in command
                    and name_part not in effective_command_features(command, package)
                    and "--all-features" not in command
                ]
            if not candidates:
                failures.append(
                    f"lane {name!r} lacks an exact {state.upper()} command for {current_feature}"
                )
                continue
            requirements = package_requirements[package_name].get(name_part, {})
            for context, states in requirements.items():
                if state not in states:
                    continue
                if (
                    context == "test"
                    and state == "off"
                    and name_part in package.dev_self_features
                ):
                    # Cargo unifies self dev-dependency features into every test
                    # target, so this source predicate has no reachable OFF test
                    # graph. The normal-library OFF command remains mandatory.
                    continue
                contextual = [
                    command
                    for command in package_commands
                    if (
                        command_compiles_tests(command)
                        if context == "test"
                        else command_compiles_normal_library(command)
                    )
                    and "--all-features" not in command
                    and (
                        name_part in effective_command_features(command, package)
                        if state == "on"
                        else name_part not in effective_command_features(command, package)
                    )
                ]
                if not contextual:
                    failures.append(
                        f"lane {name!r} lacks a {context}-context {state.upper()} command "
                        f"for {current_feature}"
                    )

    all_commands = {
        tuple(command)
        for lane in lanes
        for command in lane.get("commands", [])
        if isinstance(command, list)
    }
    proxy_witnesses = plan.get("proxy_witness", [])
    planned_proxy_pairs = {
        (witness.get("feature"), witness.get("dependency"))
        for witness in proxy_witnesses
    }
    required_proxy_pairs = {
        (feature_id(package.name, name), edge)
        for package in packages.values()
        for name, definition in package.features.items()
        if package_requirements[package.name].get(name, {}).get("test")
        and not package_requirements[package.name].get(name, {}).get("normal")
        for edge in definition
        if "/" in edge
        and "?/" not in edge
        and edge.split("/", 1)[0] in package.dependencies
    }
    for current_feature, dependency_edge in sorted(required_proxy_pairs - planned_proxy_pairs):
        failures.append(
            f"test-only proxy lacks local-OFF/dependency-ON witness: "
            f"{current_feature}->{dependency_edge}"
        )
    for witness in proxy_witnesses:
        current_feature = witness.get("feature", "")
        dependency_edge = witness.get("dependency", "")
        command = witness.get("command", [])
        if current_feature not in declared:
            failures.append(f"proxy witness names undeclared feature: {current_feature}")
            continue
        package_name, name_part = current_feature.split("/", 1)
        package = packages[package_name]
        if dependency_edge not in package.features[name_part]:
            failures.append(
                f"proxy witness edge is not declared by {current_feature}: {dependency_edge}"
            )
        if tuple(command) not in all_commands:
            failures.append(f"proxy witness command is not executed by a lane: {current_feature}")
        if (
            command_package(command) != package_name
            or "--no-default-features" not in command
            or name_part in effective_command_features(command, package)
            or dependency_edge not in command_feature_set(command)
        ):
            failures.append(
                f"proxy witness does not compile local OFF with dependency ON: {current_feature}"
            )
        test_states = package_requirements[package_name].get(name_part, {}).get("test", set())
        if test_states and not command_compiles_tests(command):
            failures.append(f"proxy witness misses test context: {current_feature}")

    workflow = (root / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
    feature_job = workflow_job_block(workflow, "package-feature-checks")
    workflow_lanes = set(re.findall(r"^\s*- lane: ([A-Za-z0-9_.-]+)\s*$", workflow, re.MULTILINE))
    for name in lane_names:
        if name not in workflow_lanes:
            failures.append(f"coverage lane missing from ci.yml: {name}")
        invocation = f"python3 scripts/check_feature_coverage.py run {name}"
        if invocation not in feature_job:
            failures.append(f"coverage lane has no ci.yml runner command: {name}")

    runner_step = re.compile(
        r"^      - name: Run exact package feature graph\n"
        r"        run: \$\{\{ matrix\.command \}\}\s*$",
        re.MULTILINE,
    )
    if runner_step.search(feature_job) is None:
        failures.append("package-feature-checks does not execute matrix.command")
    if "github.event_name == 'merge_group'" not in feature_job:
        failures.append("package-feature-checks is not required on merge_group")

    conclusion = workflow_job_block(workflow, "ci-conclusion")
    if re.search(r"^      - package-feature-checks\s*$", conclusion, re.MULTILINE) is None:
        failures.append("ci-conclusion does not require package-feature-checks")
    if re.search(r"^      - test-doc\s*$", conclusion, re.MULTILINE) is None:
        failures.append("ci-conclusion does not require the baseline test-doc job")

    test_doc = workflow_job_block(workflow, "test-doc")
    baseline_shell = "cargo check --workspace --all-targets --locked ${LASH_CI_FEATURES}"
    if baseline_shell not in test_doc:
        failures.append("test-doc does not execute the workspace default baseline")

    repo_gates = workflow_job_block(workflow, "repo-gates")
    for invocation in (
        "python3 scripts/test_check_feature_coverage.py",
        "python3 scripts/check_feature_coverage.py check",
    ):
        if invocation not in repo_gates:
            failures.append(f"repo-gates does not execute {invocation}")

    for row in unresolved_rows:
        current_feature = row.get("feature")
        failures.append(f"unapproved unresolved feature: {current_feature}")

    if failures:
        print("feature coverage contract failed:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        raise SystemExit(1)

    print(
        "feature coverage contract passed: "
        f"{len(declared)} declared features, {len(lanes)} executable lanes, "
        f"{len(unresolved)} explicit unresolved"
    )
    return packages, plan


def run_command(command: list[str], root: Path) -> str:
    print("+ " + " ".join(command), flush=True)
    process = subprocess.Popen(
        command,
        cwd=root,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    output: list[str] = []
    assert process.stdout is not None
    for line in process.stdout:
        output.append(line)
        print(line, end="", flush=True)
    return_code = process.wait()
    combined = "".join(output)
    if return_code != 0:
        raise subprocess.CalledProcessError(return_code, command, output=combined)
    return combined


def resolver_witness(
    root: Path, packages: dict[str, Package], current_feature: str, state: str
) -> None:
    package_name, name = current_feature.split("/", 1)
    command = [
        "cargo",
        "tree",
        "-p",
        package_name,
        "--no-default-features",
    ]
    if state == "on":
        command.extend(["--features", name])
    command.extend(["-e", "normal,features", "-i", package_name, "--locked"])
    output = run_command(command, root)
    marker = f'{package_name} feature "{name}"'
    if state == "on" and marker not in output:
        raise SystemExit(f"resolver did not enable {current_feature}")
    if state == "off" and marker in output:
        raise SystemExit(f"resolver unexpectedly enabled {current_feature}")

    if state != "on":
        return
    definition = packages[package_name].features[name]
    for edge in definition:
        if "?/" in edge:
            continue
        if edge.startswith("dep:"):
            alias = edge.removeprefix("dep:")
            dependency = packages[package_name].dependencies.get(alias)
            if dependency is None:
                print(f"resolver note: {current_feature} has non-normal edge {edge}")
                continue
            graph = run_command(
                [
                    "cargo",
                    "tree",
                    "-p",
                    package_name,
                    "--no-default-features",
                    "--features",
                    name,
                    "-e",
                    "normal,features",
                    "-i",
                    dependency,
                    "--locked",
                ],
                root,
            )
            if dependency not in graph:
                raise SystemExit(
                    f"resolver did not activate optional dependency {dependency} for {current_feature}"
                )
        elif "/" in edge:
            alias, forwarded = edge.split("/", 1)
            dependency = packages[package_name].dependencies.get(alias)
            if dependency is None:
                print(f"resolver note: {current_feature} has non-normal edge {edge}")
                continue
            graph = run_command(
                [
                    "cargo",
                    "tree",
                    "-p",
                    package_name,
                    "--no-default-features",
                    "--features",
                    name,
                    "-e",
                    "normal,features",
                    "-i",
                    dependency,
                    "--locked",
                ],
                root,
            )
            marker = f'{dependency} feature "{forwarded}"'
            if marker not in graph:
                raise SystemExit(
                    f"resolver did not forward {dependency}/{forwarded} for {current_feature}"
                )


def resolver_proxy_witness(
    root: Path, packages: dict[str, Package], witness: dict[str, Any]
) -> None:
    current_feature = witness["feature"]
    package_name, name = current_feature.split("/", 1)
    dependency_edge = witness["dependency"]
    alias, forwarded = dependency_edge.split("/", 1)
    dependency = packages[package_name].dependencies[alias]
    base = [
        "cargo",
        "tree",
        "-p",
        package_name,
        "--no-default-features",
        "--features",
        dependency_edge,
        "-e",
        "normal,features",
    ]
    local_graph = run_command([*base, "-i", package_name, "--locked"], root)
    if f'{package_name} feature "{name}"' in local_graph:
        raise SystemExit(f"proxy witness unexpectedly enabled local feature {current_feature}")
    dependency_graph = run_command([*base, "-i", dependency, "--locked"], root)
    if f'{dependency} feature "{forwarded}"' not in dependency_graph:
        raise SystemExit(
            f"proxy witness did not independently enable {dependency}/{forwarded}"
        )


def run_lane(root: Path, lane_name: str) -> None:
    packages, plan = validate(root)
    lanes = {lane["name"]: lane for lane in plan["lane"]}
    if lane_name not in lanes:
        raise SystemExit(f"unknown feature coverage lane: {lane_name}")
    lane = lanes[lane_name]
    for token in lane["features"]:
        current_feature, state = split_coverage(token)
        resolver_witness(root, packages, current_feature, state)
    owned_features = {split_coverage(token)[0] for token in lane["features"]}
    for witness in plan.get("proxy_witness", []):
        if witness["feature"] in owned_features:
            resolver_proxy_witness(root, packages, witness)
    for command in lane["commands"]:
        run_command(command, root)
    print(f"feature coverage lane passed: {lane_name}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("check", "run"))
    parser.add_argument("lane", nargs="?")
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    root = args.root.resolve()
    if args.action == "check":
        if args.lane is not None:
            parser.error("check does not accept a lane")
        validate(root)
    else:
        if args.lane is None:
            parser.error("run requires a lane")
        run_lane(root, args.lane)


if __name__ == "__main__":
    main()
