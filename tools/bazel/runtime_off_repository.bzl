"""Isolated runtime OFF source workspace, derived from current Cargo manifests."""

def _runtime_off_sources_impl(rctx):
    root = rctx.path(rctx.attr.manifest).dirname
    rctx.watch(rctx.path(rctx.attr.manifest))
    rctx.watch(rctx.path(rctx.attr.generator))
    rctx.watch(root.get_child("Cargo.lock"))
    rctx.watch(root.get_child("tools/bazel/runtime-off.Cargo.lock"))
    result = rctx.execute([
        "python3",
        rctx.path(rctx.attr.generator),
        "--root",
        root,
        "--output",
        rctx.path("sources"),
    ])
    if result.return_code:
        fail(result.stderr)
    for manifest in json.decode(result.stdout):
        rctx.watch(root.get_child(manifest))
        root.get_child(manifest).dirname.readdir(watch = "yes")
    rctx.file("BUILD.bazel", "exports_files([\"sources/witness/Cargo.toml\", \"sources/witness/Cargo.lock\"])\n")

runtime_off_sources = repository_rule(
    implementation = _runtime_off_sources_impl,
    attrs = {
        "manifest": attr.label(default = "//:Cargo.toml"),
        "generator": attr.label(default = "//tools/bazel:runtime_off_workspace.py"),
    },
    local = True,
)
