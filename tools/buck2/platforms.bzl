load(":exec_sizes.bzl", "POOL_BUDGETS")

# `-c kiln.memory_scale=N` multiplies the memory of every sized first-party
# compile request for one invocation: the way to get a build past an
# OOM-killed compile before its row is re-measured. Unset, it is 1 and every
# request is exactly its table's.
MEMORY_SCALE = int(read_root_config("kiln", "memory_scale", "1"))

# `-c kiln.re_priority=N` is the priority of every Execute request the
# invocation sends: the pool queue serves a higher one first and never
# preempts a running action. The driver passes KILN_RE_PRIORITY, 0 when unset.
RE_PRIORITY = int(read_root_config("kiln", "re_priority", "0"))
if RE_PRIORITY < -1000 or RE_PRIORITY > 1000:
    fail("kiln.re_priority (KILN_RE_PRIORITY) must be an integer from -1000 to 1000, not {}".format(RE_PRIORITY))

def pool_budget_name(cpu, memory_kb):
    return "pool_{}_{}".format(cpu, memory_kb)


def pool_constraint(cpu, memory_kb):
    return "//tools/buck2:{}".format(pool_budget_name(cpu, memory_kb))


# Selects the platform that runs a tiny, deterministic helper action on the
# invoking host instead of queuing it on the pool.
LOCAL_HELPER_CONSTRAINT = "//tools/buck2:local_helper"


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
                priority = RE_PRIORITY,
                remote_execution_properties = pool_properties(cpu, memory),
                remote_execution_use_case = "lash",
                remote_output_paths = "output_paths",
            ),
        ))

    # The one platform that runs on the invoking host. It is registered last
    # and carries its own value of the budget setting, so only a target that
    # names `LOCAL_HELPER_CONSTRAINT` resolves to it: a compile, link or
    # archive can never land there.
    helper = ctx.attrs.local_helper
    platforms.append(ExecutionPlatformInfo(
        label = helper.label.raw_target(),
        configuration = ConfigurationInfo(
            constraints = constraints | helper[ConfigurationInfo].constraints,
            values = {},
        ),
        executor_config = CommandExecutorConfig(
            local_enabled = True,
            remote_enabled = False,
            remote_cache_enabled = False,
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
        "local_helper": attrs.dep(providers = [ConfigurationInfo]),
    },
)


def declare_pool_platforms(name, constraints):
    native.constraint_setting(name = "budget")
    budgets = {}
    if MEMORY_SCALE < 1:
        fail("kiln.memory_scale must be a positive integer")
    scaled = [
        (cpu, memory_kb * MEMORY_SCALE)
        for cpu, memory_kb in POOL_BUDGETS
        if MEMORY_SCALE > 1 and (cpu, memory_kb * MEMORY_SCALE) not in POOL_BUDGETS
    ]
    for cpu, memory_kb in POOL_BUDGETS + scaled:
        budget_name = pool_budget_name(cpu, memory_kb)
        native.constraint_value(name = budget_name, constraint_setting = ":budget")
        budgets["{}|{}|{}".format(budget_name, cpu, memory_kb)] = ":" + budget_name

    # Compatibility names used by the one-crate proof remain valid while the
    # generated graph names every shape by its exact request.
    for alias, cpu, memory_kb in [("small", 1, 1572864), ("large", 2, 3145728)]:
        native.constraint_value(name = alias, constraint_setting = ":budget")
        budgets["{}|{}|{}".format(alias, cpu, memory_kb)] = ":" + alias

    native.constraint_value(name = "local_helper", constraint_setting = ":budget")
    platforms(name = name, budgets = budgets, constraints = constraints, local_helper = ":local_helper")

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
