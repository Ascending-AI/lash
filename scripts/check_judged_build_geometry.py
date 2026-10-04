#!/usr/bin/env python3
"""Gate the build geometry that judged runbooks score.

A judged runbook scores what a host actually ships, so the example binaries it
drives must not carry development-only self-checks. Two things drifted before
this gate existed:

* `examples/agent-workbench` enabled `lash-protocol-rlm`'s `testing` feature on
  its *runtime* dependency, so an exhausted Lashlang execution bound — an
  in-contract outcome the runtime reports back as `Policy` feedback — tripped a
  test-only assertion inside the effect task and surfaced as `effect_panicked`
  → an opaque "turn could not be completed".
* Judged hosts booted the `dev` profile, so `debug_assert!` had the same effect.

The fix is the workspace `judged` profile plus a rule that no judged host
enables a `testing` feature at runtime. This gate keeps both true.

The judged profile now has a second spelling. `scripts/agent-workbench-dev.sh`
builds its host through Buck2 so every checkout on the box shares one action
cache instead of compiling the workspace again in its own target directory
(FIG-3153), and Buck2 does not read `[profile.judged]`. `--config=judged`
selects `//tools/buck2:judged`, whose configuration says the same two things
in rustc flags. Nothing in rustc's own
defaults would: `-C debug-assertions` defaults to ON at `-C opt-level=0`, which
is exactly the geometry a judged row must not score. So this gate reads both
spellings out of their own files and refuses them to drift, and it refuses a
launcher that builds a judged host through Buck2 without naming the config.

It also pins where the build happens. The workbench launcher used to compile
inside `/tmp/lash-agent-workbench-$UID/data-ownership.lock`, a lock every
checkout on the box shares, so one lane's cold build stalled every other
stack's boot and teardown. A build proves nothing about ownership; it belongs
outside the locks, and `check_build_precedes_launcher_locks` keeps it there.

"""

from __future__ import annotations

import ast
import importlib.util
import pathlib
import re
import shlex
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]

# Examples a judged runbook boots as a host. Kept explicit rather than derived
# from the runbook tree: adding a judged host is a deliberate act, and the
# reviewer should see the geometry claim land in this list.
JUDGED_HOSTS = (
    "agent-workbench",
    "workflow-graph-roundtrip",
)

# Files that launch a judged host. Product docs under `docs/` show readers the
# ordinary `cargo run` and are deliberately out of scope.
BOOT_SITES = ("justfile",)

BOOT_GLOBS = ("scripts/*-dev.sh", "runbooks/*/runbook.md", "runbooks/RULES.md")

PROFILE_REQUIREMENTS = {
    "inherits": "dev",
    "debug-assertions": False,
    "overflow-checks": False,
}

CARGO_BOOT = re.compile(
    r"cargo\s+(?:run|build)\s+-p\s+(" + "|".join(JUDGED_HOSTS) + r")\b([^\n]*)"
)


def check_profile(failures: list[str]) -> None:
    with (ROOT / "Cargo.toml").open("rb") as handle:
        manifest = tomllib.load(handle)
    profile = manifest.get("profile", {}).get("judged")
    if profile is None:
        failures.append(
            "Cargo.toml: [profile.judged] is missing; judged runbooks have no "
            "shipping-shaped build to boot"
        )
        return
    for key, expected in PROFILE_REQUIREMENTS.items():
        actual = profile.get(key)
        if actual != expected:
            failures.append(
                f"Cargo.toml: [profile.judged] {key} is {actual!r}, expected {expected!r}"
            )


def check_no_runtime_testing_features(failures: list[str]) -> None:
    for manifest_path in sorted((ROOT / "examples").glob("*/Cargo.toml")):
        with manifest_path.open("rb") as handle:
            manifest = tomllib.load(handle)
        if manifest.get("package", {}).get("name") not in JUDGED_HOSTS:
            continue
        for section in ("dependencies", "build-dependencies"):
            for name, spec in manifest.get(section, {}).items():
                if not isinstance(spec, dict):
                    continue
                if "testing" in spec.get("features", []):
                    rel = manifest_path.relative_to(ROOT)
                    failures.append(
                        f"{rel}: [{section}] {name} enables the `testing` feature; a "
                        "judged host must ship without it (move the entry to "
                        "[dev-dependencies])"
                    )


