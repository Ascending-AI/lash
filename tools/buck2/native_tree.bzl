"""Package a downloaded native tool directory as one declared artifact."""

def _native_tree(ctx):
    tree = ctx.actions.copied_dir(ctx.label.name, ctx.attrs.srcs)
    return [DefaultInfo(default_output = tree)]

native_tree = rule(
    impl = _native_tree,
    attrs = {"srcs": attrs.dict(attrs.string(), attrs.source())},
)
