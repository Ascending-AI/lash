"""Preserve Cargo's repository-relative source layout for Rust compilation."""

load("@prelude//:artifacts.bzl", "ArtifactGroupInfo")
load("@prelude//rust:sources.bzl", "RustSources", "RustSourcesTSet")
load("@prelude//:paths.bzl", "paths")

WorkspaceSourcesInfo = provider(fields = {"entries": dict})


def _add(mapped, destination, source):
    if destination in mapped and mapped[destination] != source:
        fail("duplicate Rust source-tree destination {}".format(destination))
    mapped[destination] = source


def _dependency_entries(dependency):
    if WorkspaceSourcesInfo in dependency:
        return dependency[WorkspaceSourcesInfo].entries
    package = dependency.label.package
    sources = dependency[ArtifactGroupInfo].artifacts if ArtifactGroupInfo in dependency else dependency[DefaultInfo].default_outputs
    return {
        paths.join(package, source.short_path): source
        for source in sources
    }


def _source_tree_impl(ctx):
    mapped = {}
    for source in ctx.attrs.package_srcs:
        _add(mapped, paths.join(ctx.attrs.package, source.short_path), source)
    for dependency in ctx.attrs.workspace_srcs:
        for destination, source in _dependency_entries(dependency).items():
            _add(mapped, destination, source)

    tree = ctx.actions.symlinked_dir(
        "workspace",
        mapped,
        has_content_based_path = True,
    )
    sources = ctx.actions.tset(
        RustSourcesTSet,
        value = tree,
        children = [],
    )
    return [
        DefaultInfo(default_output = tree),
        RustSources(tset = sources),
    ]


_source_tree = rule(
    impl = _source_tree_impl,
    attrs = {
        "package": attrs.string(),
        "package_srcs": attrs.list(attrs.source()),
        "workspace_srcs": attrs.list(attrs.dep()),
    },
)


def _workspace_sources_impl(ctx):
    entries = {}
    for dependency in ctx.attrs.deps:
        for destination, source in _dependency_entries(dependency).items():
            _add(entries, destination, source)
    tree = ctx.actions.symlinked_dir(
        "workspace_rust_sources",
        entries,
        has_content_based_path = True,
    )
    return [
        DefaultInfo(default_output = tree),
        ArtifactGroupInfo(artifacts = entries.values()),
        WorkspaceSourcesInfo(entries = entries),
    ]


_workspace_sources = rule(
    impl = _workspace_sources_impl,
    attrs = {"deps": attrs.list(attrs.dep())},
)


def lash_rust_source_tree(name, package, package_srcs, workspace_srcs):
    _source_tree(
        name = name,
        package = package,
        package_srcs = package_srcs,
        workspace_srcs = workspace_srcs,
        visibility = [],
    )


def lash_workspace_sources(name, deps, visibility):
    _workspace_sources(
        name = name,
        deps = deps,
        visibility = visibility,
    )
