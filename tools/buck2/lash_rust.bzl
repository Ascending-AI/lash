"""Cargo-shaped first-party Rust macros for the generated Lash graph."""

load("@prelude//rust:cargo_buildscript.bzl", "buildscript_run")
load(":buildscript_manifest.bzl", "buildscript_manifest", "buildscript_sources")
load(":clippy_policy.bzl", "FIRST_PARTY_CLIPPY_LINT_FLAGS", "FIRST_PARTY_RUST_LINT_FLAGS", "first_party_clippy_configuration")
load(":deps.bzl", "PACKAGE_DEPS")
load(":exec_sizes.bzl", "HELPER_BUDGET")
load(":platforms.bzl", "MEMORY_SCALE", "pool_constraint")
load(":profile.bzl", "FIRST_PARTY_OPT_LEVELS")
load(":run_binary.bzl", "binary_run_attrs", "lash_run_binary")
load(":source_tree.bzl", "lash_rust_source_tree")
load(":test_rules.bzl", "lash_test_wrapper")
load(":ui_fixtures.bzl", "ui_fixture_harness")

_DEFAULT_CPU = 1
_DEFAULT_MEMORY_KB = 1572864
_IGNORED = ["BUCK", "**/__pycache__/**", "**/node_modules/**"]
_TIMEOUTS = {None: 300, "short": 60, "moderate": 300, "long": 900, "eternal": 3600}
_HOST_TRANSITION = "//tools/buck2:host_transition"
_OPTIMIZED_FLAGS = [
    "-Copt-level=3",
    "-Cdebuginfo=0",
    "-Cstrip=debuginfo",
    "-Cembed-bitcode=no",
]

def _cargo_env(package_name, crate_name, manifest_dir, version, extra = {}):
    result = {
        "CARGO_CRATE_NAME": crate_name,
        # Project-relative at compile and run time alike: tests run from the
        # project root, where this names the package directory.
        "CARGO_MANIFEST_DIR": manifest_dir,
        "CARGO_PKG_NAME": package_name,
        "CARGO_PKG_VERSION": version,
        "KILN_RELATIVE_CARGO_MANIFEST_DIR": manifest_dir,
    }
    result.update(extra)
    return result

def _rustc_flags(package_name, declared_features, extra = []):
    feature_values = ",".join(['"{}"'.format(feature) for feature in declared_features])
    profile = select({
        "//tools/buck2:profile_host": _OPTIMIZED_FLAGS,
        "//tools/buck2:profile_optimized": _OPTIMIZED_FLAGS,
        "DEFAULT": [],
    })
    if package_name in FIRST_PARTY_OPT_LEVELS:
        profile += select({
            "//tools/buck2:profile_host": [],
            "DEFAULT": ["-Copt-level={}".format(FIRST_PARTY_OPT_LEVELS[package_name])],
        })
    judged = select({
        "//tools/buck2:profile_judged": ["-Cdebug-assertions=no", "-Coverflow-checks=no"],
        "DEFAULT": [],
    })
    return FIRST_PARTY_RUST_LINT_FLAGS + FIRST_PARTY_CLIPPY_LINT_FLAGS + [
        "--check-cfg=cfg(docsrs,test)",
        "--check-cfg=cfg(feature,values({}))".format(feature_values),
    ] + profile + extra + judged

def _request(exec_properties):
    return (
        int(exec_properties.get("cpu_count", str(_DEFAULT_CPU))),
        int(exec_properties.get("memory_kb", str(_DEFAULT_MEMORY_KB))),
    )

def _run_request(exec_properties):
    return (
        int(exec_properties["test.cpu_count"]),
        int(exec_properties["test.memory_kb"]),
    )

def _pool(cpu, memory):
    return {
        "exec_compatible_with": [pool_constraint(cpu, memory)],
        "kiln_action_cpu_count": str(cpu),
        "kiln_action_memory_kb": str(memory),
    }

def _resource_attrs(exec_properties):
    """A target's compile request: its platform and the supervisor's limits.

    An `optimized.` request applies in the configurations that compile with
    the optimized flags, where LLVM holds more than a dev compile does. A
    target without one keeps plain attributes, and a dev configuration takes
    the default branch, so neither's action keys depend on the select.
    `-c kiln.memory_scale=N` multiplies the memory of every request made here
    for one invocation.
    """
    cpu, memory = _request(exec_properties)
    dev = _pool(cpu, memory * MEMORY_SCALE)
    optimized = _pool(
        int(exec_properties.get("optimized.cpu_count", str(cpu))),
        int(exec_properties.get("optimized.memory_kb", str(memory))) * MEMORY_SCALE,
    )
    if optimized == dev:
        return dev
    return {
        key: select({
            "//tools/buck2:profile_host": optimized[key],
            "//tools/buck2:profile_optimized": optimized[key],
            "DEFAULT": value,
        })
        for key, value in dev.items()
    }

