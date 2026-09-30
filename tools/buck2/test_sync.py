#!/usr/bin/env python3
"""Fail-closed checks for the generated Buck2 graph and pool contracts."""

from __future__ import annotations

import ast
import importlib.util
import json
import pathlib
import re
import os
import subprocess
import sys
import tempfile
import tomllib


ROOT = pathlib.Path(__file__).resolve().parents[2]
HERE = ROOT / "tools/buck2"


def load_json(name: str):
    return json.loads((HERE / name).read_text(encoding="utf-8"))


def bzl_value(text: str, name: str):
    match = re.search(rf"(?ms)^{re.escape(name)} = (\{{.*?^\}}|\[.*?^\])\n", text)
    if not match:
        raise AssertionError(f"missing generated {name}")
    return ast.literal_eval(match.group(1))


def labels(inventory: dict) -> tuple[set[str], set[str]]:
    ordinary = {
        target["label"]
        for package in inventory["packages"]
        for target in package["targets"]
        if target.get("label")
    }
    variants = {unit["label"] for unit in inventory["feature_lane_units"]}
    return ordinary, variants


def check_inventory() -> None:
    inventory = load_json("target-inventory.json")
    ordinary, variants = labels(inventory)
    assert len(ordinary) == inventory["generated_label_count"]
    assert len(inventory["packages"]) == inventory["cargo_package_count"]

    kinds = {
        target["label"]: target["kind"]
        for package in inventory["packages"]
        for target in package["targets"]
        if target.get("label")
    } | {unit["label"]: unit["kind"] for unit in inventory["feature_lane_units"]}
    for collection in ("workspace_compile_targets", "feature_lane_compile_targets"):
        assert set(inventory[collection]) <= ordinary | variants
    for collection in ("workspace_build_targets", "feature_lane_build_targets"):
        for label in inventory[collection]:
            base = label.removesuffix("[static]")
            assert base in ordinary | variants
            assert label.endswith("[static]") == (kinds[base] == "lib")
    checks = inventory["feature_lane_check_targets"]
    assert len(checks) == len(inventory["feature_lane_compile_targets"])
    assert checks == [label + "[check]" for label in inventory["feature_lane_compile_targets"]]
    for unit in inventory["feature_lane_units"]:
        assert unit["check_label"] == unit["label"] + "[check]"

    for unit in [
        target
        for package in inventory["packages"]
        for target in package["targets"]
    ] + inventory["feature_lane_units"]:
        if unit["kind"] == "lib":
            assert unit["doc_label"] == unit["label"] + "[doc]"

    lane_members = set()
    for lane, members in inventory["feature_lanes"].items():
        assert lane and members
        assert set(members) <= set(inventory["feature_lane_compile_targets"])
        lane_members.update(members)
    assert lane_members == set(inventory["feature_lane_compile_targets"])
    assert set(inventory["feature_lane_test_floors"]) <= set(
        inventory["feature_lane_test_targets"]
    )

    batches = inventory["workspace_test_batches"]
    member_owner = {}
    for batch, members in batches.items():
        assert members
        for member in members:
            assert member in inventory["workspace_test_targets"]
            assert member not in member_owner, f"{member} belongs to two batches"
            member_owner[member] = batch
    excluded = {
        target["label"]
        for package in inventory["packages"]
        for target in package["targets"]
        if target.get("label")
        and ({"manual", "pr-deferred"} & set(target.get("tags", [])))
    }
    expected_suites = (
        set(inventory["workspace_test_targets"]) - set(member_owner) - excluded
    ) | set(batches)
    assert expected_suites == set(inventory["workspace_test_suite_labels"])
    core = set(inventory["workspace_core_suite_labels"])
    tail = set(inventory["workspace_tail_suite_labels"])
    assert core.isdisjoint(tail)
    assert core | tail == expected_suites

    timeouts = inventory["test_timeout_seconds"]
    assert set(inventory["workspace_test_targets"]) | set(batches) <= set(timeouts)
    assert set(timeouts.values()) <= {60, 300, 900, 3600}

    required = {
        "workspace_compile",
        "workspace_check",
        "workspace_tests",
        "dev_tests",
        "workspace_core_tests",
        "workspace_tail_tests",
        "deferred_tests",
        "workspace_clippy",
        "workspace_docs",
        "feature_lane_compile",
        "feature_lane_tests",
        "feature_lane_clippy",
        "host_schema_documents",
        "host_schema_check",
        "schema_checks",
        "workspace_rust_sources",
        "workspace_test_scripts",
        "confidence_gate_scripts",
        "perf_guard_budgets",
        "perf_duration_level_shifts",
    }
    root_buck = (ROOT / "BUCK").read_text(encoding="utf-8")
    actual = set(re.findall(r'^\s*name = "([^"]+)"', root_buck, re.M))
    assert required <= actual, f"missing stable root labels: {sorted(required - actual)}"
    assert "schema_check_group(\n    name = \"schema_checks\"" in root_buck
    assert 'load("//tools/buck2:source_tree.bzl", "lash_workspace_sources")' in root_buck
    assert 'lash_workspace_sources(\n    name = "workspace_rust_sources"' in root_buck
    assert root_buck.count("[check]\"") == 2 * len(checks) + len(inventory["workspace_check_targets"])


