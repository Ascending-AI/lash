"""Generate and compare schema documents on the pinned execution image."""

def _schema_documents_impl(ctx):
    documents = ctx.actions.declare_directory(ctx.label.name + ".documents")
    args = ctx.actions.args()
    args.add(ctx.file.script.path)
    args.add("--generator", ctx.executable.generator.path)
    args.add("--output", documents.path)
    ctx.actions.run_shell(
        command = 'exec /usr/bin/python3 "$@"',
        arguments = [args],
        inputs = [ctx.file.script],
        tools = [ctx.attr.generator[DefaultInfo].files_to_run],
        outputs = [documents],
        mnemonic = "SchemaGenerate",
        progress_message = "Generating schemas %{label}",
    )
    return [DefaultInfo(files = depset([documents]))]

schema_documents = rule(
    implementation = _schema_documents_impl,
    attrs = {
        # Keep the workspace's target configuration: an exec transition would
        # compile a second generator instead of using the ordinary binary.
        "generator": attr.label(executable = True, cfg = "target", mandatory = True),
        "script": attr.label(allow_single_file = [".py"], mandatory = True),
    },
)

def _schema_check_impl(ctx):
    stamp = ctx.actions.declare_file(ctx.label.name + ".ok")
    args = ctx.actions.args()
    args.add(ctx.file.script.path)
    args.add("--generated", ctx.file.documents.path)
    args.add("--check")
    args.add("--stamp", stamp.path)
    ctx.actions.run_shell(
        command = 'exec /usr/bin/python3 "$@"',
        arguments = [args],
        inputs = [ctx.file.script, ctx.file.documents] + ctx.files.checked,
        outputs = [stamp],
        mnemonic = "SchemaCheck",
        progress_message = "Checking schema drift %{label}",
    )
    return [DefaultInfo(files = depset([stamp]))]

schema_check = rule(
    implementation = _schema_check_impl,
    attrs = {
        "documents": attr.label(allow_single_file = True, mandatory = True),
        "checked": attr.label_list(allow_files = [".json"], mandatory = True),
        "script": attr.label(allow_single_file = [".py"], mandatory = True),
    },
)
