"""The `--run_under` prefix that makes every Lash test write its own JUnit report.

Bazel 9.1 follows every test that leaves `$XML_OUTPUT_FILE` unwritten with a
second TestRunner spawn, `generate-xml.sh`. That spawn carries the test's own
run request (`test.cpu_count` / `test.memory_kb`) for about 0.1 s of work and queues for a pool slot a second time. Bazel has no
option to turn it off or resize it: the report must already exist when the
test spawn returns.

`.bazelrc` sets `test --run_under=//tools/bazel:test_xml_runner`, so every test
action -- plain and sharded `rust_test`, `lash_batch_test`, the UI fixtures --
runs through `test_xml_runner.sh`, which writes the report with
`junit_xml.py`. `--run_under` is a test option: Bazel re-configures only test
targets for it, and the prefix's runfiles join the test's own runfiles tree.
"""

def _test_xml_runner_impl(ctx):
    executable = ctx.actions.declare_file(ctx.label.name)
    ctx.actions.symlink(
        output = executable,
        target_file = ctx.file.src,
        is_executable = True,
    )
    return [DefaultInfo(
        executable = executable,
        runfiles = ctx.runfiles(files = ctx.files.data),
    )]

test_xml_runner = rule(
    implementation = _test_xml_runner_impl,
    doc = "An executable shell script with runfiles, usable as `--run_under`.",
    attrs = {
        "data": attr.label_list(
            doc = "Files the script finds next to itself in the runfiles tree.",
            allow_files = True,
        ),
        "src": attr.label(
            doc = "The script.",
            allow_single_file = [".sh"],
            mandatory = True,
        ),
    },
    executable = True,
)
