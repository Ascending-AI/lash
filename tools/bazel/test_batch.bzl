"""Batched execution of plain libtest binaries (FIG-3365).

Every `rust_test` target pays four bookkeeping actions per run -- the runfiles
symlink tree, its source manifest, the repo-mapping manifest, and the test
runner itself -- and each tree materializes the crate's sources and dep graph
as a fresh symlink forest. With ~100 test targets in `//:workspace_tests`,
that scaffolding is the dominant local-side cost of the Bazel partition even
when every test result is a remote-cache hit.

`lash_batch_test` runs a package's *plain* test binaries -- no `args`, no
extra `env`, no sharding, no custom timeout -- inside one test action with one
merged runfiles tree. The batch boundary is the package boundary, so cache
granularity stays honest: a change to crate X invalidates only X's batch, and
the member `rust_test` targets remain individually runnable for developers and
`bazel test //...`.

Tests that carry any per-case contract (skip args, `RUST_TEST_THREADS=1`,
`CARGO_BIN_EXE_*` helpers, sharding, explicit timeouts, service gates) are
never batched -- the generator emits only qualifying labels, and the coverage
contract reconciles `WORKSPACE_TEST_BATCHES` against
`WORKSPACE_BAZEL_TEST_TARGETS` so a silently dropped member fails CI.

A batch declares what it runs. The generator sizes it from its members' own
test-run requests (measured per label, see `tools/bazel/test-run-sizes.json`):
it reserves the sum of its `jobs` largest members' requests, as
`test.cpu_count` / `test.memory_kb` on the TestRunner spawn, and the runner
starts at most `jobs` members at once, so whichever members run side by side
fit the reservation.
"""

def _rloc(file):
    """Path of a file inside a test's runfiles tree."""
    short = file.short_path
    if short.startswith("../"):
        return short[len("../"):]
    return "_main/" + short

def _lash_batch_test_impl(ctx):
    lines = []
    runfiles = ctx.runfiles(files = [ctx.file._runner, ctx.file._junit_xml])
    for target in ctx.attr.tests:
        executable = target[DefaultInfo].files_to_run.executable
        if executable == None:
            fail("batch member %s has no executable" % target.label)
        lines.append(_rloc(executable))
        runfiles = runfiles.merge(target[DefaultInfo].default_runfiles)
        runfiles = ctx.runfiles(
            transitive_files = depset([executable]),
        ).merge(runfiles)

    manifest = ctx.actions.declare_file(ctx.label.name + ".manifest")
    ctx.actions.write(manifest, "\n".join(lines) + "\n")

    script = ctx.actions.declare_file(ctx.label.name + ".sh")
    ctx.actions.write(script, """#!/usr/bin/env bash
set -euo pipefail
export LASH_BATCH_MANIFEST="$TEST_SRCDIR/{manifest}"
export LASH_BATCH_JOBS={jobs}
exec bash "$TEST_SRCDIR/_main/tools/bazel/test_batch_runner.sh" "$@"
""".format(manifest = _rloc(manifest), jobs = ctx.attr.jobs))

    runfiles = ctx.runfiles(files = [manifest, script]).merge(runfiles)
    return [DefaultInfo(executable = script, runfiles = runfiles)]

_lash_batch_test = rule(
    implementation = _lash_batch_test_impl,
    doc = "Runs a package's plain libtest binaries in one test action, " +
          "sharing a single runfiles tree instead of one forest per binary " +
          "(FIG-3365).",
    attrs = {
        "jobs": attr.int(
            doc = "Members the runner starts at once.",
            mandatory = True,
        ),
        "tests": attr.label_list(
            doc = "rust_test targets whose executables this batch runs. " +
                  "Only plain members -- no args, env, sharding, or custom " +
                  "timeout -- may be listed.",
            mandatory = True,
            providers = [DefaultInfo],
        ),
        "_junit_xml": attr.label(
            default = "//tools/bazel:junit_xml.py",
            allow_single_file = True,
        ),
        "_runner": attr.label(
            default = "//tools/bazel:test_batch_runner.sh",
            allow_single_file = True,
        ),
    },
    test = True,
)

def lash_batch_test(name, tests, jobs, budget, **kwargs):
    """A package's plain test binaries, run `jobs` at a time.

    Args:
      name: the batch label.
      tests: the member `rust_test` targets.
      jobs: members the runner starts at once.
      budget: the test-run reservation from generated
        `//tools/bazel:exec_sizes.bzl`'s `test_batch_budget(<label>)` -- the
        sum of the `jobs` largest member requests as `cpu_count`/`memory_kb`.
      **kwargs: forwarded to the rule.
    """
    _lash_batch_test(
        name = name,
        exec_properties = {
            "test.cpu_count": str(budget["cpu_count"]),
            "test.memory_kb": str(budget["memory_kb"]),
        },
        jobs = jobs,
        tests = tests,
        **kwargs
    )