def boot_files() -> list[pathlib.Path]:
    paths = [ROOT / name for name in BOOT_SITES]
    for pattern in BOOT_GLOBS:
        paths.extend(sorted(ROOT.glob(pattern)))
    return [path for path in paths if path.is_file()]


# A launcher shared with the scripted evidence layer may take its profile from a
# shell variable, but the variable's default must still be `judged`: a caller has
# to opt out deliberately, and forgetting the flag can never silently downgrade a
# judged row.
PROFILE_VARIABLE = re.compile(r'--profile\s+"?\$\{?(\w+)')


def variable_defaults_to_judged(text: str, name: str) -> bool:
    default = re.compile(rf'{re.escape(name)}="\$\{{\w+:-judged\}}"')
    return bool(default.search(text))


def check_boot_sites(failures: list[str]) -> None:
    seen = False
    for path in boot_files():
        text = path.read_text(encoding="utf-8")
        for match in CARGO_BOOT.finditer(text):
            seen = True
            host, tail = match.group(1), match.group(2)
            if "--profile judged" in tail:
                continue
            variable = PROFILE_VARIABLE.search(tail)
            if variable and variable_defaults_to_judged(text, variable.group(1)):
                continue
            line = text.count("\n", 0, match.start()) + 1
            rel = path.relative_to(ROOT)
            failures.append(
                f"{rel}:{line}: boots `{host}` without `--profile judged`; judged "
                "hosts must not carry debug assertions"
            )
    if not seen:
        failures.append(
            "no judged host boot command found; the gate's file list has gone stale"
        )


# Cargo's artifact directory is not the profile name. `dev` writes to
# `target/debug`; every other profile uses its own name. A launcher that pastes
# the profile straight into the path silently points at a directory that never
# exists, and the error it produces ("binary is missing") names the wrong cause.
TARGET_SUBDIR = re.compile(r"CARGO_TARGET_DIR[^}]*\}/([A-Za-z0-9_-]+)/")
VARIABLE_PROFILE = re.compile(r'--profile\s+"?\$')
DEV_MAPS_TO_DEBUG = re.compile(r"^\s*dev\).*\bdebug\b", re.MULTILINE)


def declared_profiles() -> set[str]:
    with (ROOT / "Cargo.toml").open("rb") as handle:
        manifest = tomllib.load(handle)
    return set(manifest.get("profile", {}))


def check_artifact_dirs(failures: list[str]) -> None:
    # `dev` is deliberately absent: it is never a valid artifact directory.
    valid = {"debug", "release"} | (declared_profiles() - {"dev", "release"})
    for path in boot_files():
        text = path.read_text(encoding="utf-8")
        rel = path.relative_to(ROOT)
        for match in TARGET_SUBDIR.finditer(text):
            subdir = match.group(1)
            if subdir in valid:
                continue
            line = text.count("\n", 0, match.start()) + 1
            hint = (
                " (cargo writes the `dev` profile to `target/debug`)"
                if subdir == "dev"
                else ""
            )
            failures.append(
                f"{rel}:{line}: builds an artifact path under `{subdir}/`, which is not "
                f"a cargo artifact directory{hint}"
            )
        # A launcher whose profile is a variable must translate it, because the
        # variable can hold `dev`.
        if VARIABLE_PROFILE.search(text) and not DEV_MAPS_TO_DEBUG.search(text):
            failures.append(
                f"{rel}: takes its cargo profile from a variable but never maps `dev` to "
                "the `debug` artifact directory"
            )


# A profile override belongs to a whole gate run and must reach every child.
PROFILE_ASSIGNMENT = re.compile(r"^(?P<lead>[^\n#]*?)(?P<var>\w*CARGO_PROFILE)=", re.MULTILINE)


def check_profile_overrides_exported(failures: list[str]) -> None:
    for path in sorted((ROOT / "scripts").glob("*.sh")):
        text = path.read_text(encoding="utf-8")
        for match in PROFILE_ASSIGNMENT.finditer(text):
            if match.group("lead").strip().endswith("export"):
                continue
            line = text.count("\n", 0, match.start()) + 1
            rel = path.relative_to(ROOT)
            failures.append(
                f"{rel}:{line}: sets {match.group('var')} without `export`; the override "
                "must reach every child of the run, not one command"
            )