def check_sizing() -> None:
    text = (HERE / "exec_sizes.bzl").read_text(encoding="utf-8")
    compile_requests = bzl_value(text, "COMPILE_REQUESTS")
    test_requests = bzl_value(text, "TEST_RUN_REQUESTS")
    batches = bzl_value(text, "BATCH_BUDGETS")
    measured_compile = load_json("action-sizes.json")
    measured_tests = load_json("test-run-sizes.json")
    for key, measured in measured_compile.items():
        assert key in compile_requests
        assert {
            "cpu_count": compile_requests[key]["cpu_count"],
            "memory_kb": compile_requests[key]["memory_kb"],
        } == {"cpu_count": measured["cpu_count"], "memory_kb": measured["memory_kb"]}
    for label, measured in measured_tests.items():
        assert label in test_requests or label in batches
        actual = test_requests.get(label, batches.get(label))
        assert actual["cpu_count"] >= measured["cpu_count"]
        assert actual["memory_kb"] >= measured["memory_kb"]

    platform_text = (HERE / "platforms.bzl").read_text(encoding="utf-8")
    budgets = {
        (int(cpu), int(memory))
        for cpu, memory in re.findall(r"^    \((\d+), (\d+)\),$", platform_text, re.M)
    }
    requested = {
        (value["cpu_count"], value["memory_kb"])
        for table in (compile_requests, test_requests, batches)
        for value in table.values()
    }
    assert requested <= budgets, f"unregistered pool budgets: {sorted(requested - budgets)}"

    for path in sorted(ROOT.rglob("BUCK")):
        if ".buck2" in path.parts or "vendor" in path.parts:
            continue
        text = path.read_text(encoding="utf-8")
        if not text.startswith("# @generated by tools/buck2/sync.py"):
            continue
        for block in text.split("\n\n"):
            if re.match(r"lash_rust_(library|binary|unit_test|integration_test|feature)", block):
                assert "exec_properties = sized_exec_properties(" in block, path


def check_ownership() -> None:
    ownership = load_json("source-ownership.json")
    package_dirs = {
        pathlib.Path(package["manifest"]).parent.as_posix()
        for package in load_json("target-inventory.json")["packages"]
    }
    for package, policy in ownership.items():
        assert package in package_dirs
        patterns = []
        for key, value in policy.items():
            if isinstance(value, list):
                patterns.extend(value)
            elif isinstance(value, dict):
                patterns.extend(pattern for values in value.values() for pattern in values)
        for pattern in patterns:
            assert list((ROOT / package).glob(pattern)), f"unmatched ownership pattern {package}/{pattern}"


