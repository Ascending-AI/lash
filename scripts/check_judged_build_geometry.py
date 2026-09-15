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
builds its host through Bazel so every checkout on the box shares one action
cache instead of compiling the workspace again in its own target directory
(FIG-3153), and Bazel does not read `[profile.judged]`. `--config=judged` in
`.bazelrc` says the same two things in rustc flags, and nothing in rustc's own
defaults would: `-C debug-assertions` defaults to ON at `-C opt-level=0`, which
is exactly the geometry a judged row must not score. So this gate reads both
spellings out of their own files and refuses them to drift, and it refuses a
launcher that builds a judged host through Bazel without naming the config.

It also pins where the build happens. The workbench launcher used to compile
inside `/tmp/lash-agent-workbench-$UID/data-ownership.lock`, a lock every
checkout on the box shares, so one lane's cold build stalled every other
stack's boot and teardown. A build proves nothing about ownership; it belongs
outside the locks, and `check_build_precedes_launcher_locks` keeps it there.

Known limit, accepted rather than fixed: the `testing`-feature check reads the
`[dependencies]`/`[build-dependencies]` tables only, so it is blind to routes
through a host's own feature table. `examples/slack-clone`'s `e2e` feature
forwards to `lash/testing`, and `scripts/slack-clone-dev.sh` turns it on for the
`scripted-v1` provider — an operator who combines that flag with the judged
profile by hand gets a judged-profile host that still carries `testing`. Closing
it means resolving each host's feature graph transitively, which is `cargo
tree`'s job, not a text gate's; the scripted gate that uses `e2e` sets the `dev`
profile, so no judged row reaches the combination today.
"""

from __future__ import annotations

import pathlib
import re
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]

# Examples a judged runbook boots as a host. Kept explicit rather than derived
# from the runbook tree: adding a judged host is a deliberate act, and the
# reviewer should see the geometry claim land in this list.
JUDGED_HOSTS = (
    "agent-service",
    "agent-workbench",
    "slack-clone",
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


# A profile override belongs to a whole gate run, not to one command. The
# scripted slack-clone gate boots the host once and then restarts it from its
# Python driver; a `VAR=value cmd` prefix reaches only the first of those, and
# the second silently rebuilds under the other profile mid-gate.
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


# The Bazel spelling of the judged profile. `[profile.judged]` says
# `debug-assertions = false` / `overflow-checks = false`; these are the rustc
# flags that say it to Bazel, and the mapping is written here so a profile key
# renamed in Cargo.toml cannot leave the Bazel config behind.
BAZEL_JUDGED_CONFIG = "judged"
BAZEL_PROFILE_FLAGS = {
    "debug-assertions": "-Cdebug-assertions=off",
    "overflow-checks": "-Coverflow-checks=off",
}

BAZEL_CONFIG_LINE = re.compile(
    rf"^build:{BAZEL_JUDGED_CONFIG}\s+(\S+)\s*$", re.MULTILINE
)


def check_bazel_judged_config(failures: list[str]) -> None:
    bazelrc = ROOT / ".bazelrc"
    if not bazelrc.is_file():
        failures.append(".bazelrc is missing; the Bazel judged config has no home")
        return
    flags = set(BAZEL_CONFIG_LINE.findall(bazelrc.read_text(encoding="utf-8")))
    for key, expected_off in PROFILE_REQUIREMENTS.items():
        flag = BAZEL_PROFILE_FLAGS.get(key)
        if flag is None:
            continue
        if expected_off is not False:
            continue
        if not any(entry.endswith("=" + flag) or entry == flag for entry in flags):
            failures.append(
                f".bazelrc: --config={BAZEL_JUDGED_CONFIG} never passes `{flag}`, so a "
                f"Bazel-built judged host keeps the `{key}` that "
                "[profile.judged] turns off (rustc defaults them ON at -C opt-level=0)"
            )


# A launcher that builds a judged host through Bazel must name the config on
# the same command. Without it the label builds under the ordinary
# configuration, which is the drift this whole gate exists to catch — and it is
# silent, because the binary runs.
BAZEL_BUILD_INVOCATION = re.compile(
    r"^[^\n#]*(?:kiln build|hermetic-build\.sh|build_command\[@\]\})[^\n]*$",
    re.MULTILINE,
)
# Either the label itself or the variable this repository holds it in. The
# variable is matched too because the launcher keeps the label in one place,
# and a gate that only understood the literal would read the invocation as
# building nothing.
JUDGED_HOST_LABEL = re.compile(
    r"(?://examples/(?:" + "|".join(JUDGED_HOSTS) + r"):|workbench_bazel_label)"
)


def check_bazel_boot_sites(failures: list[str]) -> None:
    seen = False
    for path in boot_files():
        text = path.read_text(encoding="utf-8")
        rel = path.relative_to(ROOT)
        for match in BAZEL_BUILD_INVOCATION.finditer(text):
            line_text = match.group(0)
            if not JUDGED_HOST_LABEL.search(line_text):
                continue
            seen = True
            if f"--config={BAZEL_JUDGED_CONFIG}" in line_text:
                continue
            line = text.count("\n", 0, match.start()) + 1
            failures.append(
                f"{rel}:{line}: builds a judged host through Bazel without "
                f"`--config={BAZEL_JUDGED_CONFIG}`; the label would carry the "
                "development self-checks a judged row must not score"
            )
    if not seen:
        failures.append(
            "no Bazel build of a judged host found; this gate's file list or its "
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
    check_bazel_judged_config(failures)
    check_bazel_boot_sites(failures)
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
