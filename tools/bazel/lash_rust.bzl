"""Small rules_rs wrappers for Cargo-shaped first-party Lash targets."""

load("@crates//:defs.bzl", "aliases", "all_crate_deps", "lint_config")
load("@rules_rs//rs:rust_binary.bzl", "rust_binary")
load("@rules_rs//rs:rust_library.bzl", "rust_library")
load("@rules_rs//rs:rust_test.bzl", "rust_test")
load("@rules_rust//cargo:defs.bzl", "cargo_build_script")
load("@rules_rust//rust:defs.bzl", "rust_doc_test")

_IGNORED_FILES = [
    "BUILD",
    "BUILD.bazel",
]

def _cargo_env(package_name, manifest_dir, version, extra = {}):
    env = {
        "CARGO_MANIFEST_DIR": manifest_dir,
        "CARGO_PKG_NAME": package_name,
        "CARGO_PKG_VERSION": version,
    }
    env.update(extra)
    return env

def _all_package_files():
    return native.glob(
        ["**"],
        allow_empty = True,
        exclude = _IGNORED_FILES,
        exclude_directories = 1,
    )

def _compile_data():
    return native.glob(
        ["**"],
        allow_empty = True,
        exclude = _IGNORED_FILES + ["**/*.rs"],
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
        extra_compile_data = []):
    deps = all_crate_deps(normal = True)
    if build_script:
        deps = deps + [build_script]
    rust_library(
        name = name,
        aliases = _aliases_for(deps),
        compile_data = _compile_data() + extra_compile_data,
        crate_features = crate_features,
        crate_name = crate_name,
        crate_root = "src/lib.rs",
        data = _all_package_files() + extra_compile_data,
        deps = deps,
        edition = "2024",
        lint_config = lint_config(),
        rustc_env = _cargo_env(package_name, manifest_dir, version),
        rustc_flags = _cargo_check_cfg(declared_features),
        srcs = native.glob(
            ["src/**/*.rs", "shared/**/*.rs"],
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
        include_dev_deps = False,
        library = None,
        library_crate_name = None,
        extra_compile_data = [],
        rustc_env = {},
        tags = []):
    deps = all_crate_deps(normal = True, normal_dev = include_dev_deps)
    if library:
        deps = deps + [library]
    rust_binary(
        name = name,
        aliases = _aliases_for(deps, library, library_crate_name),
        compile_data = _compile_data() + extra_compile_data,
        crate_features = crate_features,
        crate_name = crate_name,
        crate_root = crate_root,
        data = _all_package_files() + extra_compile_data,
        deps = deps,
        edition = "2024",
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
        build_script = None,
        extra_compile_data = [],
        library = None,
        library_crate_name = None,
        test_env = {},
        tags = []):
    deps = all_crate_deps(normal = True, normal_dev = True)
    if build_script:
        deps = deps + [build_script]
    if library:
        deps = deps + [library]
    rust_test(
        name = name,
        aliases = _aliases_for(deps, library, library_crate_name),
        compile_data = _compile_data() + extra_compile_data,
        crate_features = crate_features,
        crate_name = crate_name,
        crate_root = crate_root,
        data = _all_package_files() + extra_compile_data,
        deps = deps,
        edition = "2024",
        env = _test_env(test_env),
        lint_config = lint_config(),
        rustc_env = _cargo_env(package_name, manifest_dir, version),
        rustc_flags = _cargo_check_cfg(declared_features),
        srcs = _crate_srcs(
            crate_root,
            ["src/**/*.rs", "tests/**/*.rs", "shared/**/*.rs"],
        ),
        tags = tags,
        version = version,
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
        library = None,
        library_crate_name = None,
        extra_compile_data = [],
        extra_data = [],
        rustc_env = {},
        test_env = {},
        tags = []):
    deps = all_crate_deps(normal = True, normal_dev = True)
    if library:
        deps = deps + [library]
    rust_test(
        name = name,
        aliases = _aliases_for(deps, library, library_crate_name),
        compile_data = _compile_data() + extra_compile_data,
        crate_features = crate_features,
        crate_name = crate_name,
        crate_root = crate_root,
        data = _all_package_files() + extra_compile_data + extra_data,
        deps = deps,
        edition = "2024",
        env = _test_env(test_env),
        lint_config = lint_config(),
        rustc_env = _cargo_env(package_name, manifest_dir, version, rustc_env),
        rustc_flags = _cargo_check_cfg(declared_features),
        srcs = _crate_srcs(
            crate_root,
            ["src/**/*.rs", "tests/**/*.rs", "examples/**/*.rs", "shared/**/*.rs"],
        ),
        tags = tags,
        version = version,
    )

def lash_rust_doc_test(name, crate):
    rust_doc_test(
        name = name,
        crate = crate,
        tags = ["cargo-authoritative-doctest", "manual"],
    )
