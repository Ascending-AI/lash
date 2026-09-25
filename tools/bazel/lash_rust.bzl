"""Small rules_rs wrappers for Cargo-shaped first-party Lash targets."""

load("@crates//:defs.bzl", "aliases", "all_crate_deps", "lint_config")
load("@rules_rs//rs:rust_binary.bzl", "rust_binary")
load("@rules_rs//rs:rust_library.bzl", "rust_library")
load("@rules_rs//rs:rust_test.bzl", "rust_test")
load("@rules_rust//cargo:defs.bzl", "cargo_build_script")

# Every remote action carries a `memory_kb` and a `cpu_count` request. The
# repository default in `.bazelrc` is the small action; a target that needs more
# says so through `exec_properties`, which a generated BUILD file resolves
# through `sized_exec_properties` in `tools/bazel/exec_sizes.bzl` -- the
# generated table carries the numbers, so a re-measured row never rewrites a
# crate's file. Plain keys size the target's compile actions (from the
# measured table in `tools/bazel/action-sizes.json`), and on a test target
# `test.cpu_count` / `test.memory_kb` size its run: Bazel applies
# `test.`-prefixed properties to the TestRunner spawn only. Per-target
# properties merge with the remote defaults.

_IGNORED_FILES = [
    "BUILD",
    "BUILD.bazel",
    "**/__pycache__/**",
    "**/node_modules/**",
]

def _cargo_env(package_name, manifest_dir, version, extra = {}):
    env = {
        "CARGO_MANIFEST_DIR": manifest_dir,
        "CARGO_PKG_NAME": package_name,
        "CARGO_PKG_VERSION": version,
    }
    env.update(extra)
    return env

# Non-Rust inputs are declared, not globbed wholesale. A library or binary
# compiles in only the package files `tools/bazel/source-ownership.json` lists
# under `compile_data` (none by default), so a snapshot, trybuild pin or
# fixture edit does not rebuild the crate and everything above it. Test targets
# see every package file except those another test target of the package owns
# (`test_data`), which arrive here as `data_exclude`.

def _all_package_files(exclude = []):
    return native.glob(
        ["**"],
        allow_empty = True,
        exclude = _IGNORED_FILES + exclude,
        exclude_directories = 1,
    )

def _compile_data(patterns = ["**"], exclude = []):
    return native.glob(
        patterns,
        allow_empty = True,
        exclude = _IGNORED_FILES + ["**/*.rs"] + exclude,
        exclude_directories = 1,
    )

def _crate_srcs(crate_root, patterns):
    """Declares the crate root even when it lives outside the usual source tree."""
    return [crate_root] + native.glob(
        patterns,
        allow_empty = True,
        exclude = [crate_root],
    )

def _cargo_check_cfg(declared_features):
    feature_values = ",".join(['"{}"'.format(feature) for feature in declared_features])
    return [
        "--check-cfg=cfg(docsrs,test)",
        "--check-cfg=cfg(feature,values({}))".format(feature_values),
    ]

def _sharding(shard_count):
    """Splits one libtest binary across `shard_count` parallel test actions.

    `rust_test`'s sharding wrapper enumerates the binary with `--list`, sorts
    the names and assigns each to a shard by a stable name hash, so the shards
    execute disjoint subsets whose union is every case the unsharded binary
    runs. It is opt-in because each shard pays its own `--list` execution and
    runfiles tree; only a binary whose wall time dominates a partition earns
    one.
    """
    if shard_count <= 0:
        return {}
    return {
        "experimental_enable_sharding": True,
        "shard_count": shard_count,
    }

def _test_env(extra):
    # Insta otherwise shells out to Cargo to discover the workspace and then
    # resolves snapshots from the package path twice inside Bazel runfiles.
    result = {"INSTA_WORKSPACE_ROOT": "."}
    result.update(extra)
    return result

def _aliases_for(deps, library = None, library_crate_name = None):
    result = {
        label: crate_name
        for label, crate_name in aliases().items()
        if label in deps
    }
    if library:
        result[library] = library_crate_name
    return result

