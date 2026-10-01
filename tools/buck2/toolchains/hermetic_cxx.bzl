load("@prelude//cxx:cxx_toolchain_types.bzl", "LinkerType")
load("@prelude//toolchains:cxx.bzl", "CxxToolsInfo")

def _tools(ctx):
    # The published LLVM archive uses one multicall binary behind the named
    # clang/clang++/llvm-ar symlinks.  Preserve each symlink's basename for
    # dispatch and ship the complete tree as a hidden action input so the
    # symlink target and Clang resource directory are present remotely.
    llvm = ctx.attrs.llvm_tree[DefaultInfo].default_outputs[0]
    clang = cmd_args(llvm.project("bin/clang"), hidden = llvm)
    clangxx = cmd_args(llvm.project("bin/clang++"), hidden = llvm)
    llvm_ar = cmd_args(llvm.project("bin/llvm-ar"), hidden = llvm)
    return [
        DefaultInfo(),
        CxxToolsInfo(
            compiler = clang,
            compiler_type = "clang",
            cxx_compiler = clangxx,
            asm_compiler = clang,
            asm_compiler_type = "clang",
            rc_compiler = None,
            cvtres_compiler = None,
            archiver = llvm_ar,
            archiver_type = "gnu",
            linker = clangxx,
            linker_type = LinkerType("gnu"),
        ),
    ]

hermetic_cxx_tools = rule(
    impl = _tools,
    attrs = {
        "llvm_tree": attrs.exec_dep(),
    },
)