def check_action_bridge() -> None:
    overlay = (HERE / "prelude_overlay.py").read_text(encoding="utf-8")
    assert "rust_identity = True" in overlay
    assert 'exec_compatible_with = ["root//tools/buck2:pool_1_1572864"]' in overlay
    assert "action_allow_cache_upload = True" in overlay
    third_party = (HERE / "third_party.bzl").read_text(encoding="utf-8")
    assert '"KILN_ACTION_CPU_COUNT": _DEFAULT_CPU' in third_party
    assert '"KILN_ACTION_MEMORY_KB": _DEFAULT_MEMORY_KB' in third_party
    assert "_DEFAULT_CONSTRAINT" in third_party
    for name in ("schema_checks.bzl", "test_rules.bzl", "test_batch.bzl", "platforms.bzl"):
        text = (HERE / name).read_text(encoding="utf-8")
        if "ctx.actions.run(" in text or "ExternalRunnerTestInfo(" in text:
            assert "KILN_ACTION_CPU_COUNT" in text
            assert "KILN_ACTION_MEMORY_KB" in text
    tests = (HERE / "test_rules.bzl").read_text(encoding="utf-8")
    batch = (HERE / "test_batch.bzl").read_text(encoding="utf-8")
    for text in (tests, batch):
        assert '"TEST_TARGET"' in text
        assert '"TEST_SHARD_INDEX"' in text
        assert '"TEST_TOTAL_SHARDS"' in text
    assert "default_outputs = test_info.default_outputs" in tests
    assert "for name, providers in test_info.sub_targets.items()" in tests


def check_measurement_filter() -> None:
    spec = importlib.util.spec_from_file_location("action_sizes", HERE / "action_sizes_from_log.py")
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    known = {("lash-internal-store-sql", "lash_store_sql")}
    base = (
        "1\ttool={tool}\tpkg={pkg}\tcrate={crate}\texit=0\twall_ms=2000"
        "\tcpu_usec=2000000\tpeak_bytes=1073741824\trequested_cpu=2"
    )
    lines = [
        base.format(tool="python3", pkg="lash-internal-store-sql", crate="lash_store_sql"),
        base.format(tool="python3", pkg="", crate=""),
        base.format(tool="bash", pkg="lash-internal-store-sql", crate="lash_store_sql"),
        base.format(tool="python3", pkg="another-repo", crate="lash_store_sql"),
    ]
    measured = module.collect(lines, known)
    assert list(measured) == ["lash-internal-store-sql/lash_store_sql"]
    assert len(measured["lash-internal-store-sql/lash_store_sql"].records) == 1


def check_native_inputs() -> None:
    lock = load_json("native-tools-lock.json")
    assert lock["tools"]["node"]["version"] == "24.20.0"
    assert "bin/node" in lock["tools"]["node"]["required"]
    assert "lib/clang/22/include/stdarg.h" in lock["tools"]["llvm"]["required"]
    assert len(lock["tools"]["llvm"]["sha256"]) == 64
    bootstrap = (HERE / "bootstrap_native_tools.py").read_text(encoding="utf-8")
    assert 'name = "llvm_tree"' in bootstrap
    assert 'p.removeprefix("llvm/")' in bootstrap
    toolchain = (HERE / "toolchains/BUCK").read_text(encoding="utf-8")
    assert '"-resource-dir", "$(location native//:llvm_tree)/lib/clang/22"' in toolchain


