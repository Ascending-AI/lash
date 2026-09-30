"""Direct-rustc UI contracts using the harness's exact Rust dependency graph."""

load("@prelude//decls:toolchains_common.bzl", "toolchains_common")
load("@prelude//linking:link_info.bzl", "LinkStrategy")
load("@prelude//rust:build_params.bzl", "MetadataKind")
load("@prelude//rust:link_info.bzl", "RustLinkInfo", "RustProcMacroMarker", "RustProcMacroPlugin", "get_available_proc_macros", "strategy_info")
load("@prelude//rust:rust_toolchain.bzl", "RustToolchainInfo")
load("@prelude//rust:sources.bzl", "RustSources")
load(":platforms.bzl", "pool_properties")

UIHarnessInfo = provider(fields = {
    "edition": str,
    "externs": dict,
    "features": list,
    "harness_outputs": list,
    "libraries": list,
    "sources": list,
})

def _fixture_harness_impl(ctx):
    toolchain = ctx.attrs._rust_toolchain[RustToolchainInfo]
    available = get_available_proc_macros(ctx)
    externs = {}
    libraries = {}
    sources = {}
    for alias, declared in ctx.attrs.named_deps.items():
        marker = declared.get(RustProcMacroMarker)
        dep = available[marker.label] if marker else declared
        if RustLinkInfo not in dep:
            fail("UI fixture named dependency lacks RustLinkInfo: {}".format(dep.label))
        strategy = strategy_info(toolchain, dep[RustLinkInfo], LinkStrategy("static_pic"))
        externs[alias] = strategy.outputs[MetadataKind("link")]
        libraries[externs[alias]] = None
        for transitive in strategy.transitive_deps[MetadataKind("link")].traverse():
            if transitive.crate.dynamic != None:
                fail("UI fixtures require statically named dependency artifacts")
            libraries[transitive.artifact] = None
        for proc_marker in strategy.transitive_proc_macro_deps:
            proc = available[proc_marker.label]
            proc_strategy = strategy_info(toolchain, proc[RustLinkInfo], LinkStrategy("static_pic"))
            libraries[proc_strategy.outputs[MetadataKind("link")]] = None
            if RustSources in proc:
                for source in proc[RustSources].tset.traverse():
                    sources[source] = None
        if RustSources in dep:
            for source in dep[RustSources].tset.traverse():
                sources[source] = None
    harness_outputs = ctx.attrs.harness[DefaultInfo].default_outputs
    if not harness_outputs:
        fail("UI harness must name the native rust_test binary, not its external wrapper")
    return [
        DefaultInfo(),
        UIHarnessInfo(
            edition = ctx.attrs.edition,
            externs = externs,
            features = ctx.attrs.features,
            harness_outputs = harness_outputs,
            libraries = libraries.keys(),
            sources = [{"root": source, "package": source.owner.package} for source in sources],
        ),
    ]

ui_fixture_harness = rule(
    impl = _fixture_harness_impl,
    attrs = {
        "edition": attrs.string(default = "2024"),
        "features": attrs.list(attrs.string(), default = []),
        "harness": attrs.dep(),
        "named_deps": attrs.dict(attrs.string(), attrs.dep(pulls_and_pushes_plugins = [RustProcMacroPlugin])),
        "_rust_toolchain": toolchains_common.rust(),
    },
    uses_plugins = [RustProcMacroPlugin],
)

def _ui_fixtures_impl(ctx):
    harness = ctx.attrs.harness[UIHarnessInfo]
    toolchain = ctx.attrs._rust_toolchain[RustToolchainInfo]
    if toolchain.sysroot_path == None:
        fail("The UI gate requires a declared Rust toolchain sysroot")
    expected = {file.basename.removesuffix(".stderr"): file for file in ctx.attrs.expected}
    fixtures = []
    names = {}
    for source in ctx.attrs.fixtures:
        name = source.basename.removesuffix(".rs")
        if name in names or name not in expected:
            fail("UI fixture needs a unique source and stderr pin: {}".format(name))
        names[name] = None
        fixtures.append({"name": name, "source": source, "expected": expected[name]})
    if not fixtures or len(fixtures) != len(expected):
        fail("UI fixture sources and stderr pins must be a nonempty one-to-one set")
    manifest = ctx.actions.write_json("ui-fixtures.json", {
        "schema": 1,
        "compiler": cmd_args(toolchain.compiler),
        "sysroot": toolchain.sysroot_path,
        "edition": ctx.attrs.edition,
        "features": harness.features,
        "externs": harness.externs,
        "libraries": harness.libraries,
        "sources": harness.sources,
        "package": ctx.attrs.package,
        "fixtures": fixtures,
    }, with_inputs = True)
    helpers = ctx.attrs.helpers[DefaultInfo].default_outputs[0]
    local = read_root_config("kiln", "execution_mode", "remote") == "local"
    return [
        DefaultInfo(default_outputs = harness.harness_outputs),
        ExternalRunnerTestInfo(
            type = "custom",
            command = [
                cmd_args("/usr/bin/bash", helpers.project("test_launcher.sh"), hidden = [helpers, harness.harness_outputs]),
                "/usr/bin/python3",
                ctx.attrs.runner,
                "--manifest",
                manifest,
            ],
            env = {
                "BUILD_WORKSPACE_DIRECTORY": ".",
                "INSTA_WORKSPACE_ROOT": ".",
                "KILN_ACTION_CPU_COUNT": ctx.attrs.cpu,
                "KILN_ACTION_MEMORY_KB": ctx.attrs.memory_kb,
                "PATH": "/usr/bin:/bin",
                "TEST_BINARY": str(ctx.label.raw_target()),
                "TEST_TARGET": str(ctx.label.raw_target()),
            },
            labels = ctx.attrs.tags + ["lash.timeout_seconds=300", "lash.resource_cpu=" + ctx.attrs.cpu, "lash.resource_memory_kb=" + ctx.attrs.memory_kb],
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

_ui_fixtures = rule(
    impl = _ui_fixtures_impl,
    attrs = {
        "cpu": attrs.string(),
        "edition": attrs.string(),
        "expected": attrs.list(attrs.source()),
        "fixtures": attrs.list(attrs.source()),
        "harness": attrs.dep(providers = [UIHarnessInfo]),
        "helpers": attrs.dep(default = "//tools/buck2:test_helpers"),
        "memory_kb": attrs.string(),
        "package": attrs.string(),
        "runner": attrs.source(default = "//tools/buck2:ui_fixtures_runner.py"),
        "tags": attrs.list(attrs.string()),
        "_rust_toolchain": toolchains_common.rust(),
    },
)

def ui_fixtures_test(name, harness, package, fixtures, expected, edition = "2024", tags = [], exec_properties = {}):
    # These defaults preserve the former gate's pool request. The measured
    # harness compiler retains its own independent resource row.
    _ui_fixtures(
        name = name,
        harness = harness + "__ui_inputs",
        package = package,
        edition = edition,
        fixtures = fixtures,
        expected = expected,
        tags = tags,
        cpu = exec_properties.get("cpu_count", "1"),
        memory_kb = exec_properties.get("memory_kb", "1572864"),
        visibility = ["PUBLIC"],
    )