def _clippy_attrs(exec_properties):
    """The Clippy twin's request: its crate's measured Clippy row, else the
    target's own compile request."""
    if "clippy.cpu_count" not in exec_properties:
        return _resource_attrs(exec_properties)
    return _pool(
        int(exec_properties["clippy.cpu_count"]),
        int(exec_properties["clippy.memory_kb"]) * MEMORY_SCALE,
    )

def _rust_rule(rule, name, clippy_name, exec_properties, **attrs):
    """Declares a Rust target and its Clippy twin, `clippy_name`.

    Buck2 resolves one execution platform per target, so every action a
    target runs reserves its compile request, Clippy included, though Clippy
    holds a fraction of a compile's memory. The twin is the same rule with the
    same attributes on a platform sized for Clippy (`clippy-sizes.json`); its
    `[clippy.txt]` is what `kiln clippy` and `//:workspace_clippy` build. It
    shares the target's dependencies and source tree, so building it runs
    Clippy's own action and nothing the target has not already built.
    """
    rule(name = name, **(attrs | _resource_attrs(exec_properties)))
    twin = dict(attrs)
    twin.update(_clippy_attrs(exec_properties))
    twin["visibility"] = ["PUBLIC"]
    rule(name = clippy_name, **twin)

def _named_deps(package_name, include_dev = False, build = False):
    groups = PACKAGE_DEPS[package_name]
    result = dict(groups["build"] if build else groups["normal"])
    if include_dev:
        result.update(groups["dev"])
    return result

def _variant_named_deps(package_name, include_dev, pruned_deps, variant_deps, extra_deps, library, library_crate_name):
    result = _named_deps(package_name, include_dev = include_dev)

    # PACKAGE_DEPS is the workspace resolution; a variant's own resolution
    # leaves some of its optional dependencies off.
    for extern in pruned_deps:
        result.pop(extern)
    for extern, label in result.items():
        result[extern] = variant_deps.get(label, label)
    for label, extern in extra_deps.items():
        result[extern] = label
    if library:
        result[library_crate_name] = library
    return result

def _srcs(crate_root, patterns):
    return [crate_root] + glob(patterns, exclude = _IGNORED + [crate_root])

def _source_attrs(name, manifest_dir, crate_root, package_srcs, workspace_srcs):
    if not workspace_srcs:
        return {
            "crate_root": crate_root,
            "srcs": package_srcs,
        }
    tree = name + "__source_tree"
    lash_rust_source_tree(
        name = tree,
        package = manifest_dir,
        package_srcs = package_srcs,
        workspace_srcs = workspace_srcs,
    )
    return {
        "crate_root": manifest_dir + "/" + crate_root,
        # The tree is keyed by repository path; see prelude_overlay.py.
        "kiln_repo_rooted_srcs": True,
        "srcs_filegroup": ":" + tree,
    }

def _resources(files, labels):
    """Name a package file by its path and another target by its label.

    Buck2 names an unnamed resource by its output's short path, so two
    packages' `:rust_sources` filegroups would share one name and all but one
    would silently drop out of the binary's inputs.
    """
    result = {path: path for path in files}
    for label in labels:
        parts = [part for part in label.replace(":", "/").split("/") if part]
        result["__lash_inputs__/" + "/".join(parts)] = label
    return result

def _data(patterns = ["**"], exclude = []):
    return glob(patterns, exclude = _IGNORED + exclude + ["**/*.rs"])

def _buildscript_args(build_script, env, flags):
    if not build_script:
        return env, flags
    result = dict(env)
    result["OUT_DIR"] = "$(location {}[out_dir])".format(build_script)
    return result, flags + ["@$(location {}[rustc_flags])".format(build_script)]

def lash_buildscript_sources(name, srcs):
    buildscript_sources(
        name = name,
        srcs = srcs,
        visibility = ["PUBLIC"],
    )

