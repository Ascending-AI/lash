"""Direct-rustc compile-fail UI fixtures for the seal lane (FIG-3364).

`tests/ui/*.stderr` pins are compile-fail contracts: each fixture must fail
with exactly the pinned diagnostic. trybuild checks them by driving a nested
`cargo check` of the whole dependency graph under `--cfg trybuild` -- a second
copy of the work Bazel already did, and one the remote cache can never serve
because the nested fingerprints differ from both the outer build and the
Bazel actions.

This rule instead compiles each fixture with the toolchain's `rustc` directly,
against the same rlib closure `ui__test` links: `--extern` for every crate the
harness sees (`CrateInfo.deps` + `proc_macro_deps`), `-L` over
`DepInfo.transitive_crate_outputs`, and the toolchain sysroot. Output is
normalized to trybuild's spelling (cargo's `error: aborting`/`--explain`
trailer dropped, execroot-relative paths mapped back to `src/` and
`$WORKSPACE/crates/`) and diffed against the pin, so one `.stderr` file serves
both runners: the trusted lane runs this test, untrusted runs keep
`cargo test --test ui`.
"""

# buildifier: disable=bzl-visibility
load("@rules_rust//rust/private:providers.bzl", "CrateInfo", "DepInfo")

def _rloc(file):
    """Path of a file inside a test's runfiles tree."""
    short = file.short_path
    if short.startswith("../"):
        return short[len("../"):]
    return "_main/" + short

def _dir(rloc):
    return rloc[:rloc.rindex("/")]

def _ui_fixtures_test_impl(ctx):
    toolchain = ctx.toolchains["@rules_rust//rust:toolchain_type"]
    crate_info = ctx.attr.harness[CrateInfo]
    dep_info = ctx.attr.harness[DepInfo]

    # `--extern` for every crate the harness itself can name. `crate_info.deps`
    # carries the ordinary dependencies and `proc_macro_deps` the macro crates
    # (whose outputs are .so); both spell the same flag. Duplicate crate names
    # keep their first output, matching rustc's first-`--extern`-wins rule.
    externs = {}
    for dep in depset(
        transitive = [crate_info.deps, crate_info.proc_macro_deps],
    ).to_list():
        info = dep.crate_info
        if info == None or info.output == None or info.name in externs:
            continue
        externs[info.name] = info.output

    dep_files = depset(transitive = [
        dep_info.transitive_crate_outputs,
        dep_info.transitive_metadata_outputs,
    ]).to_list()

    # Diagnostics reach into dependency sources to render snippets (the pins
    # carry them, e.g. `| pub(crate) fn with_generation...`), so every source
    # file of every crate in the closure is a runfiles input.
    dep_srcs = depset(transitive = [
        info.srcs
        for info in dep_info.transitive_crates.to_list()
    ])

    std_files = toolchain.rust_std.to_list()
    sysroot = None
    for f in std_files:
        rloc = _rloc(f)
        marker = "/lib/rustlib/"
        if marker in rloc:
            sysroot = rloc[:rloc.index(marker)]
            break
    if sysroot == None:
        fail("toolchain rust_std contains no lib/rustlib path: " +
             str([f.short_path for f in std_files[:5]]))

    lib_dirs = depset(
        [_dir(_rloc(f)) for f in dep_files + std_files],
    ).to_list()

    rustc_lib_files = (
        toolchain.rustc_lib.to_list() if toolchain.rustc_lib != None else []
    )

    lines = [
        "rustc=" + _rloc(toolchain.rustc),
        "sysroot=" + sysroot,
        "edition=" + ctx.attr.edition,
    ]
    for d in sorted(depset([_dir(_rloc(f)) for f in rustc_lib_files]).to_list()):
        lines.append("rustc_lib_dir=" + d)
    for d in sorted(lib_dirs):
        lines.append("lib_dir=" + d)
    for name in sorted(externs):
        lines.append("extern=" + name + "=" + _rloc(externs[name]))
    for f in ctx.files.fixtures:
        stem = f.basename[:-len(".rs")]
        lines.append("fixture=" + stem)

    manifest = ctx.actions.declare_file(ctx.label.name + ".manifest")
    ctx.actions.write(manifest, "\n".join(lines) + "\n")

    script = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(script, """#!/usr/bin/env bash
set -euo pipefail
export UI_MANIFEST="$TEST_SRCDIR/{manifest}"
export UI_PACKAGE={package}
exec bash "$TEST_SRCDIR/_main/tools/bazel/ui_fixtures_runner.sh" "$@"
""".format(manifest = _rloc(manifest), package = ctx.attr.package))

    runfiles = ctx.runfiles(
        files = [manifest, ctx.file._runner, toolchain.rustc] +
                ctx.files.fixtures +
                ctx.files.expected +
                externs.values() +
                std_files +
                rustc_lib_files,
        transitive_files = depset(transitive = [
            dep_info.transitive_crate_outputs,
            dep_info.transitive_metadata_outputs,
            dep_info.transitive_proc_macro_data,
            dep_srcs,
        ]),
    )
    return [DefaultInfo(executable = script, runfiles = runfiles)]

ui_fixtures_test = rule(
    implementation = _ui_fixtures_test_impl,
    doc = "Runs every compile-fail UI fixture through the toolchain rustc " +
          "against the harness's rlib closure and diffs normalized stderr " +
          "against the .stderr pins (FIG-3364).",
    attrs = {
        "harness": attr.label(
            doc = "The rust_test the fixtures share a dependency graph with " +
                  "(//crates/lash:ui__test). Its CrateInfo/DepInfo supply the " +
                  "--extern set and -L closure.",
            mandatory = True,
            providers = [CrateInfo, DepInfo],
        ),
        "fixtures": attr.label_list(
            doc = "The tests/ui/*.rs compile-fail sources.",
            allow_files = [".rs"],
            mandatory = True,
        ),
        "expected": attr.label_list(
            doc = "The tests/ui/*.stderr pins, one per fixture.",
            allow_files = [".stderr"],
            mandatory = True,
        ),
        "package": attr.string(
            doc = "Package directory the fixtures compile under, e.g. crates/lash.",
            mandatory = True,
        ),
        "edition": attr.string(default = "2024"),
        "_runner": attr.label(
            default = "//tools/bazel:ui_fixtures_runner.sh",
            allow_single_file = True,
        ),
    },
    toolchains = ["@rules_rust//rust:toolchain_type"],
    test = True,
)
