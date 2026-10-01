"""Canonical configuration for Rust proc macros and build-script binaries."""

def _host_transition_impl(ctx):
    budget = ctx.attrs.budget[ConstraintSettingInfo]
    host = ctx.attrs.host[ConstraintValueInfo]
    platform_target = ctx.attrs.platform_target[ConstraintValueInfo]

    def apply(platform):
        constraints = dict(platform.configuration.constraints)
        constraints.pop(budget.label, None)
        constraints[host.setting.label] = host
        constraints[platform_target.setting.label] = platform_target
        return PlatformInfo(
            label = "lash-rust-host",
            configuration = ConfigurationInfo(
                constraints = constraints,
                values = platform.configuration.values,
            ),
        )

    return [
        DefaultInfo(),
        TransitionInfo(impl = apply),
    ]

host_transition = rule(
    impl = _host_transition_impl,
    attrs = {
        "budget": attrs.dep(providers = [ConstraintSettingInfo]),
        "host": attrs.dep(providers = [ConstraintValueInfo]),
        "platform_target": attrs.dep(providers = [ConstraintValueInfo]),
    },
    is_configuration_rule = True,
)