def check_dependency_and_profile_projection() -> None:
    deps = (HERE / "deps.bzl").read_text(encoding="utf-8")
    external = set(re.findall(r'//third-party/rust:([^\"]+)', deps))
    assert external
    assert all(re.fullmatch(r"p\d{4}", target) for target in external), external

    root_rules = (HERE / "lash_rust.bzl").read_text(encoding="utf-8")
    assert 'if "cargo-trybuild" in tags:' in root_rules
    assert 'harness = ":" + binary' in root_rules
    assert 'visibility = ["PUBLIC"]' in root_rules

    lash_rules = (ROOT / "crates/lash/BUCK").read_text(encoding="utf-8")
    for source in (
        "tests/ui/core_builder_plugin_host_is_removed.rs",
        "tests/ui/core_builder_plugin_host_is_removed.stderr",
    ):
        assert f'name = "{source}"' in lash_rules
        assert f'src = "{source}"' in lash_rules
    manual_ui = (
        ROOT / "crates/lash/tests/builder_contract/BUCK"
    ).read_text(encoding="utf-8")
    assert 'name = "builder_plugin_host_is_removed_without_testing"' in manual_ui
    assert 'fixtures = ["//crates/lash:tests/ui/core_builder_plugin_host_is_removed.rs"]' in manual_ui
    assert 'expected = ["//crates/lash:tests/ui/core_builder_plugin_host_is_removed.stderr"]' in manual_ui
    ui_rule = (HERE / "ui_fixtures.bzl").read_text(encoding="utf-8")
    assert "DefaultInfo(default_outputs = harness.harness_outputs)" in ui_rule
    harness = re.search(r'harness = "(//crates/lash:ui__test__fv_[0-9a-f]+)"', manual_ui)
    assert harness
    variant_name = harness.group(1).split(":", 1)[1]
    variant = re.search(
        rf'lash_rust_feature_test\(\n\s*name = "{variant_name}",.*?\n\)',
        lash_rules,
        re.S,
    )
    assert variant and '"cargo-trybuild"' in variant.group(0)

    package = (HERE / "BUCK").read_text(encoding="utf-8")
    assert 'constraint_value(name = "profile_optimized"' in package
    assert 'name = "optimized"' in package
    assert '":profile_optimized"' in package

    toolchain = (HERE / "toolchains/rust.bzl").read_text(encoding="utf-8")
    first_party = (HERE / "lash_rust.bzl").read_text(encoding="utf-8")
    third_party = (HERE / "third_party.bzl").read_text(encoding="utf-8")
    for flag in (
        "-Copt-level=3",
        "-Cdebuginfo=0",
        "-Cstrip=debuginfo",
        "-Cembed-bitcode=no",
    ):
        assert flag in first_party
        assert flag in third_party
    assert "extra_rustc_flags = []" in toolchain
    assert '"//tools/buck2:profile_host": _OPTIMIZED_FLAGS' in first_party
    assert '"//tools/buck2:profile_host": _OPTIMIZED_FLAGS' in third_party
    assert 'incoming_transition = _HOST_TRANSITION' in first_party
    assert 'kwargs["incoming_transition"] = _HOST_TRANSITION' in third_party
    transition = (HERE / "host_transition.bzl").read_text(encoding="utf-8")
    assert "constraints.pop(budget.label, None)" in transition
    assert "constraints[host.setting.label] = host" in transition
    assert "constraints[platform_target.setting.label] = platform_target" in transition

    overlay = (HERE / "prelude_overlay.py").read_text(encoding="utf-8")
    assert "rust_toolchain_info.rustc_flags + rust_toolchain_info.extra_rustc_flags" in overlay
    assert "rust_toolchain_info.extra_rustc_flags," in overlay

    vendor = (HERE / "bootstrap_vendor.py").read_text(encoding="utf-8")
    assert 'ROOT / ".cargo/config.toml"' not in vendor
    assert ".unlink(" not in vendor


def check_sync_receipt() -> None:
    spec = importlib.util.spec_from_file_location("buck2_sync", HERE / "sync.py")
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    receipt = json.loads(module.RECEIPT.read_text(encoding="utf-8"))
    assert receipt["schema"] == 1
    assert receipt["inputs"]
    assert receipt["outputs"]
    assert "tools/buck2/sync.py" in receipt["inputs"]
    assert "tools/buck2/target-inventory.json" in receipt["outputs"]
    assert ".buck2/sync-receipt.json" not in receipt["outputs"]
    assert module.receipt_is_current()


def check_clippy_receipt_inputs() -> None:
    spec = importlib.util.spec_from_file_location("buck2_sync_clippy", HERE / "sync.py")
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    with tempfile.TemporaryDirectory(prefix="lash-clippy-receipt-") as directory:
        root = pathlib.Path(directory)
        module.ROOT = root
        module.RECEIPT = root / ".buck2/sync-receipt.json"
        subprocess.run(["git", "init", "--quiet", str(root)], check=True)
        generator = root / "tools/buck2/clippy_policy.py"
        generator.parent.mkdir(parents=True)
        generator.write_text("original lint renderer\n", encoding="utf-8")
        config = root / "crates/lash-store-sql/clippy.toml"
        config.parent.mkdir(parents=True)
        output = root / "tools/buck2/clippy_policy.bzl"
        output.write_text("generated lint configs\n", encoding="utf-8")
        outputs = {output: output.read_text(encoding="utf-8")}
        subprocess.run(["git", "add", "."], cwd=root, check=True)
        module.write_receipt(outputs)
        assert module.receipt_is_current()
        config.write_text('disallowed-methods = ["std::thread::sleep"]\n', encoding="utf-8")
        assert not module.receipt_is_current(), "new nearest Clippy config reused stale receipt"
        module.write_receipt(outputs)
        assert module.receipt_is_current()
        config.write_text("disallowed-methods = []\n", encoding="utf-8")
        assert not module.receipt_is_current(), "edited Clippy config reused stale receipt"
        module.write_receipt(outputs)
        config.unlink()
        assert not module.receipt_is_current(), "removed Clippy config reused stale receipt"
        module.write_receipt(outputs)
        assert module.receipt_is_current()
        generator.write_text("changed lint renderer\n", encoding="utf-8")
        assert not module.receipt_is_current(), "changed lint renderer reused stale receipt"