def lash_rust_build_script(
        name,
        crate_features,
        declared_features,
        manifest_dir,
        package_name,
        version,
        data = [],
        extra_data = [],
        extra_srcs = [],
        build_script_env = {}):
    binary = name + "__build"
    manifest = name + "__manifest"
    buildscript_manifest(
        name = manifest,
        package_srcs = ["Cargo.toml"] + data,
        workspace_srcs = extra_data,
    )
    source_attrs = _source_attrs(
        binary,
        manifest_dir,
        "build.rs",
        _srcs("build.rs", ["build/**/*.rs"]) + data,
        extra_srcs,
    )
    _rust_rule(
        native.rust_binary,
        binary,
        binary + "__clippy",
        {},
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = "build_script_build",
        edition = "2024",
        env = _cargo_env(package_name, "build_script_build", manifest_dir, version),
        features = crate_features,
        named_deps = _named_deps(package_name, build = True),
        rustc_flags = _rustc_flags(package_name, declared_features),
        incoming_transition = _HOST_TRANSITION,
        visibility = ["PUBLIC"],
        **source_attrs
    )
    # Running a build script is a helper action, sized as one.
    cpu, memory = HELPER_BUDGET
    env = dict(build_script_env)
    env.update({
        "KILN_ACTION_CPU_COUNT": str(cpu),
        "KILN_ACTION_MEMORY_KB": str(memory),
        "PATH": "/usr/bin:/bin",
    })
    buildscript_run(
        name = name,
        buildscript_rule = ":" + binary,
        package_name = package_name,
        version = version,
        cargo_rustc_flags = _rustc_flags(package_name, declared_features),
        manifest_dir = ":" + manifest,
        env = env,
        exec_compatible_with = [pool_constraint(cpu, memory)],
        kiln_action_cpu_count = str(cpu),
        kiln_action_memory_kb = str(memory),
        visibility = ["PUBLIC"],
    )

def lash_rust_library(
        name,
        crate_name,
        crate_features,
        declared_features,
        manifest_dir,
        package_name,
        version,
        build_script = None,
        exec_properties = {},
        test_srcs = [],
        compile_data_patterns = [],
        extra_compile_data = []):
    env, flags = _buildscript_args(
        build_script,
        _cargo_env(package_name, crate_name, manifest_dir, version),
        _rustc_flags(package_name, declared_features),
    )
    package_compile_data = _data(compile_data_patterns)
    source_attrs = _source_attrs(
        name,
        manifest_dir,
        "src/lib.rs",
        glob(["src/**/*.rs", "shared/**/*.rs"], exclude = _IGNORED + test_srcs) + package_compile_data,
        extra_compile_data,
    )
    _rust_rule(
        native.rust_library,
        name,
        name + "__clippy",
        exec_properties,
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = crate_name,
        edition = "2024",
        env = env,
        features = crate_features,
        named_deps = _named_deps(package_name),
        resources = _resources(package_compile_data, extra_compile_data),
        rustc_flags = flags,
        visibility = ["PUBLIC"],
        **source_attrs
    )

def lash_rust_binary(
        name,
        crate_name,
        crate_root,
        crate_features,
        declared_features,
        manifest_dir,
        package_name,
        version,
        exec_properties = {},
        include_dev_deps = False,
        library = None,
        library_crate_name = None,
        compile_data_patterns = [],
        data_exclude = [],
        extra_compile_data = [],
        extra_data = [],
        run_env = {},
        rustc_env = {},
        tags = []):
    deps = _named_deps(package_name, include_dev = include_dev_deps)
    if library:
        deps[library_crate_name] = library
    package_compile_data = _data(compile_data_patterns, data_exclude)
    source_attrs = _source_attrs(
        name,
        manifest_dir,
        crate_root,
        _srcs(crate_root, ["src/**/*.rs", "examples/**/*.rs", "benches/**/*.rs", "shared/**/*.rs"]) + package_compile_data,
        extra_compile_data,
    )
    run_attrs = binary_run_attrs(name, extra_data, run_env)
    _rust_rule(
        lash_run_binary if run_attrs else native.rust_binary,
        name,
        name + "__clippy",
        exec_properties,
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = crate_name,
        edition = "2024",
        env = _cargo_env(package_name, crate_name, manifest_dir, version, rustc_env),
        features = crate_features,
        named_deps = deps,
        resources = _resources(package_compile_data, extra_compile_data),
        rustc_flags = _rustc_flags(package_name, declared_features),
        labels = tags,
        visibility = ["PUBLIC"],
        **(source_attrs | run_attrs)
    )

