"""Declared Cargo package layouts for first-party build-script actions."""

load("@prelude//:paths.bzl", "paths")

BuildscriptSourcesInfo = provider(fields = {"package": str, "sources": list})

def _buildscript_sources_impl(ctx):
    return [
        DefaultInfo(),
        BuildscriptSourcesInfo(package = ctx.label.package, sources = ctx.attrs.srcs),
    ]

buildscript_sources = rule(
    impl = _buildscript_sources_impl,
    attrs = {"srcs": attrs.list(attrs.source())},
)

def _insert(entries, path, artifact):
    if path in entries and entries[path] != artifact:
        fail("build-script manifest path has multiple owners: {}".format(path))
    entries[path] = artifact

def _buildscript_manifest_impl(ctx):
    entries = {}
    for source in ctx.attrs.package_srcs:
        _insert(entries, source.short_path, source)
    for dependency in ctx.attrs.workspace_srcs:
        if BuildscriptSourcesInfo in dependency:
            source_info = dependency[BuildscriptSourcesInfo]
            package = source_info.package
            sources = source_info.sources
        else:
            package = dependency.label.package
            sources = dependency[DefaultInfo].default_outputs
        for source in sources:
            _insert(
                entries,
                paths.join(".lash-workspace", package, source.short_path),
                source,
            )
    if not entries:
        fail("build-script manifest must declare at least one package input")
    tree = ctx.actions.symlinked_dir(
        "manifest",
        entries,
        has_content_based_path = True,
    )
    return [DefaultInfo(default_output = tree)]

buildscript_manifest = rule(
    impl = _buildscript_manifest_impl,
    attrs = {
        "package_srcs": attrs.list(attrs.source()),
        "workspace_srcs": attrs.list(attrs.dep(), default = []),
    },
)
