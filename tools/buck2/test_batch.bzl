"""One cacheable remote action for a package's plain libtest binaries."""

load(":platforms.bzl", "pool_properties")

def _batch_impl(ctx):
    local = read_root_config("kiln", "execution_mode", "remote") == "local"
    helpers = ctx.attrs.helpers[DefaultInfo].default_outputs[0]
    command = [
        cmd_args("/usr/bin/bash", helpers.project("test_batch_launcher.sh"), hidden = helpers),
        str(len(ctx.attrs.members)),
    ]
    # Each member is `<n> <n env assignments> <binary>`: the native test's own
    # command and environment (see `_external_test_impl`), so its artifact
    # values render project-relative and the runner sees one binary per member.
    for test in ctx.attrs.members:
        native_test = test[ExternalRunnerTestInfo]
        if len(native_test.command) != 1:
            fail("{} must run as a single binary to join a batch".format(test.label))
        command.append(str(len(native_test.env)))
        for name, value in sorted(native_test.env.items()):
            command.append(cmd_args(name, "=", value, delimiter = ""))
        command.extend(native_test.command)
    return [
        DefaultInfo(),
        ExternalRunnerTestInfo(
            type = "custom",
            command = command,
            env = {
                "BUILD_WORKSPACE_DIRECTORY": ".",
                "INSTA_WORKSPACE_ROOT": ".",
                "KILN_ACTION_CPU_COUNT": ctx.attrs.cpu,
                "KILN_ACTION_MEMORY_KB": ctx.attrs.memory_kb,
                "LASH_BATCH_JOBS": str(ctx.attrs.jobs),
                "PATH": "/usr/bin:/bin",
                "TEST_BINARY": str(ctx.label.raw_target()),
                "TEST_SHARD_INDEX": "0",
                "TEST_TARGET": str(ctx.label.raw_target()),
                "TEST_TOTAL_SHARDS": "0",
            },
            labels = [
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
            executor_overrides = {
                "local": CommandExecutorConfig(
                    local_enabled = True,
                    remote_enabled = False,
                    remote_cache_enabled = False,
                ),
            },
        ),
    ]

_batch_test = rule(
    impl = _batch_impl,
    attrs = {
        "cpu": attrs.string(),
        "helpers": attrs.dep(),
        "jobs": attrs.int(),
        "memory_kb": attrs.string(),
        "members": attrs.list(attrs.dep(providers = [ExternalRunnerTestInfo])),
    },
)

def lash_batch_test(name, tests, jobs, budget):
    binaries = [test + "__rust_test" for test in tests]
    _batch_test(
        name = name,
        members = binaries,
        helpers = "//tools/buck2:test_helpers",
        jobs = jobs,
        cpu = str(budget["cpu_count"]),
        memory_kb = str(budget["memory_kb"]),
        visibility = ["PUBLIC"],
    )