# The Buck2 spelling of the judged profile. The selected configuration must
# carry the same rustc flags as Cargo's `[profile.judged]`.
BUCK2_JUDGED_PLATFORM = "//tools/buck2:judged"
BUCK2_PROFILE_FLAGS = {
    "debug-assertions": "-Cdebug-assertions=no",
    "overflow-checks": "-Coverflow-checks=no",
}


def starlark_function(text: str, name: str) -> str | None:
    match = re.search(
        rf"^def {re.escape(name)}\([^\n]*\)(?:\s*->\s*[^:]+)?:\n"
        r"(?P<body>.*?)(?=^def |\Z)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    return match.group("body") if match else None


def selected_flags(body: str, constraint: str) -> set[str]:
    match = re.search(
        rf'"{re.escape(constraint)}"\s*:\s*\[(?P<flags>.*?)\]',
        body,
        re.DOTALL,
    )
    return set(re.findall(r'"([^"]+)"', match.group("flags"))) if match else set()


def driver_config_command(config: str) -> list[str]:
    path = ROOT / "tools/buck2/driver.py"
    if not path.is_file():
        return []
    module_name = "lash_buck2_driver_geometry_check"
    spec = importlib.util.spec_from_file_location(module_name, path)
    if spec is None or spec.loader is None:
        return []
    module = importlib.util.module_from_spec(spec)
    tool_path = str(path.parent)
    sys.path.insert(0, tool_path)
    try:
        spec.loader.exec_module(module)
    finally:
        sys.path.remove(tool_path)
    options, remaining = module.arguments(
        ["build", f"--config={config}", "//examples/agent-workbench:agent-workbench"]
    )
    return module.command(
        options,
        remaining,
        pathlib.Path("/fixture/buck2"),
        ROOT,
        {"packages": [], "feature_lane_units": []},
    )


def selected_target_platform(command: list[str]) -> str | None:
    for index, argument in enumerate(command[:-1]):
        if argument == "--target-platforms":
            return command[index + 1]
    return None


def check_buck2_judged_config(failures: list[str]) -> None:
    platform = ROOT / "tools/buck2/BUCK"
    first_party = ROOT / "tools/buck2/lash_rust.bzl"
    third_party = ROOT / "tools/buck2/third_party.bzl"
    if not all(path.is_file() for path in (platform, first_party, third_party)):
        failures.append("tools/buck2 judged platform or Rust macro configuration is missing")
        return
    platform_text = platform.read_text(encoding="utf-8")
    platform_call = re.search(
        r'^platform\(\s*name\s*=\s*"judged"\s*,(?P<body>.*?)^\)',
        platform_text,
        re.MULTILINE | re.DOTALL,
    )
    if platform_call is None or '":profile_judged"' not in platform_call.group("body"):
        failures.append("tools/buck2/BUCK: judged platform does not select profile_judged")
    try:
        mapped_platform = selected_target_platform(driver_config_command("judged"))
    except (Exception, SystemExit) as error:
        failures.append(f"tools/buck2/driver.py: cannot resolve --config=judged: {error}")
    else:
        if mapped_platform != BUCK2_JUDGED_PLATFORM:
            failures.append(
                "tools/buck2/driver.py: --config=judged does not select "
                f"{BUCK2_JUDGED_PLATFORM}"
            )

    first_text = first_party.read_text(encoding="utf-8")
    rustc_flags = starlark_function(first_text, "_rustc_flags")
    first_selected = selected_flags(rustc_flags or "", "//tools/buck2:profile_judged")
    first_effective = bool(
        rustc_flags
        and re.search(r"\+\s*judged\s*$", rustc_flags.rstrip())
        and first_text.count("rustc_flags = _rustc_flags(") >= 3
    )

    third_text = third_party.read_text(encoding="utf-8")
    profile_kwargs = starlark_function(third_text, "_profile_kwargs")
    third_appends = []
    if profile_kwargs:
        third_appends = list(
            re.finditer(
                r"flags\s*\+=\s*select\(\{(?P<body>.*?)\}\)",
                profile_kwargs,
                re.DOTALL,
            )
        )
    third_append = next(
        (
            append
            for append in third_appends
            if selected_flags(
                append.group("body"), "//tools/buck2:profile_judged"
            )
        ),
        None,
    )
    third_selected = (
        selected_flags(third_append.group("body"), "//tools/buck2:profile_judged")
        if third_append
        else set()
    )
    third_assignment = (
        profile_kwargs.find('result["rustc_flags"] = flags')
        if profile_kwargs
        else -1
    )
    third_effective = bool(
        profile_kwargs
        and third_append
        and third_append.end() < third_assignment
        and third_text.count("_profile_kwargs(kwargs)") >= 3
    )

    required = {
        flag
        for key, flag in BUCK2_PROFILE_FLAGS.items()
        if PROFILE_REQUIREMENTS.get(key) is False
    }
    if not first_effective or first_selected != required:
        failures.append(
            "tools/buck2/lash_rust.bzl: profile_judged flags are not appended "
            "to first-party target rustc_flags"
        )
    if not third_effective or third_selected != required:
        failures.append(
            "tools/buck2/third_party.bzl: profile_judged flags are not appended "
            "to third-party target rustc_flags"
        )


BUCK2_OPTIMIZED_PLATFORM = "//tools/buck2:optimized"
BUCK2_OPTIMIZED_FLAGS = {
    "-Copt-level=3",
    "-Cdebuginfo=0",
    "-Cstrip=debuginfo",
    "-Cembed-bitcode=no",
}


def starlark_literal(text: str, name: str) -> object | None:
    match = re.search(rf"^{re.escape(name)}\s*=\s*", text, re.MULTILINE)
    if match is None or match.end() == len(text):
        return None
    opening = text[match.end()]
    closing = {"[": "]", "{": "}"}.get(opening)
    if closing is None:
        return None
    depth = 0
    quote: str | None = None
    escaped = False
    for index in range(match.end(), len(text)):
        character = text[index]
        if quote is not None:
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == quote:
                quote = None
            continue
        if character in ('"', "'"):
            quote = character
        elif character == opening:
            depth += 1
        elif character == closing:
            depth -= 1
            if depth == 0:
                try:
                    return ast.literal_eval(text[match.end() : index + 1])
                except (SyntaxError, ValueError):
                    return None
    return None


def evaluate_first_party_flags(
    text: str,
    opt_levels: dict[str, int],
    constraint: str,
    package: str,
) -> list[str]:
    function = re.search(
        r"^def _rustc_flags\([^\n]*\):\n.*?(?=^def |\Z)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    optimized = starlark_literal(text, "_OPTIMIZED_FLAGS")
    if function is None or not isinstance(optimized, list):
        raise ValueError("first-party optimized flag function is not evaluable")

    def select(options: dict[str, list[str]]) -> list[str]:
        value = options.get(constraint, options.get("DEFAULT", []))
        return list(value)

    namespace = {
        "FIRST_PARTY_OPT_LEVELS": opt_levels,
        "_OPTIMIZED_FLAGS": optimized,
        "select": select,
    }
    lint_text = (ROOT / "tools/buck2/clippy_policy.bzl").read_text(encoding="utf-8")
    for name in ("FIRST_PARTY_RUST_LINT_FLAGS", "FIRST_PARTY_CLIPPY_LINT_FLAGS"):
        flags = starlark_literal(lint_text, name)
        if not isinstance(flags, list):
            raise ValueError(f"{name} is not a generated list")
        namespace[name] = flags
    exec(function.group(0), namespace)
    return namespace["_rustc_flags"](package, [], [])


def evaluate_third_party_flags(
    text: str,
    opt_levels: dict[str, int],
    constraint: str,
    package: str,
) -> list[str]:
    function = re.search(
        r"^def _profile_kwargs\([^\n]*\):\n.*?(?=^def |\Z)",
        text,
        re.MULTILINE | re.DOTALL,
    )
    optimized = starlark_literal(text, "_OPTIMIZED_FLAGS")
    if function is None or not isinstance(optimized, list):
        raise ValueError("third-party optimized flag function is not evaluable")

    def select(options: dict[str, list[str]]) -> list[str]:
        value = options.get(constraint, options.get("DEFAULT", []))
        return list(value)

    namespace = {
        "THIRD_PARTY_OPT_LEVELS": opt_levels,
        "_OPTIMIZED_FLAGS": optimized,
        "select": select,
    }
    exec(function.group(0), namespace)
    result = namespace["_profile_kwargs"](
        {"env": {"CARGO_PKG_NAME": package}, "rustc_flags": []}
    )
    flags = result.get("rustc_flags")
    if not isinstance(flags, list):
        raise ValueError("third-party profile did not return rustc_flags")
    return flags


def effective_opt_level(flags: list[str]) -> int | None:
    effective = None
    for flag in flags:
        match = re.fullmatch(r"-Copt-level=(\d+)", flag)
        if match:
            effective = int(match.group(1))
    return effective


def check_buck2_optimized_config(failures: list[str]) -> None:
    platform = ROOT / "tools/buck2/BUCK"
    first_party = ROOT / "tools/buck2/lash_rust.bzl"
    profile = ROOT / "tools/buck2/profile.bzl"
    third_party = ROOT / "tools/buck2/third_party.bzl"
    host_transition = ROOT / "tools/buck2/host_transition.bzl"
    toolchain = ROOT / "tools/buck2/toolchains/rust.bzl"
    overlay = ROOT / "tools/buck2/prelude_overlay.py"
    if not all(
        path.is_file()
        for path in (
            platform,
            first_party,
            profile,
            third_party,
            host_transition,
            toolchain,
            overlay,
        )
    ):
        failures.append(
            "tools/buck2 optimized platform, Rust profile rules, toolchain, or "
            "prelude overlay is missing"
        )
        return

    platform_text = platform.read_text(encoding="utf-8")
    platform_call = re.search(
        r'^platform\(\s*name\s*=\s*"optimized"\s*,(?P<body>.*?)^\)',
        platform_text,
        re.MULTILINE | re.DOTALL,
    )
    if platform_call is None or '":profile_optimized"' not in platform_call.group("body"):
        failures.append(
            "tools/buck2/BUCK: optimized platform does not select profile_optimized"
        )

    try:
        command = driver_config_command("optimized")
    except (Exception, SystemExit) as error:
        failures.append(f"tools/buck2/driver.py: cannot resolve --config=optimized: {error}")
    else:
        if selected_target_platform(command) != BUCK2_OPTIMIZED_PLATFORM:
            failures.append(
                "tools/buck2/driver.py: --config=optimized does not select "
                f"{BUCK2_OPTIMIZED_PLATFORM}"
            )
        settings = {
            command[index + 1]
            for index, argument in enumerate(command[:-1])
            if argument == "-c"
        }
        if "kiln.rust_profile=optimized" not in settings:
            failures.append(
                "tools/buck2/driver.py: --config=optimized does not preserve the "
                "optimized profile across Rust exec transitions"
            )

    host_profile = (
        'constraint_value(name = "profile_host", constraint_setting = ":profile")'
        in platform_text
        and 'host = ":profile_host"' in platform_text
    )
    if not host_profile:
        failures.append(
            "tools/buck2/BUCK: optimized Rust host actions do not select profile_host"
        )

    first_text = first_party.read_text(encoding="utf-8")
    third_text = third_party.read_text(encoding="utf-8")
    host_text = host_transition.read_text(encoding="utf-8")
    host_impl = starlark_function(host_text, "_host_transition_impl") or ""
    host_transition_effective = (
        "constraints[host.setting.label] = host" in host_impl
        and "constraints.pop(budget.label, None)" in host_impl
        and "incoming_transition = _HOST_TRANSITION" in first_text
        and third_text.count('kwargs["incoming_transition"] = _HOST_TRANSITION') >= 2
    )
    if not host_transition_effective:
        failures.append(
            "tools/buck2 host transition: build scripts and proc macros do not "
            "retain optimized profile_host geometry"
        )

    first_optimized = starlark_literal(first_text, "_OPTIMIZED_FLAGS")
    third_optimized = starlark_literal(third_text, "_OPTIMIZED_FLAGS")
    if (
        not isinstance(first_optimized, list)
        or set(first_optimized) != BUCK2_OPTIMIZED_FLAGS
        or not isinstance(third_optimized, list)
        or set(third_optimized) != BUCK2_OPTIMIZED_FLAGS
    ):
        failures.append(
            "tools/buck2 Rust macros: optimized Rust flags do not match the repository profile"
        )

    profile_text = profile.read_text(encoding="utf-8")
    levels = starlark_literal(profile_text, "FIRST_PARTY_OPT_LEVELS")
    third_levels = starlark_literal(profile_text, "THIRD_PARTY_OPT_LEVELS")
    try:
        if not isinstance(levels, dict):
            raise ValueError("FIRST_PARTY_OPT_LEVELS is not a dictionary")
        if not isinstance(third_levels, dict):
            raise ValueError("THIRD_PARTY_OPT_LEVELS is not a dictionary")
        target_regress = evaluate_first_party_flags(
            first_text, levels, "//tools/buck2:profile_optimized", "lash-regress"
        )
        target_ordinary = evaluate_first_party_flags(
            first_text, levels, "//tools/buck2:profile_optimized", "lash-core"
        )
        host_regress = evaluate_first_party_flags(
            first_text, levels, "//tools/buck2:profile_host", "lash-regress"
        )
        target_dependency = evaluate_third_party_flags(
            third_text, third_levels, "//tools/buck2:profile_optimized", "serde"
        )
        host_dependency = evaluate_third_party_flags(
            third_text, third_levels, "//tools/buck2:profile_host", "serde"
        )
    except (KeyError, NameError, TypeError, ValueError, SyntaxError) as error:
        failures.append(
            "tools/buck2/lash_rust.bzl: optimized Rust flag ordering cannot be "
            f"evaluated: {error}"
        )
    else:
        regress_opt3 = (
            target_regress.index("-Copt-level=3")
            if "-Copt-level=3" in target_regress
            else -1
        )
        regress_opt2 = (
            target_regress.index("-Copt-level=2")
            if "-Copt-level=2" in target_regress
            else -1
        )
        effective = (
            levels.get("lash-regress") == 2
            and regress_opt3 >= 0
            and regress_opt3 < regress_opt2
            and effective_opt_level(target_regress) == 2
            and effective_opt_level(target_ordinary) == 3
            and effective_opt_level(host_regress) == 3
            and "-Copt-level=2" not in host_regress
            and effective_opt_level(target_dependency) == 3
            and effective_opt_level(host_dependency) == 3
        )
        if not effective:
            failures.append(
                "tools/buck2 Rust macros: optimized Rust flag ordering does not "
                "preserve the lash-regress target opt2 override and target/host opt3"
            )

    toolchain_text = toolchain.read_text(encoding="utf-8")
    rust_impl = starlark_function(toolchain_text, "_rust") or ""
    if "extra_rustc_flags = []" not in rust_impl or "_OPTIMIZED_FLAGS" in rust_impl:
        failures.append(
            "tools/buck2/toolchains/rust.bzl: toolchain appends optimized Rust flags "
            "after package overrides"
        )

    overlay_text = overlay.read_text(encoding="utf-8")
    overlay_body = starlark_function(overlay_text, "add_buildscript_metadata_bzl") or ""
    ordered_buildscript_flags = re.search(
        r"rust_toolchain_info\.rustc_flags,\s*"
        r"ctx\.attrs\.cargo_rustc_flags,\s*"
        r"rust_toolchain_info\.extra_rustc_flags,",
        overlay_body,
    )
    scans_extra_flags = (
        "rust_toolchain_info.rustc_flags + ctx.attrs.cargo_rustc_flags + "
        "rust_toolchain_info.extra_rustc_flags"
        in overlay_body
    )
    if not ordered_buildscript_flags or not scans_extra_flags:
        failures.append(
            "tools/buck2/prelude_overlay.py: target profile and package Rust flags "
            "do not reach cargo build-script commands in effective order"
        )


def check_monty_optimized_config(failures: list[str]) -> None:
    path = ROOT / "scripts/profile_monty_comparison.sh"
    if not path.is_file():
        failures.append("scripts/profile_monty_comparison.sh is missing")
        return
    commands = [
        shlex.split(line.strip())
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip().startswith("exec kiln run ")
    ]
    expected = [
        "exec",
        "kiln",
        "run",
        "--config=optimized",
        "//crates/lash-typescript:monty_comparison__example",
        "--",
        "$@",
    ]
    if commands != [expected]:
        failures.append(
            "scripts/profile_monty_comparison.sh: Monty must run through the "
            "Buck2 optimized configuration"
        )


# A launcher that builds a judged host through Buck2 must name the config on
# the same command. Without it the label builds under the ordinary
# configuration, which is the drift this whole gate exists to catch — and it is
# silent, because the binary runs.
BUCK2_BUILD_INVOCATION = re.compile(
    r"(?:\bkiln\s+build\b|hermetic-build\.sh|build_command\[@\]\})"
)
# Either the label itself or the variable this repository holds it in. The
# variable is matched too because the launcher keeps the label in one place,
# and a gate that only understood the literal would read the invocation as
# building nothing.
JUDGED_HOST_LABEL = re.compile(
    r"(?://examples/(?:" + "|".join(JUDGED_HOSTS) + r"):|workbench_buck2_label|build_label)"
)


def check_buck2_boot_sites(failures: list[str]) -> None:
    seen = False
    for path in (
        path for path in boot_files()
        if path.name == "justfile" or path.suffix == ".sh"
    ):
        text = path.read_text(encoding="utf-8")
        rel = path.relative_to(ROOT)
        current: list[str] = []
        start_line = 0
        for line_number, raw in enumerate(text.splitlines(), 1):
            stripped = raw.strip()
            if not current and (not stripped or stripped.startswith("#")):
                continue
            if not current:
                start_line = line_number
            continued = raw.rstrip().endswith("\\")
            current.append(raw.rstrip()[:-1].strip() if continued else stripped)
            if continued:
                continue
            command = " ".join(current)
            current = []
            try:
                words = shlex.split(command)
            except ValueError:
                continue
            control = next(
                (index for index, word in enumerate(words) if word in {"||", "&&", ";"}),
                len(words),
            )
            invocation = " ".join(words[:control])
            if not BUCK2_BUILD_INVOCATION.search(invocation):
                continue
            if not JUDGED_HOST_LABEL.search(invocation):
                continue
            seen = True
            if "--config=judged" in words[:control]:
                continue
            failures.append(
                f"{rel}:{start_line}: builds a judged host through Buck2 without "
                "`--config=judged`; the label would carry the "
                "development self-checks a judged row must not score"
            )
    if not seen:
        failures.append(
            "no Buck2 build of a judged host found; this gate's file list or its "
            "idea of the invocation has gone stale"
        )


# The launcher's own build step, and the two `flock` acquisitions it must
# precede. A build holds no ownership claim, and the data-ownership lock is
# shared by every checkout on the box, so compiling inside it makes one lane's
# cold build the whole machine's boot latency (FIG-3153).
LAUNCHER_LOCK_ORDER_SITE = "scripts/agent-workbench-dev.sh"
LAUNCHER_BUILD_CALL = re.compile(r"^\s*prepare_workbench_binary$", re.MULTILINE)
LAUNCHER_LOCK_ACQUISITION = re.compile(r"^\s*flock -n ", re.MULTILINE)


def check_build_precedes_launcher_locks(failures: list[str]) -> None:
    path = ROOT / LAUNCHER_LOCK_ORDER_SITE
    if not path.is_file():
        failures.append(f"{LAUNCHER_LOCK_ORDER_SITE} is missing; the lock-order check has gone stale")
        return
    text = path.read_text(encoding="utf-8")
    calls = [match.start() for match in LAUNCHER_BUILD_CALL.finditer(text)]
    locks = [match.start() for match in LAUNCHER_LOCK_ACQUISITION.finditer(text)]
    if not calls:
        failures.append(
            f"{LAUNCHER_LOCK_ORDER_SITE}: no `prepare_workbench_binary` call; the "
            "lock-order check has gone stale"
        )
        return
    if not locks:
        failures.append(
            f"{LAUNCHER_LOCK_ORDER_SITE}: no `flock` acquisition; the lock-order check "
            "has gone stale"
        )
        return
    first_lock = min(locks)
    for start in calls:
        if start > first_lock:
            line = text.count("\n", 0, start) + 1
            failures.append(
                f"{LAUNCHER_LOCK_ORDER_SITE}:{line}: builds the host after a launcher "
                "lock is taken; the box-wide data-ownership lock would then hold for "
                "the length of a compile"
            )


def main() -> int:
    failures: list[str] = []
    check_profile(failures)
    check_no_runtime_testing_features(failures)
    check_boot_sites(failures)
    check_artifact_dirs(failures)
    check_profile_overrides_exported(failures)
    check_buck2_judged_config(failures)
    check_buck2_optimized_config(failures)
    check_monty_optimized_config(failures)
    check_buck2_boot_sites(failures)
    check_build_precedes_launcher_locks(failures)
    if failures:
        print("judged build geometry gate: FAILED", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print("judged build geometry gate: clean")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
