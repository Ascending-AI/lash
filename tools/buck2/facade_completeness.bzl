"""The facade-completeness test (FIG-4373, FIG-4375).

A host depends on the `lash-runtime` facade alone, so every first-party type a
facade-exported signature names must itself be nameable through a `lash::`
path. `scripts/facade_completeness.py` walks rustdoc JSON documents and fails
on every such type no facade path reaches.

rustdoc's JSON backend never inlines a re-export from another crate, so the
facade's document alone does not carry the signatures it re-exports. The test
therefore reads the `doc-json` subtarget (tools/buck2/prelude_overlay.py) of
the facade and of every first-party library in its dependency closure, each
documented against its own resolved dependency graph and features, exactly as
`doc` is.

The generator emits that closure from Cargo's resolve (`[facade]` in
tools/buck2/package-policy.toml); analysis checks it against the facade's
actual Rust link graph, so a dependency change that `kiln sync` has not
regenerated fails here rather than leaving a crate unchecked.
"""

load("@prelude//decls:toolchains_common.bzl", "toolchains_common")
load("@prelude//linking:link_info.bzl", "LinkStrategy")
load("@prelude//rust:build_params.bzl", "MetadataKind")
load("@prelude//rust:link_info.bzl", "RustLinkInfo", "strategy_info")
load("@prelude//rust:rust_toolchain.bzl", "RustToolchainInfo")
load(":platforms.bzl", "pool_properties")

_THIRD_PARTY_PACKAGE = "third-party/rust"

def _first_party_closure(ctx):
    toolchain = ctx.attrs._rust_toolchain[RustToolchainInfo]
    strategy = strategy_info(toolchain, ctx.attrs.facade[RustLinkInfo], LinkStrategy("static_pic"))
    closure = {}
    for dep in strategy.transitive_deps[MetadataKind("link")].traverse():
        owner = dep.artifact.owner
        if owner == None:
            fail("facade dependency artifact has no owning target: {}".format(dep.artifact))
        if owner.package == _THIRD_PARTY_PACKAGE:
            continue
        closure[str(owner.raw_target())] = None
    return closure

def _document(library):
    sub_targets = library[DefaultInfo].sub_targets
    if "doc-json" not in sub_targets:
        fail("{} has no doc-json subtarget; is the prelude overlay applied?".format(library.label))
    return sub_targets["doc-json"][DefaultInfo].default_outputs[0]

def _facade_completeness_impl(ctx):
    closure = _first_party_closure(ctx)
    declared = {str(library.label.raw_target()): None for library in ctx.attrs.libraries}
    missing = [label for label in closure if label not in declared]
    extra = [label for label in declared if label not in closure]
    if missing or extra:
        fail(
            "facade_completeness libraries differ from the facade's first-party " +
            "dependency closure; run `kiln sync`. Missing: {}. Not in the closure: {}.".format(
                sorted(missing),
                sorted(extra),
            ),
        )
    documents = [_document(ctx.attrs.facade)] + [_document(library) for library in ctx.attrs.libraries]
    helpers = ctx.attrs.helpers[DefaultInfo].default_outputs[0]
    local = read_root_config("kiln", "execution_mode", "remote") == "local"
    return [
        DefaultInfo(default_outputs = documents),
        ExternalRunnerTestInfo(
            type = "custom",
            command = [
                cmd_args("/usr/bin/bash", helpers.project("test_launcher.sh"), hidden = helpers),
                "/usr/bin/python3",
                ctx.attrs.checker,
                "--facade",
                ctx.attrs.facade[RustLinkInfo].crate.simple,
                cmd_args(documents),
            ],
            env = {
                "PATH": "/usr/bin:/bin",
                "TEST_BINARY": str(ctx.label.raw_target()),
                "TEST_TARGET": str(ctx.label.raw_target()),
                "KILN_ACTION_CPU_COUNT": ctx.attrs.cpu,
                "KILN_ACTION_MEMORY_KB": ctx.attrs.memory_kb,
            },
            labels = ctx.attrs.tags + [
                "lash.timeout_seconds=300",
                "lash.resource_cpu=" + ctx.attrs.cpu,
                "lash.resource_memory_kb=" + ctx.attrs.memory_kb,
            ],
            run_from_project_root = True,
            use_project_relative_paths = True,
            supports_test_execution_caching = True,
            default_executor = CommandExecutorConfig(
                local_enabled = local,
                remote_enabled = not local,
                remote_cache_enabled = not local,
                remote_execution_properties = pool_properties(ctx.attrs.cpu, ctx.attrs.memory_kb),
                remote_execution_use_case = "lash",
            ),
            executor_overrides = {"local": CommandExecutorConfig(local_enabled = True, remote_enabled = False, remote_cache_enabled = False)},
        ),
    ]

_facade_completeness = rule(
    impl = _facade_completeness_impl,
    attrs = {
        "checker": attrs.source(),
        "cpu": attrs.string(),
        "facade": attrs.dep(providers = [RustLinkInfo]),
        "helpers": attrs.dep(default = "//tools/buck2:test_helpers"),
        "libraries": attrs.list(attrs.dep(providers = [RustLinkInfo])),
        "memory_kb": attrs.string(),
        "tags": attrs.list(attrs.string()),
        "_rust_toolchain": toolchains_common.rust(),
    },
)

def facade_completeness_test(name, facade, libraries, tags = [], exec_properties = {}):
    _facade_completeness(
        name = name,
        checker = "//:scripts/facade_completeness.py",
        facade = facade,
        libraries = libraries,
        tags = tags,
        cpu = exec_properties.get("cpu_count", "1"),
        memory_kb = exec_properties.get("memory_kb", "1572864"),
        visibility = ["PUBLIC"],
    )
