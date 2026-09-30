POOL_BUDGETS = [
    (1, 1048576),
    (1, 1572864),
    (1, 2097152),
    (2, 1048576),
    (2, 1572864),
    (2, 2097152),
    (2, 2621440),
    (2, 3145728),
    (2, 3670016),
    (2, 4194304),
    (2, 4718592),
    (2, 5242880),
    (3, 1048576),
    (3, 1572864),
    (3, 12058624),
    (4, 1048576),
    (4, 1572864),
    (4, 2097152),
    (4, 3670016),
    (5, 1048576),
    (5, 2097152),
    (6, 3145728),
    (8, 1572864),
]


def pool_budget_name(cpu, memory_kb):
    return "pool_{}_{}".format(cpu, memory_kb)


def pool_constraint(cpu, memory_kb):
    return "//tools/buck2:{}".format(pool_budget_name(cpu, memory_kb))


def pool_properties(cpu = "1", memory = "1572864"):
    runtime = read_root_config("kiln", "executor_runtime")
    mode = read_root_config("kiln", "execution_mode", "remote")
    if not runtime and mode != "local":
        fail("Missing kiln.executor_runtime in .buckconfig.local")
    return {
        "cpu_count": cpu,
        "memory_kb": memory,
        "cpu_arch": "x86_64",
        "OSFamily": "linux",
        "kiln_executor_runtime": runtime or "local",
    }


def test_remote_execution(cpu, memory_kb):
    local = read_root_config("kiln", "execution_mode", "remote") == "local"
    return {
        "capabilities": pool_properties(str(cpu), str(memory_kb)),
        "local_enabled": local,
        "remote_cache_enabled": not local,
        "use_case": "lash",
    }

def _platforms(ctx):
    local = read_root_config("kiln", "execution_mode", "remote") == "local"
    constraints = {
        key: value
        for dep in ctx.attrs.constraints
        for key, value in dep[ConfigurationInfo].constraints.items()
    }
    platforms = []
    for spec, budget in ctx.attrs.budgets.items():
        _name, cpu, memory = spec.split("|")
        platforms.append(ExecutionPlatformInfo(
            label = budget.label.raw_target(),
            configuration = ConfigurationInfo(
                constraints = constraints | budget[ConfigurationInfo].constraints,
                values = {},
            ),
            executor_config = CommandExecutorConfig(
                local_enabled = local,
                remote_enabled = not local,
                remote_execution_properties = pool_properties(cpu, memory),
                remote_execution_use_case = "lash",
                remote_output_paths = "output_paths",
            ),
        ))
    return [DefaultInfo(), ExecutionPlatformRegistrationInfo(platforms = platforms)]

platforms = rule(
    impl = _platforms,
    attrs = {
        "budgets": attrs.dict(
            attrs.string(),
            attrs.dep(providers = [ConfigurationInfo]),
        ),
        "constraints": attrs.list(attrs.dep(providers = [ConfigurationInfo])),
    },
)


def declare_pool_platforms(name, constraints):
    native.constraint_setting(name = "budget")
    budgets = {}
    for cpu, memory_kb in POOL_BUDGETS:
        budget_name = pool_budget_name(cpu, memory_kb)
        native.constraint_value(name = budget_name, constraint_setting = ":budget")
        budgets["{}|{}|{}".format(budget_name, cpu, memory_kb)] = ":" + budget_name

    # Compatibility names used by the one-crate proof remain valid while the
    # generated graph names every shape by its exact request.
    for alias, cpu, memory_kb in [("small", 1, 1572864), ("large", 2, 3145728)]:
        native.constraint_value(name = alias, constraint_setting = ":budget")
        budgets["{}|{}|{}".format(alias, cpu, memory_kb)] = ":" + alias

    platforms(name = name, budgets = budgets, constraints = constraints)

def _probe(ctx):
    out = ctx.actions.declare_output("runtime.json")
    ctx.actions.run(
        ["/usr/bin/python3", ctx.attrs.script, out.as_output()],
        env = {
            "KILN_ACTION_CPU_COUNT": ctx.attrs.cpu,
            "KILN_ACTION_MEMORY_KB": ctx.attrs.memory_kb,
        },
        category = "runtime_probe",
    )
    return [DefaultInfo(default_output = out)]

_runtime_probe = rule(impl = _probe, attrs = {
    "cpu": attrs.string(),
    "memory_kb": attrs.string(),
    "script": attrs.source(),
})

def runtime_probe(name, script, cpu = 1, memory_kb = 1572864):
    _runtime_probe(
        name = name,
        script = script,
        cpu = str(cpu),
        memory_kb = str(memory_kb),
        exec_compatible_with = [pool_constraint(cpu, memory_kb)],
    )