def check_rust_edit_receipt_inputs() -> None:
    spec = importlib.util.spec_from_file_location("buck2_sync_rust", HERE / "sync.py")
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    with tempfile.TemporaryDirectory(prefix="lash-rust-edit-receipt-") as directory:
        root = pathlib.Path(directory)
        module.ROOT = root
        module.RECEIPT = root / ".buck2/sync-receipt.json"
        subprocess.run(["git", "init", "--quiet", str(root)], check=True)
        source = root / "src/lib.rs"
        source.parent.mkdir()
        source.write_text("pub fn value() -> u32 { 1 }\n")
        output = root / "BUCK"
        output.write_text("generated targets\n")
        outputs = {output: output.read_text()}
        subprocess.run(["git", "add", "."], cwd=root, check=True)
        module.write_receipt(outputs)
        source.write_text("pub fn value() -> u32 { 2 }\n")
        assert module.receipt_is_current(), "ordinary Rust edit forced graph regeneration"
        source.write_text('const TOOL: &str = env!("CARGO_BIN_EXE_lash-tool");\n')
        assert not module.receipt_is_current(), "new runtime binary dependency reused stale graph"
        module.write_receipt(outputs)
        source.write_text("pub fn value() -> u32 { 3 }\n")
        assert not module.receipt_is_current(), "removed runtime binary dependency reused stale graph"
        module.write_receipt(outputs)
        new_target = root / "src/bin/new-tool.rs"
        new_target.parent.mkdir()
        new_target.write_text("fn main() {}\n")
        assert not module.receipt_is_current(), "new Cargo target reused stale graph"
        module.write_receipt(outputs)
        new_target.unlink()
        assert not module.receipt_is_current(), "removed Cargo target reused stale graph"


def check_external_buildscripts() -> None:
    lock = tomllib.loads((ROOT / "third-party/Cargo.lock").read_text(encoding="utf-8"))
    expected = set()
    for package in lock["package"]:
        if package["name"] == "lash-buck2-third-party":
            continue
        source = ROOT / "vendor" / f"{package['name']}-{package['version']}"
        manifest = tomllib.loads((source / "Cargo.toml").read_text(encoding="utf-8"))
        build = manifest.get("package", {}).get("build")
        if build is False:
            continue
        if build or (source / "build.rs").is_file():
            expected.add((package["name"], package["version"]))

    generated = (ROOT / "third-party/rust/BUCK").read_text(encoding="utf-8")
    actual = set()
    for block in re.findall(r"third_party_buildscript_run\(\n(.*?)\n\)\n", generated, re.S):
        name = re.search(r'^\s*package_name = "([^"]+)"', block, re.M)
        version = re.search(r'^\s*version = "([^"]+)"', block, re.M)
        assert name and version
        actual.add((name.group(1), version.group(1)))
    assert actual == expected, (
        f"missing build scripts: {sorted(expected - actual)}; "
        f"unexpected build scripts: {sorted(actual - expected)}"
    )
    metadata_edges = {
        re.search(r'^\s*name = "([^"]+)"', block, re.M).group(1): sorted(
            re.findall(r'":([^"\]]+\[metadata\])"', block)
        )
        for block in re.findall(r"third_party_buildscript_run\(\n(.*?)\n\)\n", generated, re.S)
        if "env_srcs =" in block
    }
    assert metadata_edges == {
        "aws-lc-rs-1-build-script-run": ["aws-lc-sys-0.45-build-script-main-run[metadata]"],
        "rustls-0.23-build-script-run": [
            "aws-lc-rs-1-build-script-run[metadata]",
            "ring-0.17-build-script-run[metadata]",
        ],
        "wasm-bindgen-0.2-build-script-run": [
            "wasm-bindgen-shared-0.2-build-script-run[metadata]"
        ],
    }


