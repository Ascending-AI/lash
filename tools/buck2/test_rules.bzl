"""Cacheable Lash test wrappers with reports and exact run budgets."""

load(":platforms.bzl", "RE_PRIORITY", "pool_properties")
load(":exec_sizes.bzl", "TEST_RUN_REQUESTS")

def _bundle_impl(ctx):
    directory = ctx.actions.copied_dir("helpers", {
        "junit_xml.py": ctx.attrs.junit,
        "libtest_selection.py": ctx.attrs.libtest_selection,
        "postgres_slot_runner.sh": ctx.attrs.postgres,
        "test_batch_runner.sh": ctx.attrs.batch,
        "test_batch_launcher.sh": ctx.attrs.batch_launcher,
        "test_launcher.sh": ctx.attrs.launcher,
        "test_shard.py": ctx.attrs.shard,
        "test_timeout.py": ctx.attrs.timeout,
        "test_xml_runner.sh": ctx.attrs.xml,
    })
    return [
        DefaultInfo(default_output = directory),
        RunInfo(args = cmd_args(
            "/usr/bin/bash",
            directory.project("test_launcher.sh"),
            hidden = directory,
        )),
    ]

test_helper_bundle = rule(
    impl = _bundle_impl,
    attrs = {
        "batch": attrs.source(),
        "batch_launcher": attrs.source(),
        "junit": attrs.source(),
        "libtest_selection": attrs.source(),
        "launcher": attrs.source(),
        "postgres": attrs.source(),
        "shard": attrs.source(),
        "timeout": attrs.source(),
        "xml": attrs.source(),
    },
)

def _external_test_impl(ctx):
    test_info = ctx.attrs.test[DefaultInfo]
    # The native rust_test's own command and environment, not its RunInfo:
    # that wraps the binary in the prelude's env injector, whose env file
    # holds absolute paths of the host that wrote it. Carried here, artifact
    # values render project-relative like every other test input.
    native_test = ctx.attrs.test[ExternalRunnerTestInfo]
    env = dict(native_test.env)
    env.update(ctx.attrs.env)
    local = read_root_config("kiln", "execution_mode", "remote") == "local"
    default_executor = CommandExecutorConfig(
        local_enabled = local,
        remote_enabled = not local,
        remote_cache_enabled = not local,
        priority = RE_PRIORITY,
        remote_execution_properties = ctx.attrs.properties,
        remote_execution_use_case = "lash",
    )
    return [
        # The public test label is also the workspace all-targets build and
        # lint label. Forward the native rust_test binary and its diagnostic
        # subtargets so building or selecting [clippy.txt] cannot skip test
        # compilation behind the external-runner wrapper.
        DefaultInfo(
            default_outputs = test_info.default_outputs,
            other_outputs = test_info.other_outputs,
            sub_targets = {
                name: [providers[DefaultInfo]]
                for name, providers in test_info.sub_targets.items()
            },
        ),
        ExternalRunnerTestInfo(
            type = "custom",
            command = [ctx.attrs.runner[RunInfo]] + ctx.attrs.prefix + native_test.command + [
                "--lash-libtest-args",
            ] + ctx.attrs.args,
            env = env,
            labels = ctx.attrs.labels,
            run_from_project_root = True,
            use_project_relative_paths = True,
            supports_test_execution_caching = True,
            default_executor = default_executor,
            executor_overrides = {
                "local": CommandExecutorConfig(
                    local_enabled = True,
                    remote_enabled = False,
                    remote_cache_enabled = False,
                ),
            },
        ),
    ]

_external_test = rule(
    impl = _external_test_impl,
    attrs = {
        "args": attrs.list(attrs.arg()),
        "env": attrs.dict(attrs.string(), attrs.arg()),
        "labels": attrs.list(attrs.string()),
        "prefix": attrs.list(attrs.arg()),
        "properties": attrs.dict(attrs.string(), attrs.string()),
        "runner": attrs.dep(providers = [RunInfo]),
        "test": attrs.dep(providers = [DefaultInfo, ExternalRunnerTestInfo]),
    },
)

