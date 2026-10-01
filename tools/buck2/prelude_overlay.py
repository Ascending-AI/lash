#!/usr/bin/env python3
"""Apply and verify Lash's bounded stock-Buck2 Rust action overlay.

Buck2 2026-09-15 places execution properties in REAPI ``Command.platform``.
NativeLink schedules from those values, while its worker-side cgroup bridge
reads the same canonical values from ``Command.env``.  This overlay threads
the two resource attributes carried by repository Rust rules to every action
created by the pinned Rust prelude.  It also makes requested Clippy subtargets
fail when Clippy reports an error, matching the previous gate contract.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys


INPUT_SHA256 = {
    "decls/rust_rules.bzl": "de5eaa88d341e1b2fdc067753c5d093b4bf87fd1f2d015200a91d5d231a273d2",
    "rust/build.bzl": "2a221551779f2e4668ec3e7acac0e97fc9d557126898a6d32843647bf19546ea",
    "rust/cargo_buildscript.bzl": "21b492f83743c9a16e76f6c2d97752cd06769ed5067bd89f908118cabc72d504",
    "rust/clippy_configuration.bzl": "ed753d9a55df3a21251be99be250749a9e0156f9bca11302030559fb0b49f775",
    "rust/failure_filter.bzl": "b53d328b53f9b319784d121973c95dc63516a1f98c217859bf9151876465270c",
    "rust/link_info.bzl": "29abbee39d84703974a52ed98fd0221be642ce45459a7db98456f13a4b6755e7",
    "rust/named_deps.bzl": "ca62f61bf878d994048c4844869f9d7e47e0b6a4d3022614a066624ea303241d",
    "rust/profile.bzl": "271c871a774952767253025d88cf3c550566ddd5ea23c9f7ded8791662034f26",
    "rust/rust_binary.bzl": "75125cd6c3bbefc14ee1c654da2f785919b14b14d74d24f22a117b84b00cbf55",
    "rust/rust_library.bzl": "17e84e3f3f608e07ac3a9365b354181d79e35510fe3bc6c8a7ddf8a1b12158fe",
    "rust/sources.bzl": "a291683f417e408a6f7ba012dff370e83f859050c31b8a5a30dd2132f01e27b2",
    "rust/tools/tool_rules.bzl": "54b9a3ea4cfd04e5a2619d3e93c0fe338c8d67cab06f20a94674e46e2dc65b43",
    "rust/tools/BUCK": "6cfb02281c0addcb59659ba3db612fbf1ae6896ffbf7dd524be5610d8bf3f6d2",
    "rust/tools/buildscript_run.py": "50007e576180b834bc35533a06947446de7dad70470afe19cb473ba3e219128a",
}

# Filled from the deterministic transform below.  These hashes make a second
# invocation a full verification, rather than trusting an on-disk receipt.
OUTPUT_SHA256 = {
    "decls/rust_rules.bzl": "97769fd0b4afced56705a34fd4f3e8404f8da78997d57fa161678b79c4ed82bc",
    "rust/build.bzl": "51f2f65902b39bb78ebe818d4484f959b6e6a95ce6a8f65ba1e844ebe34d28e6",
    "rust/cargo_buildscript.bzl": "ff69fa677037ce6414d80b326f0565168ced5e0a916d0e7f456df4cfc07420a8",
    "rust/clippy_configuration.bzl": "9f7db7c7c8e0f34d65e0a71f1eebfb36ffd8749e6061123cab71a46d548d16a2",
    "rust/failure_filter.bzl": "6ec035fcd09446d60560711c37532f8d749401c50e50767ac8eebcddcb2a9e03",
    "rust/link_info.bzl": "3ac50277c98282863c8be30a8fee1d9fc3fa294e363cc4374578ba2c97f46eec",
    "rust/named_deps.bzl": "894b0f405b7dfbe9380b3c671999efb70ac2209747ab87b8b2322d621c8f35d9",
    "rust/profile.bzl": "c76bcbf08bf1e2f9cf295c619504ff2d398f790dfc68abbe96bcfb35ed4c1e14",
    "rust/rust_binary.bzl": "15c839433910cd64edd2cf0e90666ec062e784aa3591c101a2090b123dc246f9",
    "rust/rust_library.bzl": "16e64e3a0326a8f9e373a037b5a51d99e1851e05a7dd3e1194c67877f103c6a0",
    "rust/sources.bzl": "5505f55458faba390a75f50118877d16f57f08a41a5924e7c901de51f74c1b69",
    "rust/tools/tool_rules.bzl": "447a2c751cbd20a29663d0c14d641f7055195ca874c9ddcff0dbfaac6f9ff590",
    "rust/tools/BUCK": "1e1f72a05eaab95e347fd69a4c396134017161a59e833ff215b1cbd3e48eb086",
    "rust/tools/buildscript_run.py": "6c6e7aff95ccfa9a022dce9cfb0391e635f2d760e34327eb72ae9cb4bb8ff51a",
    "rust/kiln_action_env.bzl": "7fb049e416a5d18b7e25d4324e83ed9f1b3bfa45009da3065b434d8e0c3ff630",
}

# Accepted outputs from the immediately preceding checked-in overlay. This is
# a narrow upgrade path for existing private state; arbitrary modified prelude
# files still fail closed.
PREVIOUS_OUTPUT_SHA256 = {
    "rust/build.bzl": {
        "3bed58d24563a0e9c4274d5c5530a9c18c600ee1b3e8e7f6bcddd3a482be16fa",
        "ac1bbf9c1a7084a9756af83edf8dfbbedb47269010d54149187cf1b1b6f56b34",
    },
    "rust/cargo_buildscript.bzl": {
        "49e261487744c64fda39e73f15a0c440fa4af8ae9a4cb6f4ec12bcb4257ff607",
        "ca0aa5435d5dfa26c4f1de95309bb2968f315e922ae3a0bb324e604b293d8e1f",
        "9a62bdd91096b79fd9a63ab20cd42cd68c6ea3b1decca4569278c2e7adc7b2a7",
    },
}

HELPER = '''"""Lash NativeLink cgroup environment for one prelude action."""

def kiln_action_env(ctx, existing = {}, rust_identity = False):
    if not hasattr(ctx.attrs, "kiln_action_cpu_count") or not hasattr(ctx.attrs, "kiln_action_memory_kb"):
        fail("Lash Rust action has no canonical kiln resource attributes")
    result = dict(existing)
    result["KILN_ACTION_CPU_COUNT"] = ctx.attrs.kiln_action_cpu_count
    result["KILN_ACTION_MEMORY_KB"] = ctx.attrs.kiln_action_memory_kb
    if rust_identity:
        if not hasattr(ctx.attrs, "env"):
            fail("Rust compiler action has no Cargo identity environment")
        for name in ["CARGO_CRATE_NAME", "CARGO_PKG_NAME"]:
            value = ctx.attrs.env.get(name)
            if value == None:
                fail("Rust compiler action is missing {}".format(name))
            result[name] = value
    return result
'''

RESOURCE_ATTRS = '''            "kiln_action_cpu_count": attrs.string(default = "1"),
            "kiln_action_memory_kb": attrs.string(default = "1572864"),
'''


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def add_load(text: str, relative: str) -> str:
    target = ":kiln_action_env.bzl" if relative.startswith("rust/") and "/" not in relative.removeprefix("rust/") else "//rust:kiln_action_env.bzl"
    load = 'load("{}", "kiln_action_env")\n'.format(target)
    if load in text:
        return text
    match = re.search(r"(?m)^load\(", text)
    if match is None:
        raise ValueError("file has no load statement")
    position = match.start()
    return text[:position] + load + text[position:]


def action_calls(text: str) -> list[tuple[int, int]]:
    calls = []
    start = 0
    needle = "ctx.actions.run("
    while True:
        begin = text.find(needle, start)
        if begin < 0:
            return calls
        index = begin + len(needle)
        depth = 1
        quote = None
        escaped = False
        while index < len(text) and depth:
            char = text[index]
            if quote is not None:
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == quote:
                    quote = None
            elif char in {'"', "'"}:
                quote = char
            elif char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
            index += 1
        if depth:
            raise ValueError("unterminated ctx.actions.run call")
        calls.append((begin, index))
        start = index


def wrap_actions(text: str) -> str:
    pieces = []
    cursor = 0
    for begin, end in action_calls(text):
        pieces.append(text[cursor:begin])
        call = text[begin:end]
        if "kiln_action_env(" in call:
            pieces.append(call)
            cursor = end
            continue
        if re.search(r"\benv\s*=\s*env\b", call):
            call = re.sub(r"\benv\s*=\s*env\b", "env = kiln_action_env(ctx, env)", call, count=1)
        elif "\n" in call:
            close = call.rfind(")")
            indent = re.search(r"(?m)^(\s*)\)\s*$", call[close - 20 :])
            # All pinned multiline calls close at the call site's indentation.
            line_start = call.rfind("\n", 0, close) + 1
            closing_indent = call[line_start:close]
            call = call[:line_start] + "    env = kiln_action_env(ctx),\n" + closing_indent + call[close:]
        else:
            close = call.rfind(")")
            call = call[:close] + ", env = kiln_action_env(ctx)" + call[close:]
        pieces.append(call)
        cursor = end
    pieces.append(text[cursor:])
    return "".join(pieces)


def add_attrs(text: str, anchor: str) -> str:
    if '"kiln_action_cpu_count"' in text:
        return text
    if anchor not in text:
        raise ValueError(f"attribute anchor absent: {anchor!r}")
    return text.replace(anchor, anchor + RESOURCE_ATTRS, 1)


def add_buildscript_metadata_bzl(text: str) -> str:
    text = text.replace(
        '''        rust_toolchain_info.rustc_flags,
        rust_toolchain_info.extra_rustc_flags,
        "-Cunsafe-allow-abi-mismatch=sanitizer",''',
        '''        rust_toolchain_info.rustc_flags,
        ctx.attrs.cargo_rustc_flags,
        rust_toolchain_info.extra_rustc_flags,
        "-Cunsafe-allow-abi-mismatch=sanitizer",''',
        1,
    )
    for prior_flags in (
        "rust_toolchain_info.rustc_flags + rust_toolchain_info.extra_rustc_flags",
        "rust_toolchain_info.rustc_flags",
    ):
        old = "    for flag in {}:\n".format(prior_flags)
        if old in text:
            text = text.replace(
                old,
                "    for flag in rust_toolchain_info.rustc_flags + ctx.attrs.cargo_rustc_flags + rust_toolchain_info.extra_rustc_flags:\n",
                1,
            )
            break
    if 'declare_output("METADATA"' not in text:
        text = text.replace(
            '''    rustc_flags = ctx.actions.declare_output("rustc_flags", has_content_based_path = True)
''',
            '''    rustc_flags = ctx.actions.declare_output("rustc_flags", has_content_based_path = True)
    metadata = ctx.actions.declare_output("METADATA", has_content_based_path = True)
''',
            1,
        )
    if '"--extra-env="' not in text:
        text = text.replace(
            '''        cmd_args("--outfile=", rustc_flags.as_output(), delimiter = ""),
    ]''',
            '''        cmd_args("--outfile=", rustc_flags.as_output(), delimiter = ""),
        cmd_args("--outenv=", metadata.as_output(), delimiter = ""),
    ]

    for env_src in ctx.attrs.env_srcs:
        cmd.append(cmd_args("--extra-env=", env_src[DefaultInfo].default_outputs[0], delimiter = ""))''',
            1,
        )
    if '"metadata": [DefaultInfo' not in text:
        text = text.replace(
            '''                "rustc_flags": [DefaultInfo(default_output = rustc_flags)],
            },''',
            '''                "rustc_flags": [DefaultInfo(default_output = rustc_flags, other_outputs = [out_dir])],
                "metadata": [DefaultInfo(default_output = metadata, other_outputs = [out_dir])],
            },''',
            1,
        )
    if '"env_srcs": attrs.list' not in text:
        text = text.replace(
            '''        "env": attrs.dict(key = attrs.string(), value = attrs.arg(), default = {}),
''',
            '''        "env": attrs.dict(key = attrs.string(), value = attrs.arg(), default = {}),
        "env_srcs": attrs.list(attrs.dep(), default = []),
''',
            1,
        )
    if '"cargo_rustc_flags": attrs.list' not in text:
        text = text.replace(
            '''        "env": attrs.dict(key = attrs.string(), value = attrs.arg(), default = {}),
''',
            '''        "env": attrs.dict(key = attrs.string(), value = attrs.arg(), default = {}),
        "cargo_rustc_flags": attrs.list(attrs.string(), default = []),
''',
            1,
        )
    for required in ('ctx.attrs.cargo_rustc_flags', 'declare_output("METADATA"', '"--extra-env="', '"metadata": [DefaultInfo', '"env_srcs": attrs.list'):
        if required not in text:
            raise ValueError("Cargo build-script metadata bridge insertion failed: {}".format(required))
    return text


def add_buildscript_metadata_runner(text: str) -> str:
    text = text.replace("import argparse\n", "import argparse\nimport json\n", 1)
    text = text.replace(
        '''    rustc_link_search: bool
''',
        '''    rustc_link_search: bool
    extra_env: list[str]
    outenv: IO[str]
''',
        1,
    )
    text = text.replace(
        '''    parser.add_argument("--rustc-link-search", action="store_true")
''',
        '''    parser.add_argument("--rustc-link-search", action="store_true")
    parser.add_argument("--extra-env", action="append", default=[])
    parser.add_argument("--outenv", type=argparse.FileType("w"), required=True)
''',
        1,
    )
    text = text.replace(
        '''    env = cfg_env(args.rustc_cfg)

    out_dir = os.getenv("OUT_DIR")''',
        '''    env = cfg_env(args.rustc_cfg)
    for extra_env_path in args.extra_env:
        source = Path(extra_env_path)
        with source.open(encoding="utf-8") as handle:
            extra = json.load(handle)
        upstream_out_dir = str(source.parent / "OUT_DIR")
        for key, value in extra.items():
            env[key] = value.replace(OUT_DIR_SENTINEL, upstream_out_dir)

    out_dir = os.getenv("OUT_DIR")''',
        1,
    )
    text = text.replace(
        '''    out_dir_abs = env["OUT_DIR"]
''',
        '''    cargo_metadata_pattern = re.compile("^cargo::metadata=(.+?)=(.*)")
    cargo_kv_pattern = re.compile("^cargo:([^:=]+)=(.*)")
    known_directives = {
        "rerun-if-changed",
        "rerun-if-env-changed",
        "rustc-cdylib-link-arg",
        "rustc-check-cfg",
        "rustc-link-arg",
        "rustc-link-arg-bin",
        "rustc-link-arg-bins",
        "rustc-link-arg-cdylib",
        "rustc-link-arg-examples",
        "rustc-link-arg-tests",
        "warning",
    }
    metadata = {}
    out_dir_abs = env["OUT_DIR"]
''',
        1,
    )
    text = text.replace(
        '''        print(line, end="\\n")
    args.outfile.write(flags)
''',
        '''        cargo_metadata_match = cargo_metadata_pattern.match(line)
        cargo_kv_match = cargo_kv_pattern.match(line)
        if cargo_metadata_match or (
            cargo_kv_match and cargo_kv_match.group(1) not in known_directives
        ):
            match = cargo_metadata_match or cargo_kv_match
            links = env.get("CARGO_MANIFEST_LINKS")
            if links:
                key = match.group(1).upper().replace("-", "_")
                value = match.group(2)
                reanchored = reanchor_out_dir(value)
                metadata["DEP_{}_{}".format(links.upper().replace("-", "_"), key)] = (
                    reanchored if reanchored is not None else value
                )
                continue
        print(line, end="\\n")
    args.outfile.write(flags)
    json.dump(metadata, args.outenv, indent=2, sort_keys=True)
''',
        1,
    )
    for required in ("extra_env: list[str]", '"--outenv"', "cargo_metadata_pattern", "json.dump(metadata"):
        if required not in text:
            raise ValueError("Cargo build-script metadata runner insertion failed: {}".format(required))
    return text


def preserve_relative_manifest_dir(text: str) -> str:
    anchor = """    return (plain_env, path_env)
