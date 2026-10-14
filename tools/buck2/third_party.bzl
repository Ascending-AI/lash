"""Macros consumed by Reindeer-generated third-party Rust targets."""

load("@prelude//rust:cargo_buildscript.bzl", _prelude_buildscript_run = "buildscript_run")
load("@prelude//rust:cargo_package.bzl", "cargo", "get_reindeer_platforms")
load("@prelude//utils:selects.bzl", "selects")
load(":exec_sizes.bzl", "HELPER_BUDGET")
load(":platforms.bzl", "LOCAL_HELPER_CONSTRAINT", "pool_constraint")
load(":profile.bzl", "THIRD_PARTY_OPT_LEVELS")

_DEFAULT_CPU = "1"
_DEFAULT_MEMORY_KB = "524288"
_DEFAULT_CONSTRAINT = "//tools/buck2:pool_1_524288"
_HOST_TRANSITION = "//tools/buck2:host_transition"
_OPTIMIZED_FLAGS = [
    "-Copt-level=3",
    "-Cdebuginfo=0",
    "-Cstrip=debuginfo",
    "-Cembed-bitcode=no",
]

_PROFILING_FLAGS = [
    "-Copt-level=3",
    "-Cdebuginfo=line-tables-only",
    "-Cstrip=none",
    "-Cforce-frame-pointers=yes",
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
        "//tools/buck2:profile_profiling": _PROFILING_FLAGS,
        "DEFAULT": [],
    }) + list(result.get("rustc_flags", []))
    opt_level = THIRD_PARTY_OPT_LEVELS.get(package_name, THIRD_PARTY_OPT_LEVELS.get("*"))
    if opt_level != None:
        flags += select({
            "//tools/buck2:profile_host": [],
            "//tools/buck2:profile_optimized": [],
            "//tools/buck2:profile_profiling": [],
            "DEFAULT": ["-Copt-level={}".format(opt_level)],
        })
    flags += select({
        "//tools/buck2:profile_judged": ["-Cdebug-assertions=no", "-Coverflow-checks=no"],
        "DEFAULT": [],
    })
    result["rustc_flags"] = flags
    return result


def third_party_http_archive(name, **kwargs):
    # Unpacking a crate archive is a tenth of a second of `tar` over a file
    # the daemon has just downloaded. It runs on the invoking host: on the
    # pool it waited for a scheduler slot and a worker's set-up for longer
    # than it ran. The unpacked tree is the same either way, so the compiles
    # that read it keep their action digests.
    native.http_archive(
        name = name,
        exec_compatible_with = [LOCAL_HELPER_CONSTRAINT],
        **kwargs
    )


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
    cpu, memory = HELPER_BUDGET
    helper_kwargs = dict(kwargs)
    helper_kwargs.update({
        "kiln_action_cpu_count": str(cpu),
        "kiln_action_memory_kb": str(memory),
        "exec_compatible_with": kwargs.get("exec_compatible_with", []) + [pool_constraint(cpu, memory)],
    })
    action_env = dict(env)
    action_env.update({
        "KILN_ACTION_CPU_COUNT": str(cpu),
        "KILN_ACTION_MEMORY_KB": str(memory),
    })
    _prelude_buildscript_run(
        name = name,
        package_name = package_name,
        cargo_rustc_flags = _profile_kwargs({"env": {"CARGO_PKG_NAME": package_name}})["rustc_flags"],
        env = action_env,
        **helper_kwargs
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