def _rust_test(
        name,
        crate_name,
        crate_root,
        crate_features,
        declared_features,
        manifest_dir,
        package_name,
        version,
        args,
        build_script,
        exec_properties,
        srcs_patterns,
        data_exclude,
        extra_compile_data,
        extra_data,
        library,
        library_crate_name,
        rustc_env,
        shard_count,
        test_env,
        tags,
        timeout,
        named_deps):
    binary = name + "__rust_test"
    if library:
        named_deps[library_crate_name] = library
    compile_env, flags = _buildscript_args(
        build_script,
        _cargo_env(package_name, crate_name, manifest_dir, version, rustc_env),
        _rustc_flags(package_name, declared_features),
    )
    package_files = glob(["**"], exclude = _IGNORED + data_exclude)
    package_data = _data(["**"], data_exclude)
    source_attrs = _source_attrs(
        binary,
        manifest_dir,
        crate_root,
        _srcs(crate_root, srcs_patterns) + package_data,
        extra_compile_data,
    )
    _rust_rule(
        native.rust_test,
        binary,
        name + "__clippy",
        exec_properties,
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = crate_name,
        edition = "2024",
        env = compile_env,
        features = crate_features,
        named_deps = named_deps,
        resources = _resources(package_files, extra_compile_data + extra_data),
        rustc_flags = flags,
        labels = ["lash.internal_test_binary"],
        visibility = [],
        **source_attrs
    )
    if "cargo-trybuild" in tags:
        ui_fixture_harness(
            name = name + "__ui_inputs",
            harness = ":" + binary,
            named_deps = named_deps,
            edition = "2024",
            features = crate_features,
            visibility = ["PUBLIC"],
        )
    cpu, memory = _run_request(exec_properties)
    lash_test_wrapper(
        name = name,
        test = ":" + binary,
        args = args,
        env = test_env,
        cpu = cpu,
        memory_kb = memory,
        timeout_seconds = _TIMEOUTS[timeout],
        shard_count = shard_count,
        tags = tags,
    )

def lash_rust_unit_test(
        name,
        crate_name,
        crate_root,
        crate_features,
        declared_features,
        manifest_dir,
        package_name,
        version,
        args = [],
        build_script = None,
        exec_properties = {},
        srcs_patterns = ["src/**/*.rs", "tests/**/*.rs", "shared/**/*.rs"],
        data_exclude = [],
        extra_compile_data = [],
        extra_data = [],
        library = None,
        library_crate_name = None,
        shard_count = 0,
        test_env = {},
        tags = [],
        timeout = None):
    _rust_test(
        name, crate_name, crate_root, crate_features, declared_features,
        manifest_dir, package_name, version, args, build_script, exec_properties,
        srcs_patterns, data_exclude, extra_compile_data, extra_data, library,
        library_crate_name, {}, shard_count, test_env, tags, timeout,
        _named_deps(package_name, include_dev = True),
    )

def lash_rust_integration_test(
        name,
        crate_name,
        crate_root,
        crate_features,
        declared_features,
        manifest_dir,
        package_name,
        version,
        args = [],
        exec_properties = {},
        srcs_patterns = ["src/**/*.rs", "tests/**/*.rs", "examples/**/*.rs", "shared/**/*.rs"],
        data_exclude = [],
        library = None,
        library_crate_name = None,
        extra_compile_data = [],
        extra_data = [],
        rustc_env = {},
        shard_count = 0,
        test_env = {},
        tags = []):
    _rust_test(
        name, crate_name, crate_root, crate_features, declared_features,
        manifest_dir, package_name, version, args, None, exec_properties,
        srcs_patterns, data_exclude, extra_compile_data, extra_data, library,
        library_crate_name, rustc_env, shard_count, test_env, tags, None,
        _named_deps(package_name, include_dev = True),
    )

def _rust_doc_impl(ctx):
    # The prelude always attaches a rustdoc test to `[doc]`. Every workspace
    # library keeps `doctest = false`, so only the documentation is forwarded:
    # an alias would make each doc label a test that compiles prose examples
    # and bypasses the launcher that writes the JUnit report.
    doc = ctx.attrs.doc[DefaultInfo]
    return [DefaultInfo(
        default_outputs = doc.default_outputs,
        other_outputs = doc.other_outputs,
    )]

_rust_doc = rule(
    impl = _rust_doc_impl,
    attrs = {"doc": attrs.dep(providers = [DefaultInfo])},
)

def lash_rust_doc(name, crate, **_kwargs):
    _rust_doc(name = name, doc = crate + "[doc]", visibility = ["PUBLIC"])