def _sharded_test_suite_impl(ctx):
    compile_info = ctx.attrs.compile[DefaultInfo]
    return [DefaultInfo(
        default_outputs = compile_info.default_outputs,
        other_outputs = compile_info.other_outputs,
        sub_targets = {
            name: [providers[DefaultInfo]]
            for name, providers in compile_info.sub_targets.items()
        },
    )]

_sharded_test_suite = rule(
    impl = _sharded_test_suite_impl,
    attrs = {
        "compile": attrs.dep(providers = [DefaultInfo]),
        "labels": attrs.list(attrs.string()),
    },
)

# A test tagged `hermetic-postgres` runs under `postgres_action_runner.py`,
# inside the launcher's watchdog: its action starts the pinned PostgreSQL 16
# on loopback, applies the published schema and hands the test the URL. The
# server, the schema and the runner are declared inputs, so the action runs on
# the pool and its verdict is cached like any other test's.
_POSTGRES_TAG = "hermetic-postgres"
_POSTGRES_PREFIX = [
    "/usr/bin/python3",
    "$(location //tools/buck2:postgres_action_runner)",
    "$(location native//:postgres)",
    "$(location native//:nss_wrapper)",
    "$(location //crates/lash-postgres-store:schema.sql)",
]

def lash_test_wrapper(
        name,
        test,
        args,
        env,
        cpu,
        memory_kb,
        timeout_seconds,
        shard_count = 0,
        tags = []):
    labels = list(tags) + [
        "lash.timeout_seconds={}".format(timeout_seconds),
        "lash.resource_cpu={}".format(cpu),
        "lash.resource_memory_kb={}".format(memory_kb),
    ]
    run_env = dict(env)
    run_env.update({
        "BUILD_WORKSPACE_DIRECTORY": ".",
        "INSTA_WORKSPACE_ROOT": ".",
        "KILN_ACTION_CPU_COUNT": str(cpu),
        "KILN_ACTION_MEMORY_KB": str(memory_kb),
        "PATH": "/usr/bin:/bin",
        "TEST_BINARY": "//{}:{}".format(native.package_name(), name),
        "TEST_TARGET": "//{}:{}".format(native.package_name(), name),
    })
    wrappers = []
    count = shard_count if shard_count > 0 else 1
    service = _POSTGRES_PREFIX if _POSTGRES_TAG in tags else []
    for index in range(count):
        wrapper_name = name if count == 1 else name + "__shard_{}".format(index + 1)

        # The launcher drops the libtest marker only when nothing is prefixed.
        prefix = service + ["--drop-libtest-marker"] if service else []
        wrapper_labels = list(labels)
        if count > 1:
            prefix = service + [
                "/usr/bin/python3",
                "$(location //tools/buck2:test_helpers)/test_shard.py",
                str(count),
                str(index),
                "--weights",
                "$(location //tools/buck2:test_shard_weights)",
                "//{}:{}".format(native.package_name(), name),
            ]
            wrapper_labels.append("lash.shard={}/{}".format(index + 1, count))
            shard_env = dict(run_env)
            shard_env["TEST_SHARD_INDEX"] = str(index)
            shard_env["TEST_TOTAL_SHARDS"] = str(count)
        else:
            shard_env = dict(run_env)
            shard_env["TEST_SHARD_INDEX"] = "0"
            shard_env["TEST_TOTAL_SHARDS"] = "0"
        shard_env["LASH_TEST_EXECUTION_PREFIX_ARG_COUNT"] = str(len(prefix))
        _external_test(
            name = wrapper_name,
            runner = "//tools/buck2:test_helpers",
            test = test,
            args = args,
            env = shard_env,
            labels = wrapper_labels,
            properties = pool_properties(str(cpu), str(memory_kb)),
            prefix = prefix,
            visibility = ["PUBLIC"],
        )
        wrappers.append(":" + wrapper_name)
    if count > 1:
        _sharded_test_suite(
            name = name,
            compile = test,
            labels = labels,
            tests = wrappers,
            visibility = ["PUBLIC"],
        )