def lash_rust_build_script(
        name,
        crate_features,
        declared_features,
        manifest_dir,
        package_name,
        version,
        data = []):
    cargo_build_script(
        name = name,
        aliases = _aliases_for(all_crate_deps(build = True)),
        crate_features = crate_features,
        crate_name = "build_script_build",
        crate_root = "build.rs",
        data = data,
        deps = all_crate_deps(build = True),
        edition = "2024",
        pkg_name = package_name,
        rustc_env = _cargo_env(package_name, manifest_dir, version),
        rustc_flags = _cargo_check_cfg(declared_features),
        srcs = ["build.rs"],
        version = version,
        visibility = ["//visibility:public"],
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
    deps = all_crate_deps(normal = True)
    if build_script:
        deps = deps + [build_script]
    rust_library(
        name = name,
        aliases = _aliases_for(deps),
        compile_data = _compile_data(compile_data_patterns) + extra_compile_data,
        crate_features = crate_features,
        crate_name = crate_name,
        crate_root = "src/lib.rs",
        data = _compile_data(compile_data_patterns) + extra_compile_data,
        deps = deps,
        edition = "2024",
        exec_properties = exec_properties,
        lint_config = lint_config(),
        rustc_env = _cargo_env(package_name, manifest_dir, version),
        rustc_flags = _cargo_check_cfg(declared_features),
        srcs = native.glob(
            ["src/**/*.rs", "shared/**/*.rs"],
            exclude = test_srcs,
            allow_empty = True,
        ),
        version = version,
        visibility = ["//visibility:public"],
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
    deps = all_crate_deps(normal = True, normal_dev = include_dev_deps)
    if library:
        deps = deps + [library]
    rust_binary(
        name = name,
        aliases = _aliases_for(deps, library, library_crate_name),
        compile_data = _compile_data(compile_data_patterns) + extra_compile_data,
        crate_features = crate_features,
        crate_name = crate_name,
        crate_root = crate_root,
        data = _compile_data(exclude = data_exclude) + extra_compile_data,
        deps = deps,
        edition = "2024",
        exec_properties = exec_properties,
        lint_config = lint_config(),
        rustc_env = _cargo_env(package_name, manifest_dir, version, rustc_env),
        rustc_flags = _cargo_check_cfg(declared_features),
        srcs = _crate_srcs(
            crate_root,
            [
                "src/**/*.rs",
                "examples/**/*.rs",
                "benches/**/*.rs",
                "shared/**/*.rs",
            ],
        ),
        tags = tags,
        version = version,
        visibility = ["//visibility:public"],
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
    deps = all_crate_deps(normal = True, normal_dev = True)
    if build_script:
        deps = deps + [build_script]
    if library:
        deps = deps + [library]
    rust_test(
        name = name,
        args = args,
        aliases = _aliases_for(deps, library, library_crate_name),
        compile_data = _compile_data(exclude = data_exclude) + extra_compile_data,
        crate_features = crate_features,
        crate_name = crate_name,
        crate_root = crate_root,
        data = _all_package_files(data_exclude) + extra_compile_data + extra_data,
        deps = deps,
        edition = "2024",
        env = _test_env(test_env),
        exec_properties = exec_properties,
        lint_config = lint_config(),
        rustc_env = _cargo_env(package_name, manifest_dir, version),
        rustc_flags = _cargo_check_cfg(declared_features),
        srcs = _crate_srcs(
            crate_root,
            srcs_patterns,
        ),
        tags = tags,
        timeout = timeout,
        version = version,
        **_sharding(shard_count)
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
    deps = all_crate_deps(normal = True, normal_dev = True)
    if library:
        deps = deps + [library]
    rust_test(
        name = name,
        aliases = _aliases_for(deps, library, library_crate_name),
        args = args,
        compile_data = _compile_data(exclude = data_exclude) + extra_compile_data,
        crate_features = crate_features,
        crate_name = crate_name,
        crate_root = crate_root,
        data = _all_package_files(data_exclude) + extra_compile_data + extra_data,
        deps = deps,
        edition = "2024",
        env = _test_env(test_env),
        exec_properties = exec_properties,
        lint_config = lint_config(),
        rustc_env = _cargo_env(package_name, manifest_dir, version, rustc_env),
        rustc_flags = _cargo_check_cfg(declared_features),
        srcs = _crate_srcs(
            crate_root,
            srcs_patterns,
        ),
        tags = tags,
        version = version,
        **_sharding(shard_count)
    )

# -- feature-lane variants ---------------------------------------------------
#
# `scripts/feature-coverage.toml` declares Cargo commands that resolve one
# package's closure differently from the workspace graph -- `-p X
# --no-default-features --features Y` gives every first-party dependency only
# what X's request implies. `tools/bazel/generate_build_files.py` reproduces
# that resolution (`tools/bazel/feature_variants.py`) and emits one variant
# target per distinct `(package, resolved features, Cargo target kind)`.
#
# A variant differs from the ordinary target in exactly two ways: its
# `crate_features` is the command's resolution rather than the workspace's, and
# each first-party dependency label is swapped for that dependency's variant at
# its own resolved feature set. `variant_deps` carries that swap, and the alias
# the ordinary label carried moves with it -- the crate name a source writes
# (`lash_core_ids`) is a property of the dependency edge, not of the label.
#
# Third-party crates are NOT re-resolved: `crate.from_cargo` pins `@crates`
# from one `//:Cargo.toml` + `//:Cargo.lock` resolution, so every variant links
# the same third-party feature union the workspace build uses. That union is a
# superset of what Cargo would resolve for the command, so a variant compiles
# against at least the API Cargo offers it; the Cargo lane, which is the one
# that can observe a narrowed third-party API, is retained on untrusted events.
# `docs/agents/hermetic-build.md` records the limitation in full.

def _variant_deps(
        deps,
        variant_deps,
        extra_deps = {},
        library = None,
        library_crate_name = None):
    """Swaps first-party dependency labels for their variants, aliases included.

    `extra_deps` maps a third-party label to the extern name the sources write.
    It carries the optional dependencies a variant's features activate that the
    workspace resolution leaves off -- `opentelemetry` under `lash-trace/otel`,
    say. `all_crate_deps` reports the workspace resolution's dependency list
    and nothing else, so without this the variant would compile with the
    feature on and the crate absent.
    """
    declared = aliases()
    swapped = []
    swapped_aliases = {}
    for dep in deps:
        replacement = variant_deps.get(dep, dep)
        swapped.append(replacement)
        if dep in declared:
            swapped_aliases[replacement] = declared[dep]
    for label, extern_name in extra_deps.items():
        if label in swapped:
            continue
        swapped.append(label)
        swapped_aliases[label] = extern_name
    if library:
        swapped.append(library)
        swapped_aliases[library] = library_crate_name
    return swapped, swapped_aliases

def lash_rust_feature_library(
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
        extra_compile_data = [],
        extra_deps = {},
        tags = [],
        variant_deps = {}):
    deps, dep_aliases = _variant_deps(
        all_crate_deps(normal = True),
        variant_deps,
        extra_deps,
    )
    if build_script:
        deps = deps + [build_script]
    rust_library(
        name = name,
        aliases = dep_aliases,
        compile_data = _compile_data(compile_data_patterns) + extra_compile_data,
        crate_features = crate_features,
        crate_name = crate_name,
        crate_root = "src/lib.rs",
        data = _compile_data(compile_data_patterns) + extra_compile_data,
        deps = deps,
        edition = "2024",
        exec_properties = exec_properties,
        lint_config = lint_config(),
        rustc_env = _cargo_env(package_name, manifest_dir, version),
        rustc_flags = _cargo_check_cfg(declared_features),
        srcs = native.glob(
            ["src/**/*.rs", "shared/**/*.rs"],
            exclude = test_srcs,
            allow_empty = True,
        ),
        tags = tags,
        version = version,
        visibility = ["//visibility:public"],
    )

def lash_rust_feature_binary(
        name,
        crate_name,
        crate_root,
        crate_features,
        declared_features,
        manifest_dir,
        package_name,
        version,
        exec_properties = {},
        extra_deps = {},
        include_dev_deps = False,
        library = None,
        library_crate_name = None,
        compile_data_patterns = [],
        data_exclude = [],
        extra_compile_data = [],
        rustc_env = {},
        tags = [],
        variant_deps = {}):
    deps, dep_aliases = _variant_deps(
        all_crate_deps(normal = True, normal_dev = include_dev_deps),
        variant_deps,
        extra_deps,
        library,
        library_crate_name,
    )
    rust_binary(
        name = name,
        aliases = dep_aliases,
        compile_data = _compile_data(compile_data_patterns) + extra_compile_data,
        crate_features = crate_features,
        crate_name = crate_name,
        crate_root = crate_root,
        data = _compile_data(exclude = data_exclude) + extra_compile_data,
        deps = deps,
        edition = "2024",
        exec_properties = exec_properties,
        lint_config = lint_config(),
        rustc_env = _cargo_env(package_name, manifest_dir, version, rustc_env),
        rustc_flags = _cargo_check_cfg(declared_features),
        srcs = _crate_srcs(
            crate_root,
            [
                "src/**/*.rs",
                "examples/**/*.rs",
                "benches/**/*.rs",
                "shared/**/*.rs",
            ],
        ),
        tags = tags,
        version = version,
        visibility = ["//visibility:public"],
    )

def lash_rust_feature_test(
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
        data_exclude = [],
        extra_compile_data = [],
        extra_data = [],
        extra_deps = {},
        library = None,
        library_crate_name = None,
        rustc_env = {},
        srcs_patterns = ["src/**/*.rs", "tests/**/*.rs", "shared/**/*.rs"],
        test_env = {},
        tags = [],
        timeout = None,
        variant_deps = {}):
    deps, dep_aliases = _variant_deps(
        all_crate_deps(normal = True, normal_dev = True),
        variant_deps,
        extra_deps,
        library,
        library_crate_name,
    )
    if build_script:
        deps = deps + [build_script]
    rust_test(
        name = name,
        args = args,
        aliases = dep_aliases,
        compile_data = _compile_data(exclude = data_exclude) + extra_compile_data,
        crate_features = crate_features,
        crate_name = crate_name,
        crate_root = crate_root,
        data = _all_package_files(data_exclude) + extra_compile_data + extra_data,
        deps = deps,
        edition = "2024",
        env = _test_env(test_env),
        exec_properties = exec_properties,
        lint_config = lint_config(),
        rustc_env = _cargo_env(package_name, manifest_dir, version) | rustc_env,
        rustc_flags = _cargo_check_cfg(declared_features),
        srcs = _crate_srcs(crate_root, srcs_patterns),
        tags = tags,
        timeout = timeout,
        version = version,
    )
