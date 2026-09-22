"""Fail-closed selection of metadata-only outputs for Cargo-check roots."""

def _check_only_roots_impl(ctx):
    if not ctx.attr.roots:
        fail("check_only_roots needs at least one compile-only root")

    outputs = []
    for root in ctx.attr.roots:
        if OutputGroupInfo not in root or not hasattr(root[OutputGroupInfo], "build_metadata"):
            fail("{} has no build_metadata output group".format(root.label))
        files = root[OutputGroupInfo].build_metadata.to_list()
        if len(files) != 1 or files[0].extension != "rmeta":
            fail("{} must provide exactly one check-only .rmeta output".format(root.label))
        outputs.extend(files)

    # Only files, never CrateInfo: a binary's empty .rmeta is an action
    # completion witness and must not become a dependency of another crate.
    return [DefaultInfo(files = depset(outputs))]

check_only_roots = rule(
    implementation = _check_only_roots_impl,
    attrs = {
        "roots": attr.label_list(mandatory = True),
    },
)
