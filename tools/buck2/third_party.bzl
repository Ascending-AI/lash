"""Macros consumed by Reindeer-generated third-party Rust targets."""

load("@prelude//rust:cargo_buildscript.bzl", _prelude_buildscript_run = "buildscript_run")
load("@prelude//rust:cargo_package.bzl", "cargo", "get_reindeer_platforms")
load("@prelude//utils:selects.bzl", "selects")
load(":profile.bzl", "THIRD_PARTY_OPT_LEVELS")

_DEFAULT_CPU = "1"
_DEFAULT_MEMORY_KB = "1572864"
_DEFAULT_CONSTRAINT = "//tools/buck2:pool_1_1572864"
_HOST_TRANSITION = "//tools/buck2:host_transition"
_OPTIMIZED_FLAGS = [
    "-Copt-level=3",
    "-Cdebuginfo=0",
    "-Cstrip=debuginfo",
    "-Cembed-bitcode=no",
]

def _resource_kwargs(kwargs):
    result = dict(kwargs)
    result["kiln_action_cpu_count"] = result.get("kiln_action_cpu_count", _DEFAULT_CPU)
    result["kiln_action_memory_kb"] = result.get("kiln_action_memory_kb", _DEFAULT_MEMORY_KB)
    result["exec_compatible_with"] = result.get("exec_compatible_with", []) + [_DEFAULT_CONSTRAINT]
    return result

def _profile_kwargs(kwargs):
    result = dict(kwargs)
    package_name = result.get("env", {}).get("CARGO_PKG_NAME")
    flags = select({
        "//tools/buck2:profile_host": _OPTIMIZED_FLAGS,
        "//tools/buck2:profile_optimized": _OPTIMIZED_FLAGS,
        "DEFAULT": [],
    }) + list(result.get("rustc_flags", []))
    if package_name in THIRD_PARTY_OPT_LEVELS:
        flags += select({
            "//tools/buck2:profile_host": [],
            "DEFAULT": ["-Copt-level={}".format(THIRD_PARTY_OPT_LEVELS[package_name])],
        })
    flags += select({
        "//tools/buck2:profile_judged": ["-Cdebug-assertions=no", "-Coverflow-checks=no"],
        "DEFAULT": [],
    })
    result["rustc_flags"] = flags
    return result


def _target_constraints(platforms, kwargs):
    compatible = kwargs.pop("target_compatible_with", [])
    if kwargs.get("proc_macro", False) or len(platforms) == 0:
        return compatible
    return selects.apply(
        get_reindeer_platforms(),
        lambda platform: compatible if platform in platforms else ["prelude//:none"],
    )


def third_party_rust_library(name, platform = {}, **kwargs):
    compatible = _target_constraints(platform, kwargs)
    if kwargs.get("proc_macro", False):
        kwargs["incoming_transition"] = _HOST_TRANSITION
    cargo.rust_library(
        name = name,
        platform = platform,
        target_compatible_with = compatible,
        **_resource_kwargs(_profile_kwargs(kwargs))
    )


def third_party_rust_binary(name, platform = {}, **kwargs):
    compatible = _target_constraints(platform, kwargs)
    if kwargs.get("env", {}).get("CARGO_CRATE_NAME", "").startswith("build_script"):
        kwargs["incoming_transition"] = _HOST_TRANSITION
    cargo.rust_binary(
        name = name,
        platform = platform,
        target_compatible_with = compatible,
        **_resource_kwargs(_profile_kwargs(kwargs))
    )

def third_party_buildscript_run(name, package_name, env = {}, **kwargs):
    action_env = dict(env)
    action_env.update({
        "KILN_ACTION_CPU_COUNT": _DEFAULT_CPU,
        "KILN_ACTION_MEMORY_KB": _DEFAULT_MEMORY_KB,
    })
    _prelude_buildscript_run(
        name = name,
        package_name = package_name,
        cargo_rustc_flags = _profile_kwargs({"env": {"CARGO_PKG_NAME": package_name}})["rustc_flags"],
        env = action_env,
        **_resource_kwargs(kwargs)
    )


def third_party_rust_cxx_library(name, platform = {}, **kwargs):
    compatible = _target_constraints(platform, kwargs)
    native.cxx_library(name = name, target_compatible_with = compatible, **kwargs)


def third_party_rust_prebuilt_cxx_library(name, platform = {}, **kwargs):
    compatible = _target_constraints(platform, kwargs)
    native.prebuilt_cxx_library(
        name = name,
        target_compatible_with = compatible,
        **kwargs
    )
