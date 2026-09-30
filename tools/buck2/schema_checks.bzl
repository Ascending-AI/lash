"""Hermetic schema generation and drift checks on the pinned executor."""

load(":platforms.bzl", "pool_constraint")

_CPU = "1"
_MEMORY_KB = "1572864"

def _action_env():
    return {
        "KILN_ACTION_CPU_COUNT": _CPU,
        "KILN_ACTION_MEMORY_KB": _MEMORY_KB,
        "PATH": "/usr/bin:/bin",
    }

def _schema_documents_impl(ctx):
    documents = ctx.actions.declare_output(ctx.label.name + ".documents", dir = True)
    command = ["/usr/bin/python3", ctx.attrs.script]
    for generator in ctx.attrs.generators:
        command.extend(["--generator", generator[RunInfo]])
    command.extend(["--output", documents.as_output()])
    ctx.actions.run(command, env = _action_env(), category = "schema_generate")
    return [DefaultInfo(default_output = documents)]

_schema_documents = rule(
    impl = _schema_documents_impl,
    attrs = {
        "generators": attrs.list(attrs.dep(providers = [RunInfo])),
        "script": attrs.source(),
    },
)

def schema_documents(name, **kwargs):
    _schema_documents(
        name = name,
        exec_compatible_with = [pool_constraint(1, 1572864)],
        visibility = ["PUBLIC"],
        **kwargs
    )

def _schema_check_impl(ctx):
    stamp = ctx.actions.declare_output(ctx.label.name + ".ok")
    command = cmd_args([
        "/usr/bin/python3",
        ctx.attrs.script,
        "--generated",
        ctx.attrs.documents[DefaultInfo].default_outputs[0],
        "--check",
        "--stamp",
        stamp.as_output(),
    ], hidden = ctx.attrs.checked)
    ctx.actions.run(
        command,
        env = _action_env(),
        category = "schema_check",
    )
    return [DefaultInfo(default_output = stamp)]

_schema_check = rule(
    impl = _schema_check_impl,
    attrs = {
        "checked": attrs.list(attrs.source()),
        "documents": attrs.dep(),
        "script": attrs.source(),
    },
)

def schema_check(name, **kwargs):
    _schema_check(
        name = name,
        exec_compatible_with = [pool_constraint(1, 1572864)],
        visibility = ["PUBLIC"],
        **kwargs
    )

def _schema_check_group_impl(ctx):
    outputs = []
    for check in ctx.attrs.checks:
        outputs.extend(check[DefaultInfo].default_outputs)
    return [DefaultInfo(default_outputs = outputs)]

_schema_check_group = rule(
    impl = _schema_check_group_impl,
    attrs = {"checks": attrs.list(attrs.dep())},
)

def schema_check_group(name, **kwargs):
    _schema_check_group(name = name, visibility = ["PUBLIC"], **kwargs)
