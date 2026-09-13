"""Clippy actions for first-party Lash crates.

`rules_rust` ships `rust_clippy` and `rust_clippy_aspect`, but its aspect binds
one `clippy.toml` for the whole build. Clippy resolves its configuration by
walking up from each crate's manifest directory and stopping at the first
`clippy.toml` it finds, and this repository has two: the workspace file and
`crates/lash-core/clippy.toml`, which carries the `disallowed-methods` list the
workspace lint table denies. A single global config would leave that list empty
for `lash-core` and silently pass `clippy::disallowed_methods` there, so the
aspect below selects the nearest declared config per target exactly as Cargo
does. Everything else is the upstream action.

The upstream aspect also drops its `-D warnings` default as soon as a target
carries a `lint_config`, which every generated Lash target does. CI's Cargo
command is `cargo clippy ... -- -D warnings`, where the lint-table flags are
emitted first and `-D warnings` last, so the same flag is appended here in the
same position.
"""

load("@rules_rust//rust:defs.bzl", "rust_clippy_action", "rust_common")

# Cargo appends the `-- -D warnings` arguments after the lint-table flags, and
# `rust_clippy_action` appends `extra_clippy_flags` after them too, so a warning
# the workspace table left at `warn` is denied in both tools identically.
_DENY_WARNINGS = "-Dwarnings"

def _nearest_config(package, configs):
    """Picks the declared `clippy.toml` clippy itself would resolve.

    Args:
        package: The Bazel package of the target being linted.
        configs: Declared `clippy.toml` files, as `File`s.

    Returns:
        File: The config whose directory is the longest prefix of `package`.
    """
    best = None
    for config in configs:
        directory = config.dirname
        if directory == ".":
            directory = ""
        if directory and not (package == directory or package.startswith(directory + "/")):
            continue
        if best == None or len(directory) > len(best[0]):
            best = (directory, config)
    if best == None:
        fail("no clippy.toml governs package '{}'".format(package))
    return best[1]

def _lash_clippy_aspect_impl(target, ctx):
    if OutputGroupInfo in target and hasattr(target[OutputGroupInfo], "clippy_checks"):
        return []

    crate_info = rust_clippy_action.get_clippy_ready_crate_info(target, ctx)
    if not crate_info:
        return []

    marker = ctx.actions.declare_file(
        ctx.label.name + ".lash-clippy.ok",
        sibling = crate_info.output,
    )
    configs = []
    for config in ctx.attr._configs:
        configs.extend(config.files.to_list())

    rust_clippy_action.action(
        ctx = ctx,
        clippy_executable = ctx.toolchains[str(Label("@rules_rust//rust:toolchain_type"))].clippy_driver,
        crate_info = crate_info,
        config = _nearest_config(target.label.package, configs),
        success_marker = marker,
        extra_clippy_flags = [_DENY_WARNINGS],
    )

    return [OutputGroupInfo(clippy_checks = depset([marker]))]

lash_clippy_aspect = aspect(
    implementation = _lash_clippy_aspect_impl,
    fragments = ["cpp"],
    attrs = {
        "_configs": attr.label_list(
            doc = "Every `clippy.toml` in the repository, resolved per target.",
            allow_files = True,
            default = [
                Label("//:clippy.toml"),
                Label("//crates/lash-core:clippy.toml"),
            ],
        ),
        # The remaining attributes mirror `rust_clippy_aspect` so that
        # `rust_clippy_action` sees the same build settings it does upstream.
        "_capture_output": attr.label(
            default = Label("@rules_rust//rust/settings:capture_clippy_output"),
        ),
        "_clippy_error_format": attr.label(
            default = Label("@rules_rust//rust/settings:clippy_error_format"),
        ),
        "_clippy_flag": attr.label(
            default = Label("@rules_rust//rust/settings:clippy_flag"),
        ),
        "_clippy_flags": attr.label(
            default = Label("@rules_rust//rust/settings:clippy_flags"),
        ),
        "_clippy_output_diagnostics": attr.label(
            default = Label("@rules_rust//rust/settings:clippy_output_diagnostics"),
        ),
        "_error_format": attr.label(
            default = Label("@rules_rust//rust/settings:error_format"),
        ),
        "_extra_rustc_flag": attr.label(
            default = Label("@rules_rust//rust/settings:extra_rustc_flag"),
        ),
        "_incompatible_change_clippy_error_format": attr.label(
            default = Label("@rules_rust//rust/settings:incompatible_change_clippy_error_format"),
        ),
        "_per_crate_rustc_flag": attr.label(
            default = Label("@rules_rust//rust/settings:per_crate_rustc_flag"),
        ),
    },
    required_providers = [
        [rust_common.crate_info],
        [rust_common.test_crate_info],
    ],
    toolchains = [
        str(Label("@rules_rust//rust:toolchain_type")),
        config_common.toolchain_type(
            "@bazel_tools//tools/cpp:toolchain_type",
            mandatory = False,
        ),
    ],
)

def _lash_rust_clippy_impl(ctx):
    return [DefaultInfo(files = depset(transitive = [
        dep[OutputGroupInfo].clippy_checks
        for dep in ctx.attr.deps
        if "clippy_checks" in dir(dep[OutputGroupInfo])
    ]))]

lash_rust_clippy = rule(
    doc = "Runs clippy over the listed first-party crate targets.",
    implementation = _lash_rust_clippy_impl,
    attrs = {
        "deps": attr.label_list(
            doc = "First-party Rust targets to lint.",
            providers = [
                [rust_common.crate_info],
                [rust_common.test_crate_info],
            ],
            aspects = [lash_clippy_aspect],
        ),
    },
)
