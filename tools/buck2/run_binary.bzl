"""Rust binary runfiles and caller-overridable runtime environment."""

load("@prelude//:rules.bzl", "clone_rule")
load("@prelude//rust:rust_binary.bzl", "rust_binary_impl")

def _run_binary_impl(ctx):
    providers = rust_binary_impl(ctx)
    binary = [value for value in providers if isinstance(value, RunInfo)][0]
    output = [value for value in providers if isinstance(value, DefaultInfo)][0]
    args = cmd_args("/usr/bin/bash", ctx.attrs._launcher)
    for key, value in ctx.attrs.run_env.items():
        args.add(key, value)
    for key in ctx.attrs.self_env:
        args.add(key, output.default_outputs[0])
    args.add("--", binary)
    for data in ctx.attrs.extra_data:
        info = data[DefaultInfo]
        args.add(cmd_args(hidden = info.default_outputs + info.other_outputs))
    return [value for value in providers if not isinstance(value, RunInfo)] + [RunInfo(args = args)]

lash_run_binary = clone_rule(
    "rust_binary",
    impl_override = _run_binary_impl,
    extra_attrs = {
        "extra_data": attrs.list(attrs.dep(), default = []),
        "run_env": attrs.dict(attrs.string(), attrs.arg(), default = {}),
        "self_env": attrs.list(attrs.string(), default = []),
        "_launcher": attrs.default_only(attrs.source(default = "//tools/buck2:run_binary.sh")),
    },
)

def binary_run_attrs(name, extra_data, run_env):
    if not extra_data and not run_env:
        return {}
    own_labels = [":" + name, "//" + native.package_name() + ":" + name]
    own_locations = ["$(location {})".format(label) for label in own_labels]
    # A worker whose client closure reaches the spawner uses its own output.
    # Resolve that artifact during analysis instead of creating a self edge.
    return {
        "extra_data": [label for label in extra_data if label not in own_labels],
        "run_env": {key: value for key, value in run_env.items() if value not in own_locations},
        "self_env": [key for key, value in run_env.items() if value in own_locations],
    }