def check_buildscript_metadata_bridge() -> None:
    runner = ROOT / ".buck2/prelude/rust/tools/buildscript_run.py"
    with tempfile.TemporaryDirectory(prefix="lash-links-metadata-") as directory:
        root = pathlib.Path(directory)
        manifest = root / "manifest"
        manifest.mkdir()
        cfg = root / "rustc-cfg"
        cfg.write_text("", encoding="utf-8")

        def executable(name: str, body: str) -> pathlib.Path:
            path = root / name
            path.write_text("#!/bin/sh\nset -eu\n" + body, encoding="utf-8")
            path.chmod(0o755)
            return path

        producer = executable(
            "producer.sh", 'printf "cargo:include=%s/include\\n" "$OUT_DIR"\n'
        )
        producer_root = root / "producer"
        producer_root.mkdir()
        metadata = producer_root / "METADATA"
        env = os.environ | {
            "CARGO_MANIFEST_LINKS": "aws_lc_0_45_0",
            "OUT_DIR": str(producer_root / "OUT_DIR"),
            "RUSTC": "/bin/true",
            "TARGET": "x86_64-unknown-linux-gnu",
        }
        common = [
            sys.executable,
            str(runner),
            "--rustc-cfg",
            str(cfg),
            "--manifest-dir",
            str(manifest),
        ]
        subprocess.run(
            common
            + [
                "--buildscript",
                str(producer),
                "--create-cwd",
                str(producer_root / "cwd"),
                "--outfile",
                str(producer_root / "rustc_flags"),
                "--outenv",
                str(metadata),
            ],
            env=env,
            check=True,
            capture_output=True,
            text=True,
        )
        produced = json.loads(metadata.read_text(encoding="utf-8"))
        assert produced == {
            "DEP_AWS_LC_0_45_0_INCLUDE": "${__BUILDSCRIPT_OUT_DIR__}/include"
        }

        consumer = executable(
            "consumer.sh",
            'printf "cargo:rustc-env=SEEN=%s\\n" "$DEP_AWS_LC_0_45_0_INCLUDE"\n',
        )
        consumer_root = root / "consumer"
        consumer_root.mkdir()
        env = os.environ | {
            "OUT_DIR": str(consumer_root / "OUT_DIR"),
            "RUSTC": "/bin/true",
            "TARGET": "x86_64-unknown-linux-gnu",
        }
        flags = consumer_root / "rustc_flags"
        subprocess.run(
            common
            + [
                "--buildscript",
                str(consumer),
                "--create-cwd",
                str(consumer_root / "cwd"),
                "--outfile",
                str(flags),
                "--outenv",
                str(consumer_root / "METADATA"),
                "--extra-env",
                str(metadata),
            ],
            env=env,
            check=True,
            capture_output=True,
            text=True,
        )
        expected = producer_root / "OUT_DIR/include"
        assert flags.read_text(encoding="utf-8") == f"--env-set=SEEN={expected}\n"


