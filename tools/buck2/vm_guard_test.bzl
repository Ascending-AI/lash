"""Production VM source guards, run on the same executor as the main suite."""
load(":exec_sizes.bzl", "HELPER_BUDGET")
load(":platforms.bzl", "RE_PRIORITY", "pool_properties")
load(":source_tree.bzl", "WorkspaceSourcesInfo")


def _impl(ctx):
    entries = dict(ctx.attrs.workspace[WorkspaceSourcesInfo].entries)
    entries.update({source.short_path: source for source in ctx.attrs.srcs})
    for manifest in ctx.attrs.manifests:
        entries[manifest.label.package + "/Cargo.toml"] = manifest[DefaultInfo].default_outputs[0]
    sources = ctx.actions.symlinked_dir("sources", entries)
    helpers = ctx.attrs.helpers[DefaultInfo].default_outputs[0]
    cpu = str(HELPER_BUDGET[0])
    memory = str(HELPER_BUDGET[1])
    local = read_root_config("kiln", "execution_mode", "remote") == "local"
    return [
        DefaultInfo(default_output = sources),
        ExternalRunnerTestInfo(
            type = "custom",
            command = [
                cmd_args("/usr/bin/bash", helpers.project("test_launcher.sh"), hidden = helpers),
                "/usr/bin/python3", sources.project("scripts/run-vm-guard.py"),
                "--root", sources, "--guard", ctx.attrs.guard,
                "--lash-libtest-args",
            ],
            env = {
                "PATH": "/usr/bin:/bin",
                "KILN_ACTION_CPU_COUNT": cpu,
                "KILN_ACTION_MEMORY_KB": memory,
                "TEST_BINARY": str(ctx.label.raw_target()),
                "TEST_TARGET": str(ctx.label.raw_target()),
                "PYTHONDONTWRITEBYTECODE": "1",
            },
            labels = ["lash.timeout_seconds=60", "lash.resource_cpu=" + cpu, "lash.resource_memory_kb=" + memory],
            run_from_project_root = True,
            use_project_relative_paths = True,
            supports_test_execution_caching = True,
            default_executor = CommandExecutorConfig(
                local_enabled = local,
                remote_enabled = not local,
                remote_cache_enabled = not local,
                priority = RE_PRIORITY,
                remote_execution_properties = pool_properties(cpu, memory),
                remote_execution_use_case = "lash",
            ),
        ),
    ]

vm_guard_test = rule(
    impl = _impl,
    attrs = {
        "guard": attrs.string(),
        "workspace": attrs.dep(default = "//:workspace_rust_sources", providers = [WorkspaceSourcesInfo]),
        "manifests": attrs.list(attrs.dep(providers = [DefaultInfo])),
        "srcs": attrs.list(attrs.source()),
        "helpers": attrs.dep(default = "//tools/buck2:test_helpers"),
    },
)