def lash_rust_feature_library(
        name,
        package_name,
        crate_name,
        crate_features,
        declared_features,
        manifest_dir,
        version,
        build_script = None,
        exec_properties = {},
        test_srcs = [],
        compile_data_patterns = [],
        extra_compile_data = [],
        pruned_deps = [],
        variant_deps = {},
        extra_deps = {},
        tags = []):
    env, flags = _buildscript_args(
        build_script,
        _cargo_env(package_name, crate_name, manifest_dir, version),
        _rustc_flags(package_name, declared_features),
    )
    package_compile_data = _data(compile_data_patterns)
    source_attrs = _source_attrs(
        name,
        manifest_dir,
        "src/lib.rs",
        glob(["src/**/*.rs", "shared/**/*.rs"], exclude = _IGNORED + test_srcs) + package_compile_data,
        extra_compile_data,
    )
    _rust_rule(
        native.rust_library,
        name,
        name + "__clippy",
        exec_properties,
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = crate_name,
        edition = "2024",
        env = env,
        features = crate_features,
        named_deps = _variant_named_deps(package_name, False, pruned_deps, variant_deps, extra_deps, None, None),
        resources = _resources(package_compile_data, extra_compile_data),
        rustc_flags = flags,
        labels = tags,
        visibility = ["PUBLIC"],
        **source_attrs
    )

def lash_rust_feature_binary(
        name,
        package_name,
        include_dev_deps = False,
        pruned_deps = [],
        variant_deps = {},
        extra_deps = {},
        library = None,
        library_crate_name = None,
        **kwargs):
    # Keep the public signature stable; ordinary binary emission is reused
    # after replacing the generated dependency table for this call.
    deps = _variant_named_deps(package_name, include_dev_deps, pruned_deps, variant_deps, extra_deps, library, library_crate_name)
    _feature_binary(name, package_name, deps, **kwargs)

def _feature_binary(name, package_name, named_deps, **kwargs):
    crate_name = kwargs.pop("crate_name")
    crate_root = kwargs.pop("crate_root")
    crate_features = kwargs.pop("crate_features")
    declared_features = kwargs.pop("declared_features")
    manifest_dir = kwargs.pop("manifest_dir")
    version = kwargs.pop("version")
    exec_properties = kwargs.pop("exec_properties", {})
    rustc_env = kwargs.pop("rustc_env", {})
    tags = kwargs.pop("tags", [])
    extra_compile_data = kwargs.pop("extra_compile_data", [])
    extra_data = kwargs.pop("extra_data", [])
    run_env = kwargs.pop("run_env", {})
    compile_data_patterns = kwargs.pop("compile_data_patterns", [])
    data_exclude = kwargs.pop("data_exclude", [])
    package_compile_data = _data(compile_data_patterns, data_exclude)
    source_attrs = _source_attrs(
        name,
        manifest_dir,
        crate_root,
        _srcs(crate_root, ["src/**/*.rs", "examples/**/*.rs", "benches/**/*.rs", "shared/**/*.rs"]) + package_compile_data,
        extra_compile_data,
    )
    run_attrs = binary_run_attrs(name, extra_data, run_env)
    _rust_rule(
        lash_run_binary if run_attrs else native.rust_binary,
        name,
        name + "__clippy",
        exec_properties,
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = crate_name,
        edition = "2024",
        env = _cargo_env(package_name, crate_name, manifest_dir, version, rustc_env),
        features = crate_features,
        named_deps = named_deps,
        resources = _resources(package_compile_data, extra_compile_data),
        rustc_flags = _rustc_flags(package_name, declared_features),
        labels = tags,
        visibility = ["PUBLIC"],
        **(source_attrs | run_attrs)
    )

def lash_rust_feature_test(
        name,
        package_name,
        pruned_deps = [],
        variant_deps = {},
        extra_deps = {},
        library = None,
        library_crate_name = None,
        **kwargs):
    deps = _variant_named_deps(package_name, True, pruned_deps, variant_deps, extra_deps, library, library_crate_name)
    _rust_test(
        name,
        kwargs.pop("crate_name"),
        kwargs.pop("crate_root"),
        kwargs.pop("crate_features"),
        kwargs.pop("declared_features"),
        kwargs.pop("manifest_dir"),
        package_name,
        kwargs.pop("version"),
        kwargs.pop("args", []),
        kwargs.pop("build_script", None),
        kwargs.pop("exec_properties", {}),
        kwargs.pop("srcs_patterns", ["src/**/*.rs", "tests/**/*.rs", "shared/**/*.rs"]),
        kwargs.pop("data_exclude", []),
        kwargs.pop("extra_compile_data", []),
        kwargs.pop("extra_data", []),
        None,
        None,
        kwargs.pop("rustc_env", {}),
        kwargs.pop("shard_count", 0),
        kwargs.pop("test_env", {}),
        kwargs.pop("tags", []),
        kwargs.pop("timeout", None),
        deps,
    )
