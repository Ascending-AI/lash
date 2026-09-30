load("@prelude//rust:rust_toolchain.bzl", "PanicRuntime", "RustToolchainInfo")

def _rust(ctx):
    tree = ctx.actions.copied_dir("rust", ctx.attrs.srcs)
    return [
        DefaultInfo(),
        RustToolchainInfo(
            compiler = RunInfo(args = cmd_args(tree.project("bin/rustc"), hidden = tree)),
            rustdoc = RunInfo(args = cmd_args(tree.project("bin/rustdoc"), hidden = tree)),
            clippy_driver = RunInfo(args = cmd_args(tree.project("bin/clippy-driver"), hidden = tree)),
            # Expose the declared sysroot tree to rules that invoke rustc
            # directly, and make ordinary prelude actions spell the same
            # explicit sysroot instead of discovering it from the executable.
            sysroot_path = tree,
            default_edition = "2024",
            panic_runtime = PanicRuntime("unwind"),
            # Match the captured ordinary build actions rather than Cargo's dev profile.
            # Keep this baseline on the toolchain so it covers first-party,
            # third-party, build-script, and proc-macro target compiles alike.
            rustc_flags = [
                "-Copt-level=0",
                "-Cdebuginfo=0",
                "-Cstrip=none",
                "-Cembed-bitcode=no",
            ],
            # Profile flags live on repository rules so Cargo's per-package
            # overrides retain their last-wins ordering. Host rules enter the
            # explicit profile_host configuration through host_transition.
            extra_rustc_flags = [],
            # Lash uses stable rustc with RUSTC_BOOTSTRAP for
            # hollow-rlib metadata pipelining. Buck's prelude gates the same
            # -Zno-codegen pipeline on this field.
            nightly_features = True,
            rustc_env = {"PATH": "/usr/bin:/bin"},
            rustc_target_triple = "x86_64-unknown-linux-gnu",
            doctests = False,
        ),
    ]

rust_toolchain = rule(
    impl = _rust,
    attrs = {"srcs": attrs.dict(attrs.string(), attrs.source())},
    is_toolchain_rule = True,
)
