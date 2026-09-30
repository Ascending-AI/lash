"""First-party Clippy configuration provider without a merge action."""

load("@prelude//rust:clippy_configuration.bzl", "ClippyConfiguration")

def _first_party_clippy_configuration_impl(ctx):
    config = ctx.attrs.clippy_toml
    return [
        DefaultInfo(default_output = config),
        ClippyConfiguration(clippy_toml = config),
    ]

first_party_clippy_configuration_rule = rule(
    impl = _first_party_clippy_configuration_impl,
    attrs = {
        "clippy_toml": attrs.source(),
    },
)
