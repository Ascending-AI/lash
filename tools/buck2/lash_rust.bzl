"""Cargo-shaped first-party Rust macros for the generated Lash graph."""

load("@prelude//rust:cargo_buildscript.bzl", "buildscript_run")
load(":buildscript_manifest.bzl", "buildscript_manifest", "buildscript_sources")
load(":clippy_policy.bzl", "FIRST_PARTY_CLIPPY_LINT_FLAGS", "FIRST_PARTY_RUST_LINT_FLAGS", "first_party_clippy_configuration")
load(":deps.bzl", "PACKAGE_DEPS")
load(":platforms.bzl", "pool_constraint")
load(":profile.bzl", "FIRST_PARTY_OPT_LEVELS")
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
        "CARGO_MANIFEST_DIR": ".",
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

def _resource_attrs(exec_properties):
    cpu, memory = _request(exec_properties)
    return {
        "exec_compatible_with": [pool_constraint(cpu, memory)],
        "kiln_action_cpu_count": str(cpu),
        "kiln_action_memory_kb": str(memory),
    }

def _named_deps(package_name, include_dev = False, build = False):
    groups = PACKAGE_DEPS[package_name]
    result = dict(groups["build"] if build else groups["normal"])
    if include_dev:
        result.update(groups["dev"])
    return result

def _variant_named_deps(package_name, include_dev, variant_deps, extra_deps, library, library_crate_name):
    result = _named_deps(package_name, include_dev = include_dev)
    for extern, label in result.items():
        result[extern] = variant_deps.get(label, label)
    for label, extern in extra_deps.items():
        result[extern] = label
    if library:
        result[library_crate_name] = library
    return result

def _srcs(crate_root, patterns):
    return [crate_root] + glob(patterns, exclude = _IGNORED + [crate_root])

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
    native.rust_binary(
        name = binary,
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = "build_script_build",
        crate_root = "build.rs",
        edition = "2024",
        env = _cargo_env(package_name, "build_script_build", manifest_dir, version),
        features = crate_features,
        named_deps = _named_deps(package_name, build = True),
        rustc_flags = _rustc_flags(package_name, declared_features),
        incoming_transition = _HOST_TRANSITION,
        srcs = _srcs("build.rs", ["build/**/*.rs"]) + data + extra_srcs,
        visibility = ["PUBLIC"],
        **_resource_attrs({})
    )
    cpu, memory = _request({})
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
    compile_data = _data(compile_data_patterns) + extra_compile_data
    native.rust_library(
        name = name,
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = crate_name,
        crate_root = "src/lib.rs",
        edition = "2024",
        env = env,
        features = crate_features,
        named_deps = _named_deps(package_name),
        resources = compile_data,
        rustc_flags = flags,
        srcs = glob(["src/**/*.rs", "shared/**/*.rs"], exclude = _IGNORED + test_srcs) + compile_data,
        visibility = ["PUBLIC"],
        **_resource_attrs(exec_properties)
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
        rustc_env = {},
        tags = []):
    deps = _named_deps(package_name, include_dev = include_dev_deps)
    if library:
        deps[library_crate_name] = library
    compile_data = _data(compile_data_patterns, data_exclude) + extra_compile_data
    native.rust_binary(
        name = name,
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = crate_name,
        crate_root = crate_root,
        edition = "2024",
        env = _cargo_env(package_name, crate_name, manifest_dir, version, rustc_env),
        features = crate_features,
        named_deps = deps,
        resources = compile_data,
        rustc_flags = _rustc_flags(package_name, declared_features),
        srcs = _srcs(crate_root, ["src/**/*.rs", "examples/**/*.rs", "benches/**/*.rs", "shared/**/*.rs"]) + compile_data,
        labels = tags,
        visibility = ["PUBLIC"],
        **_resource_attrs(exec_properties)
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
    native.rust_test(
        name = binary,
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = crate_name,
        crate_root = crate_root,
        edition = "2024",
        env = compile_env,
        features = crate_features,
        named_deps = named_deps,
        resources = package_files + extra_compile_data + extra_data,
        rustc_flags = flags,
        srcs = _srcs(crate_root, srcs_patterns) + package_data + extra_compile_data,
        labels = ["lash.internal_test_binary"],
        visibility = [],
        **_resource_attrs(exec_properties)
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

def lash_rust_doc(name, crate, **_kwargs):
    native.alias(name = name, actual = crate + "[doc]", visibility = ["PUBLIC"])

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
        variant_deps = {},
        extra_deps = {},
        tags = []):
    env, flags = _buildscript_args(
        build_script,
        _cargo_env(package_name, crate_name, manifest_dir, version),
        _rustc_flags(package_name, declared_features),
    )
    compile_data = _data(compile_data_patterns) + extra_compile_data
    native.rust_library(
        name = name,
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = crate_name,
        crate_root = "src/lib.rs",
        edition = "2024",
        env = env,
        features = crate_features,
        named_deps = _variant_named_deps(package_name, False, variant_deps, extra_deps, None, None),
        resources = compile_data,
        rustc_flags = flags,
        srcs = glob(["src/**/*.rs", "shared/**/*.rs"], exclude = _IGNORED + test_srcs) + compile_data,
        labels = tags,
        visibility = ["PUBLIC"],
        **_resource_attrs(exec_properties)
    )

def lash_rust_feature_binary(
        name,
        package_name,
        include_dev_deps = False,
        variant_deps = {},
        extra_deps = {},
        library = None,
        library_crate_name = None,
        **kwargs):
    # Keep the public signature stable; ordinary binary emission is reused
    # after replacing the generated dependency table for this call.
    deps = _variant_named_deps(package_name, include_dev_deps, variant_deps, extra_deps, library, library_crate_name)
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
    compile_data_patterns = kwargs.pop("compile_data_patterns", [])
    data_exclude = kwargs.pop("data_exclude", [])
    compile_data = _data(compile_data_patterns, data_exclude) + extra_compile_data
    native.rust_binary(
        name = name,
        clippy_configuration = first_party_clippy_configuration(manifest_dir),
        crate = crate_name,
        crate_root = crate_root,
        edition = "2024",
        env = _cargo_env(package_name, crate_name, manifest_dir, version, rustc_env),
        features = crate_features,
        named_deps = named_deps,
        resources = compile_data,
        rustc_flags = _rustc_flags(package_name, declared_features),
        srcs = _srcs(crate_root, ["src/**/*.rs", "examples/**/*.rs", "benches/**/*.rs", "shared/**/*.rs"]) + compile_data,
        labels = tags,
        visibility = ["PUBLIC"],
        **_resource_attrs(exec_properties)
    )

def lash_rust_feature_test(
        name,
        package_name,
        variant_deps = {},
        extra_deps = {},
        library = None,
        library_crate_name = None,
        **kwargs):
    deps = _variant_named_deps(package_name, True, variant_deps, extra_deps, library, library_crate_name)
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
        0,
        kwargs.pop("test_env", {}),
        kwargs.pop("tags", []),
        kwargs.pop("timeout", None),
        deps,
    )