def check_direct_buck_generator() -> None:
    """The checked-in graph comes directly from the Buck2 renderer."""
    generator = (HERE / "generate_model.py").read_text(encoding="utf-8")
    synchronizer = (HERE / "sync.py").read_text(encoding="utf-8")
    policy = (HERE / "package-policy.toml").read_text(encoding="utf-8")
    forbidden = (
        "transform_model_output",
        "tools/bazel",
        "BUILD.bazel",
        "MODULE.bazel",
        "@rules_rust",
        "@crates",
        "WORKSPACE_BAZEL",
        "generate_build_files.py",
    )
    combined = generator + synchronizer + policy
    assert not [token for token in forbidden if token in combined]

    llm_tools = (ROOT / "crates/lash-llm-tools/BUCK").read_text(encoding="utf-8")
    integrator = (ROOT / "examples/integrator-contract/BUCK").read_text(
        encoding="utf-8"
    )
    assert 'compile_data_patterns = [\n        "Cargo.toml",\n    ],' in llm_tools
    assert 'compile_data_patterns = [\n        "Cargo.toml",\n    ],' in integrator

    clippy = (HERE / "clippy_policy.bzl").read_text(encoding="utf-8")
    inventory = load_json("target-inventory.json")
    assert f"WORKSPACE_PACKAGE_COUNT = {len(inventory['packages'])}" in clippy
    assert '"crates/lash-core": ("clippy_config_crates_lash_core",' in clippy
    assert 'load(":clippy_policy.bzl", "declare_clippy_configurations")' in (
        HERE / "BUCK"
    ).read_text(encoding="utf-8")
    assert 'name = "clippy.toml"' in (ROOT / "crates/lash-core/BUCK").read_text(
        encoding="utf-8"
    )
    examples = (ROOT / "examples/BUCK").read_text(encoding="utf-8")
    assert 'name = "shared_rust_sources"' in examples
    assert 'name = "typescript_host_flow_cells"' in examples

    protocol = (ROOT / "crates/lash-protocol-rlm/BUCK").read_text(encoding="utf-8")
    client = (ROOT / "crates/lash-vm-client/BUCK").read_text(encoding="utf-8")
    worker = (ROOT / "crates/lash-vm-worker/BUCK").read_text(encoding="utf-8")
    for generated in (protocol, client, worker):
        assert 'name = "Cargo.toml"' in generated
        assert 'visibility = ["PUBLIC"]' in generated
    for generated in (client, worker):
        assert 'build_script_env = {"LASH_VM_WORKER_SOURCE_ROOT": ".lash-workspace"}' in generated
        assert '"//crates/lash-vm-client:buildscript_sources"' in generated
        assert '"//crates/lash-vm-worker:buildscript_sources"' in generated
    assert 'extra_srcs = ["//crates/lash-vm-worker:rust_sources"]' in client
    assert '"//crates/lash-vm-worker:lash-vm-worker__bin"' in protocol
    assert 'test_env = {"LASH_VM_WORKER": "$(location //crates/lash-vm-worker:lash-vm-worker__bin)"}' in protocol
    manifest_rule = (HERE / "buildscript_manifest.bzl").read_text(encoding="utf-8")
    assert "BuildscriptSourcesInfo" in manifest_rule
    assert "DefaultInfo(default_output = tree, other_outputs = ctx.attrs.srcs)" in manifest_rule
    assert 'name = "buildscript_sources"' in worker
    assert 'paths.join(".lash-workspace", package, source.short_path)' in manifest_rule
    assert 'manifest_dir = ":" + manifest' in (
        HERE / "lash_rust.bzl"
    ).read_text(encoding="utf-8")

    build_scripts = {
        target["label"]
        for package in inventory["packages"]
        for target in package["targets"]
        if target["kind"] == "custom-build-compile"
    }
    assert build_scripts == {
        "//crates/lash-protocol-rlm:build_script__build",
        "//crates/lash-vm-client:build_script__build",
        "//crates/lash-vm-worker:build_script__build",
    }
    bins_units = [
        unit for unit in inventory["feature_lane_units"]
        if unit["package"] == "lash-internal-vm-worker"
        and unit["kind"] == "bin"
        and unit["features"] == []
    ]
    assert {unit["label"].split(":", 1)[1].split("__fv_", 1)[0] for unit in bins_units} == {
        "lash-vm-worker__bin"
    }
    provider_stream = next(
        target
        for package in inventory["packages"]
        if package["package"] == "lash-restate-postgres-workers-e2e"
        for target in package["targets"]
        if target.get("cargo") == "provider_stream_bounds"
    )
    assert provider_stream["tags"] == ["cargo-service-gate", "manual"]
    inventory_text = json.dumps(inventory, sort_keys=True)
    assert not re.search(r":build_script_(?:\[|\")", inventory_text)

    generated_graph = "\n".join(
        path.read_text(encoding="utf-8")
        for path in ROOT.rglob("BUCK")
    )
    assert "$(rootpath " not in generated_graph
    assert (
        '"CARGO_BIN_EXE_slack-clone-mcp-server": '
        '"$(location :slack-clone-mcp-server__bin)"'
    ) in generated_graph


def main() -> int:
    checks = [
        check_inventory,
        check_sizing,
        check_ownership,
        check_action_bridge,
        check_measurement_filter,
        check_native_inputs,
        check_dependency_and_profile_projection,
        check_sync_receipt,
        check_clippy_receipt_inputs,
        check_rust_edit_receipt_inputs,
        check_external_buildscripts,
        check_buildscript_metadata_bridge,
        check_direct_buck_generator,
    ]
    for check in checks:
        check()
    print(f"PASS: {len(checks)} Buck2 graph contracts")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AssertionError as error:
        print(f"FAIL: {error}", file=sys.stderr)
        raise SystemExit(1)