"""
    replacement = """    # First-party Lash tests intentionally use the repository-relative Cargo
    # manifest directory as a stable runfiles path. The private marker keeps
    # third-party Cargo rules on the stock absolute-path behavior while
    # preventing a remote compile sandbox path from being embedded by env!().
    relative_manifest_dir = plain_env.pop("KILN_RELATIVE_CARGO_MANIFEST_DIR", None)
    if relative_manifest_dir:
        path_env.pop("CARGO_MANIFEST_DIR", None)
        plain_env["CARGO_MANIFEST_DIR"] = relative_manifest_dir

    return (plain_env, path_env)
"""
    if replacement in text:
        return text
    if text.count(anchor) != 1:
        raise ValueError("Rust process_env return changed")
    return text.replace(anchor, replacement, 1)


# Reindeer's generated package (tools/buck2/reindeer.toml `file_name`). Its
# crates compile from extracted `<name>-<version>.crate` archive directories.
THIRD_PARTY_PACKAGE = "third-party/rust"

CHECKOUT_SOURCES_PROJECTION = '''
# Lash: rust-lang/rust#153898 lets a dependency's source text change the crate
# hash of a dependent. Every Rust compile remaps its `__srcs` root to its owning
# package path and dependency metadata keeps only that remapped name, so rustc
# can find dependency text only at `<package>/<file>` relative to the action
# root. First-party sources exist there both in a local checkout and in a
# remote input root, and remain inputs. Extracted third-party archives are
# never at `{package}/<crate>.crate/`; shipping them to every dependent cannot
# change a hash and multiplied each archive's files into every input root.
def _get_checkout_artifacts(sources: Artifact) -> list[Artifact]:
    owner = sources.owner
    if owner != None and owner.package == "{package}":
        return []
    return [sources]

RustSourcesTSet = transitive_set(
    args_projections = {{
        "artifacts": _get_artifacts,
        "kiln_checkout_artifacts": _get_checkout_artifacts,
    }},
)
'''.format(package=THIRD_PARTY_PACKAGE)


def add_checkout_source_projection(text: str) -> str:
    old = '''
RustSourcesTSet = transitive_set(
    args_projections = {
        "artifacts": _get_artifacts,
    },
)
'''
    if CHECKOUT_SOURCES_PROJECTION in text:
        return text
    if text.count(old) != 1:
        raise ValueError("Rust sources transitive set changed")
    return text.replace(old, CHECKOUT_SOURCES_PROJECTION, 1)


def narrow_transitive_source_inputs(text: str) -> str:
    # Compiler and rustdoc actions take the checkout-resolvable projection. The
    # provider and its full `artifacts` projection stay stock for other users.
    for old, new in (
        (
            'hidden = compile_ctx.transitive_srcs.project_as_args("artifacts") if compile_ctx else [],',
            'hidden = compile_ctx.transitive_srcs.project_as_args("kiln_checkout_artifacts") if compile_ctx else [],',
        ),
        (
            'hidden = [toolchain_info.compiler, compile_ctx.transitive_srcs.project_as_args("artifacts")],',
            'hidden = [toolchain_info.compiler, compile_ctx.transitive_srcs.project_as_args("kiln_checkout_artifacts")],',
        ),
    ):
        if new in text:
            continue
        if text.count(old) != 1:
            raise ValueError("Rust compiler transitive source inputs changed")
        text = text.replace(old, new, 1)
    return text


def transform(relative: str, text: str) -> str:
    if relative == "rust/sources.bzl":
        return add_checkout_source_projection(text)
    if relative == "decls/rust_rules.bzl":
        return add_attrs(text, '            "clippy_configuration": attrs.option(attrs.dep(providers = [ClippyConfiguration]), default = None),\n')
    if relative in {"rust/rust_binary.bzl", "rust/rust_library.bzl"}:
        pattern = r'(emit = Emit\("clippy"\),(?:(?!\n\s*\)).)*?infallible_diagnostics = )True'
        text, count = re.subn(pattern, r"\g<1>False", text, flags=re.S)
        if count != 1:
            raise ValueError(f"expected one Clippy action in {relative}, found {count}")
        return text
    if relative == "rust/tools/BUCK":
        old_cfg = '''get_rustc_cfg(
    name = "rustc_cfg",
    visibility = ["PUBLIC"],
)'''
        new_cfg = '''get_rustc_cfg(
    name = "rustc_cfg",
    exec_compatible_with = ["root//tools/buck2:pool_1_1572864"],
    visibility = ["PUBLIC"],
)'''
        old_host = '''get_rustc_host_tuple(
    name = "rustc_host_tuple",
    visibility = ["PUBLIC"],
)'''
        new_host = '''get_rustc_host_tuple(
    name = "rustc_host_tuple",
    exec_compatible_with = ["root//tools/buck2:pool_1_1572864"],
    visibility = ["PUBLIC"],
)'''
        if old_cfg not in text or old_host not in text:
            raise ValueError("Rust helper target definitions changed")
        return text.replace(old_cfg, new_cfg, 1).replace(old_host, new_host, 1)
    if relative == "rust/tools/buildscript_run.py":
        return add_buildscript_metadata_runner(text)
    if relative == "rust/link_info.bzl":
        text = add_load(text, relative)
        signature = '''    shared_library_info: SharedLibraryInfo,
    shared_libs_symlink_tree_name_arg: str,'''
        text = text.replace(
            signature,
            '''    shared_library_info: SharedLibraryInfo,
    kiln_env: dict[str, str],
    shared_libs_symlink_tree_name_arg: str,''',
            1,
        )
        invocation = '''            shared_library_info,
            shared_libs_symlink_tree_name_arg,'''
        text = text.replace(
            invocation,
            '''            shared_library_info,
            kiln_action_env(ctx),
            shared_libs_symlink_tree_name_arg,''',
            1,
        )
        old = '''        category = "rust_shared_library_symlinks",
    )'''
        new = '''        category = "rust_shared_library_symlinks",
        env = kiln_env,
    )'''
        if old not in text:
            raise ValueError("shared-library symlink action changed")
        return text.replace(old, new, 1)
    text = add_load(text, relative)
    text = wrap_actions(text)
    if relative == "rust/build.bzl":
        text = preserve_relative_manifest_dir(text)
        text = narrow_transitive_source_inputs(text)
        old = '''        _lintify("W", is_clippy, toolchain_info.warn_lints),
    )'''
        new = '''        _lintify("W", is_clippy, toolchain_info.warn_lints),
        ["-Dwarnings"] if is_clippy else [],
    )'''
        if old not in text:
            raise ValueError("Rust lint flag ordering changed")
        text = text.replace(old, new, 1)
        text = text.replace(
            'ctx.actions.run(rustdoc_cmd, category = "rustdoc", env = kiln_action_env(ctx))',
            'ctx.actions.run(rustdoc_cmd, category = "rustdoc", env = kiln_action_env(ctx, rust_identity = True))',
            1,
        )
        compile_env = '''        error_handler = toolchain_info.rust_error_handler,
    env = kiln_action_env(ctx),
    )'''
        compile_identity = '''        error_handler = toolchain_info.rust_error_handler,
    env = kiln_action_env(ctx, rust_identity = True),
    )'''
        if compile_env not in text:
            raise ValueError("Rust compiler action environment changed")
        text = text.replace(compile_env, compile_identity, 1)
        old_cache = '''    elif is_clippy:
        # Clippy never uploads.
        action_allow_cache_upload = False'''
        new_cache = '''    elif is_clippy:
        # Lash makes Clippy fallible and treats its deterministic diagnostics
        # as a normal remote-cacheable gate.
        action_allow_cache_upload = True'''
        if old_cache not in text:
            raise ValueError("Clippy cache policy changed")
        text = text.replace(old_cache, new_cache, 1)
    if relative == "rust/cargo_buildscript.bzl":
        text = add_attrs(text, '        "buildscript": attrs.exec_dep(providers = [RunInfo]),\n')
        text = text.replace(
            '''        rust_toolchain_info.rustc_flags,
        "-Cunsafe-allow-abi-mismatch=sanitizer",''',
            '''        rust_toolchain_info.rustc_flags,
        rust_toolchain_info.extra_rustc_flags,
        "-Cunsafe-allow-abi-mismatch=sanitizer",''',
            1,
        )
        text = add_buildscript_metadata_bzl(text)
        text = text.replace(
            '''    for flag in rust_toolchain_info.rustc_flags:
        if isinstance(flag, ResolvedStringWithMacros):''',
            '''    for flag in rust_toolchain_info.rustc_flags + rust_toolchain_info.extra_rustc_flags:
        if isinstance(flag, ResolvedStringWithMacros):''',
            1,
        )
    elif relative == "rust/clippy_configuration.bzl":
        text = add_attrs(text, '        "clippy_toml_src": attrs.source(),\n')
        text = text.replace("clippy_configuration = rule(\n", "_clippy_configuration_rule = rule(\n", 1)
        text += '''

def clippy_configuration(name, **kwargs):
    _clippy_configuration_rule(
        name = name,
        exec_compatible_with = ["root//tools/buck2:pool_1_1572864"],
        **kwargs
    )
'''
    elif relative == "rust/tools/tool_rules.bzl":
        # Both tool rules own actions and therefore each needs the contract.
        first = '        "enable_nightly_cfgs": attrs.bool(default = False),\n'
        second = '        "_rust_toolchain": toolchains_common.rust(),\n'
        text = add_attrs(text, first)
        index = text.find(second, text.find("get_rustc_host_tuple = rule("))
        if index < 0:
            raise ValueError("host tuple tool attrs changed")
        text = text[: index + len(second)] + RESOURCE_ATTRS + text[index + len(second) :]
    return text


def upgrade_previous(relative: str, text: str) -> str:
    if relative == "rust/build.bzl":
        return narrow_transitive_source_inputs(preserve_relative_manifest_dir(text))
    if relative != "rust/cargo_buildscript.bzl":
        raise ValueError("no previous-overlay upgrade for {}".format(relative))
    encoded = '''        rust_toolchain_info.rustc_flags,
        "-Cunsafe-allow-abi-mismatch=sanitizer",'''
    scanned = '''    for flag in rust_toolchain_info.rustc_flags:
        if isinstance(flag, ResolvedStringWithMacros):'''
    if encoded in text:
        text = text.replace(
            encoded,
            '''        rust_toolchain_info.rustc_flags,
        rust_toolchain_info.extra_rustc_flags,
        "-Cunsafe-allow-abi-mismatch=sanitizer",''',
            1,
        )
    if scanned in text:
        text = text.replace(
            scanned,
            '''    for flag in rust_toolchain_info.rustc_flags + rust_toolchain_info.extra_rustc_flags:
        if isinstance(flag, ResolvedStringWithMacros):''',
            1,
        )
    return add_buildscript_metadata_bzl(text)


def expected_outputs(prelude: pathlib.Path) -> dict[str, bytes]:
    outputs = {"rust/kiln_action_env.bzl": HELPER.encode()}
    for relative, expected in INPUT_SHA256.items():
        data = (prelude / relative).read_bytes()
        actual = digest(data)
        if actual == OUTPUT_SHA256.get(relative):
            outputs[relative] = data
            continue
        if actual in PREVIOUS_OUTPUT_SHA256.get(relative, set()):
            outputs[relative] = upgrade_previous(relative, data.decode()).encode()
            continue
        if actual != expected:
            raise SystemExit(f"pinned prelude input changed: {relative}: {actual}")
        outputs[relative] = transform(relative, data.decode()).encode()
    return outputs


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--buck2", type=pathlib.Path, required=True)
    parser.add_argument("--prelude-dir", type=pathlib.Path, required=True)
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--print-output-hashes", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if not args.buck2.is_file():
        raise SystemExit("Buck2 executable must exist")
    if not args.prelude_dir.is_dir():
        if args.check:
            raise SystemExit("expanded prelude directory is absent")
        root = pathlib.Path(__file__).resolve().parents[2]
        config = root / ".buckconfig"
        original = config.read_text()
        temporary = original.replace("prelude = disabled", "prelude = bundled", 1)
        if temporary == original:
            raise SystemExit(".buckconfig does not pin prelude = disabled")
        try:
            config.write_text(temporary)
            subprocess.run(
                [
                    str(args.buck2.resolve()),
                    "--isolation-dir",
                    "buck2-bootstrap",
                    "expand-external-cell",
                    "prelude",
                ],
                cwd=root,
                check=True,
            )
        finally:
            config.write_text(original)
            # Expansion starts a dedicated coordinator. It has no purpose
            # after the cell is materialized, so do not leave one resident
            # for every checkout that bootstraps.
            subprocess.run(
                [str(args.buck2.resolve()), "--isolation-dir", "buck2-bootstrap", "kill"],
                cwd=root,
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
    if not args.prelude_dir.is_dir():
        raise SystemExit("Buck2 did not expand the pinned prelude")
    outputs = expected_outputs(args.prelude_dir)
    hashes = {name: digest(data) for name, data in sorted(outputs.items())}
    if args.print_output_hashes:
        print(json.dumps(hashes, indent=4, sort_keys=True))
        return 0
    if any(OUTPUT_SHA256.get(name) != value for name, value in hashes.items()):
        raise SystemExit("prelude overlay output hashes do not match the pinned transform")
    stale = []
    for name, data in outputs.items():
        path = args.prelude_dir / name
        if path.exists() and path.read_bytes() == data:
            continue
        stale.append(name)
        if not args.check:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
    if stale and args.check:
        print("prelude overlay is absent or stale: " + ", ".join(stale), file=sys.stderr)
        return 1
    receipt = args.prelude_dir / ".lash-overlay.json"
    payload = json.dumps({"schema": 1, "outputs": hashes}, indent=2, sort_keys=True) + "\n"
    if args.check:
        if not receipt.is_file() or receipt.read_text() != payload:
            print("prelude overlay receipt is absent or stale", file=sys.stderr)
            return 1
    else:
        receipt.write_text(payload)
    print("verified" if args.check else "applied", "Lash Rust action overlay")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
