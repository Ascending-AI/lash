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
from collections import defaultdict


ROOT = pathlib.Path(__file__).resolve().parents[2]
HERE = ROOT / "tools/buck2"
# The receipts these checks write describe fixtures; never offer them to real checkouts.
os.environ["LASH_BUCK2_STORE"] = "off"


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

    # A test only Cargo can execute never enters a list the test jobs run,
    # under its own label or as a feature-lane variant; compile lists may.
    cargo_only = {
        target["label"]
        for package in inventory["packages"]
        for target in package["targets"]
        if target.get("label") and "cargo-trybuild" in target.get("tags", [])
    }
    assert cargo_only
    executed = (
        inventory["feature_lane_test_targets"]
        + inventory["workspace_test_suite_labels"]
        + inventory["workspace_dev_suite_labels"]
        + inventory["workspace_deferred_test_targets"]
        + [label for labels in inventory["service_test_targets"].values() for label in labels]
        + [member for members in inventory["workspace_test_batches"].values() for member in members]
    )
    assert not [
        label for label in executed if re.sub(r"__fv_[0-9a-f]+$", "", label) in cargo_only
    ]

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
    test_compile_requests = bzl_value(text, "TEST_COMPILE_REQUESTS")
    measured_compile = load_json("action-sizes.json")
    measured_kinds = load_json("target-kind-sizes.json")
    measured_tests = load_json("test-run-sizes.json")

    def request(row):
        return {"cpu_count": row["cpu_count"], "memory_kb": row["memory_kb"]}

    # A library and its unit-test binary each resolve their own kind's row,
    # else the crate's, else the default; a test binary is listed only where
    # its request differs from its library's.
    sys.path.insert(0, str(HERE))
    import action_categories_from_events as joined
    import action_sizes_from_log as sizes

    assert set(measured_kinds) <= joined.shared_identities(load_json("target-inventory.json"))
    assert set(compile_requests) == set(measured_compile) | set(measured_kinds)
    assert set(test_compile_requests) <= set(compile_requests)
    import generate_model as model

    default = {"cpu_count": model.DEFAULT_CPU_COUNT, "memory_kb": model.DEFAULT_MEMORY_KB}
    for key, target in compile_requests.items():
        crate = request(measured_compile[key]) if key in measured_compile else default
        kinds = measured_kinds.get(key, {})
        assert kinds or key not in measured_kinds, key
        assert set(kinds) <= {"target", "test"}, key
        test = test_compile_requests.get(key, target)
        assert key not in test_compile_requests or test != target, key
        assert target == (request(kinds["target"]) if "target" in kinds else crate), key
        assert test == (request(kinds["test"]) if "test" in kinds else crate), key
        for kind in kinds:
            assert kinds[kind]["samples"] >= sizes.MIN_SAMPLES, (key, kind)

    # An optimized configuration's request is its row where that is more than
    # the dev request, and never less than the dev request: the select can
    # only raise. A row that raises nothing is not rendered.
    optimized_requests = bzl_value(text, "OPTIMIZED_COMPILE_REQUESTS")
    measured_optimized = load_json("optimized-sizes.json")
    assert set(optimized_requests) <= set(measured_optimized)
    for key, kinds in measured_optimized.items():
        assert kinds and set(kinds) <= {"target", "test"}, key
        for kind, row in kinds.items():
            dev = compile_requests.get(key, default)
            if kind == "test":
                dev = test_compile_requests.get(key, dev)
            expected = {field: max(dev[field], row[field]) for field in dev}
            rendered = optimized_requests.get(key, {}).get(kind, dev)
            assert rendered == expected, (key, kind)
            assert (kind in optimized_requests.get(key, {})) == (expected != dev), (key, kind)
    for key, kinds in optimized_requests.items():
        for kind, rendered in kinds.items():
            dev = compile_requests.get(key, default)
            if kind == "test":
                dev = test_compile_requests.get(key, dev)
            assert all(rendered[field] >= dev[field] for field in dev), (key, kind)
    evidence = load_json("compile-memory-evidence.json")
    assert set(evidence) == {"crates", "clippy", "kinds", "optimized"}
    assert evidence["crates"], "compile memory evidence is empty"
    inventory = load_json("target-inventory.json")
    identities = {
        f"{package['package']}/{target['cargo'].replace('-', '_')}"
        for package in inventory["packages"] for target in package["targets"]
        if target.get("cargo")
    }
    assert set(evidence["crates"]) <= identities
    for key, measured in evidence["crates"].items():
        # Unjoined logs cannot name a target kind or optimized configuration.
        # Use the largest resolved request; joined records below prove each
        # target kind against its own request.
        target = compile_requests.get(key, default)
        requests = [target, test_compile_requests.get(key, target)]
        requests += list(optimized_requests.get(key, {}).values())
        memory_kb = max(row["memory_kb"] for row in requests)
        if key not in compile_requests and key not in optimized_requests:
            memory_kb = max(memory_kb, sizes.ACTION_MEMORY_FLOOR_KB)
        sizes.check_compile_memory(measured, memory_kb, key)
    for profile, measured in (("kinds", evidence["kinds"]), ("optimized", evidence["optimized"])):
        for key, kinds in measured.items():
            assert set(kinds) <= {"target", "test"}, key
            for kind, row in kinds.items():
                resolved = compile_requests.get(key, default)
                if kind == "test":
                    resolved = test_compile_requests.get(key, resolved)
                if profile == "optimized":
                    resolved = optimized_requests.get(key, {}).get(kind, resolved)
                sizes.check_compile_memory(row, resolved["memory_kb"], (profile, key, kind))
    for label, measured in measured_tests.items():
        assert label in test_requests or label in batches
        actual = test_requests.get(label, batches.get(label))
        assert actual["cpu_count"] >= measured["cpu_count"]
        assert actual["memory_kb"] >= measured["memory_kb"]

    # A Clippy twin keeps its own floor and prices only Clippy samples.
    # Its anonymous-memory contract is checked against its own platform.
    clippy_requests = bzl_value(text, "CLIPPY_REQUESTS")
    measured_clippy = load_json("clippy-sizes.json")
    import generate_model as model

    assert sizes.CLIPPY_FLOOR_KB == model.CLIPPY_FLOOR_KB
    helper = ast.literal_eval(re.search(r"(?m)^HELPER_BUDGET = (\(\d+, \d+\))$", text).group(1))
    assert helper == model.HELPER_ACTION_BUDGET, helper
    assert clippy_requests == {key: request(row) for key, row in measured_clippy.items()}
    crates = {
        f"{package['package']}/{target['cargo'].replace('-', '_')}"
        for package in load_json("target-inventory.json")["packages"]
        for target in package["targets"]
        if target.get("cargo")
    }
    for key, row in measured_clippy.items():
        assert key in crates, f"Clippy row for no first-party crate: {key}"
        assert row["samples"] >= sizes.MIN_SAMPLES, key
        assert row["memory_kb"] >= sizes.CLIPPY_FLOOR_KB, key
        if key not in evidence["clippy"]:
            assert row["peak_bytes"] <= row["memory_kb"] * 1024, key
    assert set(evidence["clippy"]) <= crates
    for key, measured in evidence["clippy"].items():
        if key in clippy_requests:
            memory_kb = clippy_requests[key]["memory_kb"]
        else:
            memory_kb = max(model.DEFAULT_MEMORY_KB, sizes.ACTION_MEMORY_FLOOR_KB)
        sizes.check_compile_memory(measured, memory_kb, ("clippy", key))

    # Compile requests resolve through one registered platform each; a test
    # run or batch states its request to the test executor directly.
    budgets = [tuple(budget) for budget in bzl_value(text, "POOL_BUDGETS")]
    assert len(budgets) == len(set(budgets))
    requested = {
        (value["cpu_count"], value["memory_kb"])
        for value in list(compile_requests.values())
        + list(test_compile_requests.values())
        + list(clippy_requests.values())
        + [request for kinds in optimized_requests.values() for request in kinds.values()]
    } | {(model.DEFAULT_CPU_COUNT, model.DEFAULT_MEMORY_KB), (2, 3145728), helper}
    assert requested <= set(budgets), f"unregistered pool budgets: {sorted(requested - set(budgets))}"
    # A target that names no budget takes the first platform: it must stay the
    # unsized request, not whichever row sorts first. Helper and Clippy
    # requests may be smaller; a target names those.
    assert budgets[0] == model.UNSIZED_ACTION_BUDGET, budgets[0]
    assert budgets[1:] == sorted(budgets[1:]), budgets
    assert 'load(":exec_sizes.bzl", "POOL_BUDGETS")' in (HERE / "platforms.bzl").read_text(
        encoding="utf-8"
    )

    # A test's largest peak can be page cache filled to the box's limit, so
    # its row is held to the p99 peak the sizing rule prices -- with headroom:
    # the cgroup kills at the request, so a p99 within a tenth of it means the
    # runs that needed a little more died and left no sample.
    for label, measured in measured_tests.items():
        actual = test_requests.get(label, batches.get(label))
        assert measured["p99_peak_bytes"] < sizes.HEADROOM * actual["memory_kb"] * 1024, label

    for path in sorted(ROOT.rglob("BUCK")):
        if ".buck2" in path.parts or "vendor" in path.parts:
            continue
        text = path.read_text(encoding="utf-8")
        if not text.startswith("# @generated by tools/buck2/sync.py"):
            continue
        for block in text.split("\n\n"):
            if re.match(r"lash_rust_(library|binary|unit_test|integration_test|feature)", block):
                assert "exec_properties = sized_exec_properties(" in block, path


def check_action_categories() -> None:
    """Every remote action category resolves to a deliberate request."""
    sys.path.insert(0, str(HERE))
    import generate_model as model
    import action_sizes_from_log as sizes

    sized = model.ACTION_CATEGORY_SIZES
    assert set(sized.values()) <= {"clippy", "compile", "daemon", "default", "helper", "local", "probe", "unsized"}
    assert sized["clippy"] == "clippy" and sized["deps"] == "daemon"
    assert [category for category, source in sized.items() if source == "local"] == ["http_archive"]
    assert model.UNSIZED_ACTION_BUDGET == (1, 524288)
    assert (model.DEFAULT_CPU_COUNT, model.DEFAULT_MEMORY_KB) in model.FIXED_POOL_BUDGETS
    assert model.HELPER_ACTION_BUDGET in model.FIXED_POOL_BUDGETS
    assert model.HELPER_ACTION_BUDGET[1] == model.CLIPPY_FLOOR_KB

    sources = sorted(HERE.glob("*.bzl")) + [HERE / "prelude_overlay.py"]
    # The checkout's prelude, when bootstrap has installed it: the Rust rules
    # and the helpers the graph reaches through them.
    prelude = ROOT / ".buck2/prelude"
    for directory in ("rust", "http_archive"):
        sources += sorted((prelude / directory).rglob("*.bzl"))
    declared = {}
    for path in sources:
        text = path.read_text(encoding="utf-8")
        for category in re.findall(r'category = "([a-z0-9_]+)"', text):
            declared.setdefault(category, path)
        # `"rustdoc_json" if json else "rustdoc"`
        for category in re.findall(r'category = "[a-z0-9_]+" if \w+ else "([a-z0-9_]+)"', text):
            declared.setdefault(category, path)
    unsized = {category: str(path) for category, path in declared.items() if category not in sized}
    assert not unsized, f"action categories without a deliberate size: {unsized}"
    # A category the daemon lays out never runs as a remote action again: no
    # rule of ours and no file of the overlaid prelude declares it. The
    # overlay script itself names it only in the stock text it replaces.
    remote = {
        category: str(path)
        for path in sources
        if path.name != "prelude_overlay.py"
        for category in re.findall(r'category = "([a-z0-9_]+)"', path.read_text(encoding="utf-8"))
        if sized.get(category) == "daemon"
    }
    assert not remote, f"daemon-side categories declared as actions again: {remote}"
    sys.path.insert(0, str(HERE))
    import prelude_overlay

    assert "ctx.actions.run(" not in prelude_overlay.DAEMON_DEPENDENCY_DIRS
    assert 'category = "deps"' in prelude_overlay.STOCK_DEPENDENCY_DIRS

    # What the pool recorded. A category the workers ran is sized here, and an
    # action no row sizes -- a helper, a build script, a third-party compile --
    # must fit the category's enforced cgroup limit, including the 2 GiB floor.
    smallest = {
        "clippy": model.CLIPPY_FLOOR_KB,
        "compile": model.DEFAULT_MEMORY_KB,
        "daemon": model.DEFAULT_MEMORY_KB,
        "default": model.DEFAULT_MEMORY_KB,
        "helper": model.HELPER_ACTION_BUDGET[1],
        # What the pool recorded before the category left it.
        "local": model.UNSIZED_ACTION_BUDGET[1],
        "probe": model.DEFAULT_MEMORY_KB,
        "unsized": model.UNSIZED_ACTION_BUDGET[1],
    }
    for category, measured in load_json("category-sizes.json").items():
        assert category in sized, f"recorded action category without a deliberate size: {category}"
        assert sorted(measured) == [
            "p99_peak_bytes",
            "peak_bytes",
            "samples",
            "unsized_peak_bytes",
        ], category
        limit_kb = smallest[sized[category]]
        if sized[category] in {"compile", "default"}:
            limit_kb = max(limit_kb, sizes.ACTION_MEMORY_FLOOR_KB)
        assert measured["unsized_peak_bytes"] <= limit_kb * 1024, category

    # A rule of ours that runs an action names its request to the supervisor
    # and its platform to the scheduler; nothing falls through to the first
    # platform by omission.
    for name in ("schema_checks.bzl", "platforms.bzl"):
        text = (HERE / name).read_text(encoding="utf-8")
        assert "KILN_ACTION_CPU_COUNT" in text, name
        assert "pool_constraint(" in text, name
    third_party = (HERE / "third_party.bzl").read_text(encoding="utf-8")
    assert '_DEFAULT_CONSTRAINT = "//tools/buck2:pool_1_524288"' in third_party
    # Build-script runs and schema actions use the small helper budget.
    # Third-party macros state that same budget as their default.
    assert 'load(":exec_sizes.bzl", "HELPER_BUDGET")' in third_party
    assert "cpu, memory = HELPER_BUDGET" in third_party
    assert "pool_constraint(cpu, memory)" in third_party
    assert "pool_constraint(*HELPER_BUDGET)" in (HERE / "schema_checks.bzl").read_text(encoding="utf-8")
    rust = (HERE / "lash_rust.bzl").read_text(encoding="utf-8")
    assert rust.count("cpu, memory = HELPER_BUDGET") == 1
    assert '"exec_compatible_with": [pool_constraint(cpu, memory)]' in rust
    # Every Rust target the macros declare has its Clippy twin: no rule is
    # called but through `_rust_rule`, which declares both.
    assert not re.search(r"native\.rust_\w+\(", rust), "a Rust target declared without its Clippy twin"
    assert "    rule(name = clippy_name, **twin)\n" in rust
    inventory = load_json("target-inventory.json")
    clippy_labels = [
        target["clippy_label"]
        for package in inventory["packages"]
        for target in package["targets"]
        if "clippy_label" in target
    ] + [unit["clippy_label"] for unit in inventory["feature_lane_units"]]
    clippy_labels += inventory["workspace_clippy_build_targets"] + inventory["feature_lane_clippy_build_targets"]
    assert clippy_labels and all(label.endswith("__clippy[clippy.txt]") for label in clippy_labels)
    # The optimized request is selected on the profile constraints alone, with
    # the dev request as the default branch, and only for a target that has
    # one: a dev configuration's action keys never depend on the select.
    assert '"//tools/buck2:profile_host": optimized[key],' in rust
    assert '"//tools/buck2:profile_optimized": optimized[key],' in rust
    assert '"DEFAULT": value,' in rust
    assert "if optimized == dev:\n        return dev\n" in rust
    # `-c kiln.memory_scale=N` scales those requests for one invocation. It is
    # 1 unless the command line says otherwise, and its platforms exist only
    # when it is set.
    platforms = (HERE / "platforms.bzl").read_text(encoding="utf-8")
    assert 'MEMORY_SCALE = int(read_root_config("kiln", "memory_scale", "1"))' in platforms
    assert "if MEMORY_SCALE > 1 and (cpu, memory_kb * MEMORY_SCALE) not in POOL_BUDGETS" in platforms
    assert rust.count("* MEMORY_SCALE") == 3
    assert "memory_scale" not in (ROOT / ".buckconfig").read_text(encoding="utf-8")
    assert "exec_compatible_with = [pool_constraint(cpu, memory)]" in rust


def check_queue_priority() -> None:
    # Every executor that can send an Execute request carries the invocation's
    # queue priority; an override that only runs locally sends none.
    platforms = (HERE / "platforms.bzl").read_text(encoding="utf-8")
    assert 'RE_PRIORITY = int(read_root_config("kiln", "re_priority", "0"))' in platforms
    assert "if RE_PRIORITY < -1000 or RE_PRIORITY > 1000:\n    fail(" in platforms
    assert "KILN_RE_PRIORITY" in platforms
    assert "re_priority" not in (ROOT / ".buckconfig").read_text(encoding="utf-8")
    remote = 0
    for path in sorted(HERE.glob("*.bzl")):
        text = path.read_text(encoding="utf-8")
        for match in re.finditer(r"CommandExecutorConfig\(", text):
            depth, end = 1, match.end()
            while depth:
                depth += {"(": 1, ")": -1}.get(text[end], 0)
                end += 1
            call = text[match.end():end - 1]
            if re.search(r"remote_enabled = False\b", call):
                assert "priority" not in call, f"{path.name}: a local-only executor sets a priority"
                continue
            remote += 1
            assert re.search(r"\bpriority = RE_PRIORITY,", call), f"{path.name}: a remote-enabled CommandExecutorConfig passes no priority = RE_PRIORITY"
            assert path.name == "platforms.bzl" or re.search(r'load\(":platforms.bzl",[^)]*"RE_PRIORITY"', text), path.name
    assert remote == 5, remote


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
    assert 'exec_compatible_with = ["root//tools/buck2:pool_1_524288"]' in overlay
    assert "action_allow_cache_upload = True" in overlay
    third_party = (HERE / "third_party.bzl").read_text(encoding="utf-8")
    assert '"KILN_ACTION_CPU_COUNT": str(cpu)' in third_party
    assert '"KILN_ACTION_MEMORY_KB": str(memory)' in third_party
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


def check_transitive_source_inputs() -> None:
    spec = importlib.util.spec_from_file_location("buck2_prelude_overlay", HERE / "prelude_overlay.py")
    overlay = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(overlay)
    reindeer = tomllib.loads((HERE / "reindeer.toml").read_text(encoding="utf-8"))
    package = str(pathlib.PurePosixPath(reindeer["buck"]["file_name"]).parent)
    assert overlay.THIRD_PARTY_PACKAGE == package
    # Third-party crates remap to `<package>/<name>-<version>.crate/`; the
    # checkout must never provide those paths, or rustc could read them.
    assert not list((ROOT / package).glob("*.crate")), "checkout shadows remapped third-party sources"
    stock = '''    dep_args.add(
        cmd_args(
            hidden = compile_ctx.transitive_srcs.project_as_args("artifacts") if compile_ctx else [],
        )
    )
    compile_cmd = cmd_args(
        hidden = [toolchain_info.compiler, compile_ctx.transitive_srcs.project_as_args("artifacts")],
    )
    hidden = [
        transitive_srcs.project_as_args("artifacts"),
'''
    narrowed = overlay.narrow_transitive_source_inputs(stock)
    assert narrowed.count('project_as_args("kiln_checkout_artifacts")') == 2
    assert narrowed.count('project_as_args("artifacts")') == 1
    assert overlay.narrow_transitive_source_inputs(narrowed) == narrowed
    sources = '''
RustSourcesTSet = transitive_set(
    args_projections = {
        "artifacts": _get_artifacts,
    },
)
'''
    projected = overlay.add_checkout_source_projection(sources)
    assert '"artifacts": _get_artifacts,' in projected
    assert 'owner.package == "{}"'.format(package) in projected
    assert overlay.add_checkout_source_projection(projected) == projected


def check_repo_rooted_source_remap() -> None:
    spec = importlib.util.spec_from_file_location("buck2_prelude_overlay_remap", HERE / "prelude_overlay.py")
    overlay = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(overlay)
    # Cross-package source trees key files by repository path, so rustc must
    # remap their root to the action root rather than prefix the owning
    # package again: `file!()` and dependency metadata name `<package>/<file>`.
    tree = (HERE / "source_tree.bzl").read_text(encoding="utf-8")
    assert "_add(mapped, paths.join(ctx.attrs.package, source.short_path), source)" in tree
    assert "paths.join(package, source.short_path): source" in tree
    rules = (HERE / "lash_rust.bzl").read_text(encoding="utf-8")
    assert rules.count('"srcs_filegroup": ":" + tree,') == 1
    assert rules.count('"kiln_repo_rooted_srcs": True,') == 1
    tree_attrs = rules[rules.index("    tree = name + \"__source_tree\""):]
    tree_attrs = tree_attrs[: tree_attrs.index("    }\n") + 6]
    assert '"kiln_repo_rooted_srcs": True,' in tree_attrs and '"srcs_filegroup"' in tree_attrs
    # Third-party crates keep the stock package-relative `__srcs` remap,
    # `third-party/rust/<name>-<version>.crate/<file>`.
    third_party = (HERE / "third_party.bzl").read_text(encoding="utf-8")
    assert "kiln_repo_rooted_srcs" not in third_party and "srcs_filegroup" not in third_party

    site = '''        cmd_args(
            "--remap-path-prefix=",
            compile_ctx.symlinked_srcs,
            compile_ctx.path_sep,
            "=",
            compile_ctx.symlinked_srcs.owner.path,
            compile_ctx.path_sep,
            delimiter = "",
        ),
'''
    remapped = overlay.remap_repo_rooted_sources(site + site)
    assert remapped.count('[] if getattr(ctx.attrs, "kiln_repo_rooted_srcs", False) else [') == 2
    assert "compile_ctx.symlinked_srcs.owner.path,\n" not in remapped
    assert overlay.remap_repo_rooted_sources(remapped) == remapped
    for broken in (site, remapped + site):
        try:
            overlay.remap_repo_rooted_sources(broken)
        except ValueError:
            continue
        raise AssertionError("source remap overlay accepted a changed prelude")
    decls = '''            "kiln_action_cpu_count": attrs.string(default = "1"),
            "kiln_action_memory_kb": attrs.string(default = "524288"),
'''
    declared = overlay.add_repo_rooted_srcs_attr(decls)
    assert declared.count('"kiln_repo_rooted_srcs": attrs.bool(default = False),') == 1
    assert overlay.add_repo_rooted_srcs_attr(declared) == declared


def check_failure_filter_runs_in_daemon() -> None:
    spec = importlib.util.spec_from_file_location("buck2_prelude_overlay_filter", HERE / "prelude_overlay.py")
    overlay = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(overlay)
    # A passing compile's output is re-exposed by a declared copy chosen from
    # the build status; only a failing compile runs the stock action, with the
    # same category, identifier, error handler and environment.
    filtered = overlay.filter_failures_in_daemon(overlay.STOCK_FAILURE_FILTER)
    assert filtered.count("ctx.actions.dynamic_output(") == 1
    assert "dynamic = [build_status]," in filtered
    assert 'if required.short_path in artifacts[build_status].read_json()["files"]:' in filtered
    assert "ctx.actions.copy_file(outputs[output].as_output(), required)" in filtered
    assert filtered.count("ctx.actions.run(") == 1
    for kept in ('category = "failure_filter",', "identifier = identifier,", "error_handler = toolchain_info.rust_error_handler,", "env = env,", '"--stderr",', '"--build-status",'):
        assert kept in filtered, kept
    assert "prefer_local" not in filtered and "local_only" not in filtered
    assert overlay.filter_failures_in_daemon(filtered) == filtered
    try:
        overlay.filter_failures_in_daemon(overlay.STOCK_FAILURE_FILTER.replace('"--stderr",', '"--diagnostics",'))
    except ValueError:
        pass
    else:
        raise AssertionError("failure filter overlay accepted a changed prelude")
    # The upgrade from the previous overlay output and the stock transform agree.
    assert "rust/failure_filter.bzl" in overlay.PREVIOUS_OUTPUT_SHA256
    assert overlay.upgrade_previous("rust/failure_filter.bzl", overlay.STOCK_FAILURE_FILTER) == filtered
    # A checkout overlaid before small actions reserved 512 MiB upgrades its
    # build-script rule to the pinned output, as a fresh expansion does.
    buildscript = ROOT / ".buck2/prelude/rust/cargo_buildscript.bzl"
    if buildscript.is_file():
        current = buildscript.read_text(encoding="utf-8")
        previous = current.replace("524288", "1572864")
        assert overlay.digest(previous.encode()) in overlay.PREVIOUS_OUTPUT_SHA256["rust/cargo_buildscript.bzl"]
        assert overlay.upgrade_previous("rust/cargo_buildscript.bzl", previous) == current
    # Shared platforms stay remote-only: a hybrid executor would run the stock
    # toolchain's locally-preferred links and archives on the developer host.
    platforms = (HERE / "platforms.bzl").read_text(encoding="utf-8")
    assert "local_enabled = local,\n                remote_enabled = not local," in platforms
    assert "use_limited_hybrid" not in platforms
    # One more platform runs on the invoking host, with no remote half. Only a
    # target that names its constraint resolves to it, and only the crate
    # archive unpack does: every archive of the third-party graph goes through
    # the macro, and nothing else names the constraint.
    assert platforms.count("local_enabled = True,\n            remote_enabled = False,") == 1
    assert platforms.count("ExecutionPlatformInfo(") == 2
    named = [
        path.name
        for path in sorted(HERE.glob("*.bzl"))
        if re.search(r"\bLOCAL_HELPER_CONSTRAINT\b|:local_helper\b", path.read_text(encoding="utf-8"))
    ]
    assert named == ["platforms.bzl", "third_party.bzl"], named
    third_party = (HERE / "third_party.bzl").read_text(encoding="utf-8")
    assert third_party.count("LOCAL_HELPER_CONSTRAINT") == 2
    assert "native.http_archive(\n        name = name,\n        exec_compatible_with = [LOCAL_HELPER_CONSTRAINT]," in third_party
    generated = (ROOT / "third-party/rust/BUCK").read_text(encoding="utf-8")
    assert "\nthird_party_http_archive(\n" in generated
    assert not re.search(r"^http_archive\(", generated, re.M)
    for path in sorted(ROOT.rglob("BUCK")):
        if ".buck2" in path.parts or "vendor" in path.parts or "buck-out" in path.parts:
            continue
        assert "local_helper" not in path.read_text(encoding="utf-8"), path


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


def check_target_kind_rule() -> None:
    """The event join, the per-kind rows and the row-in-force rule hold."""
    result = subprocess.run(
        [sys.executable, str(HERE / "tests/test_action_kinds.py")],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr


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
    # The hermetic PostgreSQL tests: the pinned server and the NSS wrapper are
    # the inputs the wrapper macro prefixes, and only tagged tests carry them.
    postgres = lock["tools"]["postgres"]
    assert postgres["version"].startswith("16.") and len(postgres["sha256"]) == 64
    assert {"bin/initdb", "bin/postgres", "lib/postgresql/pg_stat_statements.so"} <= set(postgres["required"])
    assert lock["tools"]["nss_wrapper"]["required"] == ["libnss_wrapper.so"]
    assert 'name = "postgres"' in bootstrap and 'name = "nss_wrapper"' in bootstrap
    rules = (HERE / "test_rules.bzl").read_text(encoding="utf-8")
    for label in ("native//:postgres", "native//:nss_wrapper", "//tools/buck2:postgres_action_runner"):
        assert f'"$(location {label})"' in rules, label
    inventory = load_json("target-inventory.json")
    hermetic = {
        target["label"]: target["tags"]
        for package in inventory["packages"]
        for target in package["targets"]
        if "hermetic-postgres" in target.get("tags", [])
    }
    assert set(hermetic) == {
        label for label in inventory["service_test_targets"]["postgres"] if "__fv_" not in label
    }, sorted(hermetic)
    assert all(tags == ["hermetic-postgres"] for tags in hermetic.values()), hermetic
    assert set(hermetic) <= set(inventory["workspace_dev_test_targets"])


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

    ui_targets = {}
    for statement in ast.parse(lash_rules).body:
        if (
            isinstance(statement, ast.Expr)
            and isinstance(statement.value, ast.Call)
            and getattr(statement.value.func, "id", "") == "ui_fixtures_test"
        ):
            values = {
                keyword.arg: ast.literal_eval(keyword.value)
                for keyword in statement.value.keywords
            }
            ui_targets[values["name"]] = values
    assert ui_targets["ui_fixtures"]["tests"] == [":ui_store_seam"]
    seam = ui_targets["ui_store_seam"]
    inventory = json.loads((HERE / "target-inventory.json").read_text())
    seam_units = [
        unit for unit in inventory["feature_lane_units"]
        if unit["label"] == seam["harness"]
    ]
    assert seam_units and all(unit["features"] == ["rlm"] for unit in seam_units)
    policy = tomllib.loads((HERE / "package-policy.toml").read_text())
    expected_seams = set(policy["ui_fixtures"]["lash-runtime"]["store_seam"])
    assert {pathlib.Path(path).stem for path in seam["fixtures"]} == expected_seams
    assert seam["expected"] == [path.removesuffix(".rs") + ".stderr" for path in seam["fixtures"]]
    assert not expected_seams.intersection(
        pathlib.Path(path).stem for path in ui_targets["ui_fixtures"]["fixtures"]
    )
    assert "tests = tests" in ui_rule

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
    spec = importlib.util.spec_from_file_location("buck2_sync_links", HERE / "sync.py")
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    opt_outs = module.buildscript_link_opt_outs()
    for block in re.findall(r"third_party_buildscript_run\(\n(.*?)\n\)\n", generated, re.S):
        name = re.search(r'^\s*package_name = "([^"]+)"', block, re.M).group(1)
        for directive in module.BUILDSCRIPT_LINK_DIRECTIVES:
            enabled = f"    {directive} = True,\n" in block + "\n"
            expected_enabled = directive not in opt_outs.get(name, ())
            assert enabled == expected_enabled, (
                f"{name} build script {'drops' if expected_enabled else 'forwards opted-out'} "
                f"cargo:{directive.replace('_', '-')}"
            )
    # Linux builds compile these packages' native objects in their build
    # scripts; the linked binaries need the emitted static archives.
    for name in ("aws-lc-sys", "blake3", "libsqlite3-sys", "ring"):
        assert name not in opt_outs, f"{name} must forward its native link output"
    # `rustc-link-arg*` applies only to the emitting package's own linked
    # targets. Third-party packages must therefore contribute libraries only.
    third_party_binaries = re.findall(r"third_party_rust_binary\(\n(.*?)\n\)\n", generated, re.S)
    assert third_party_binaries
    for block in third_party_binaries:
        assert '"CARGO_CRATE_NAME": "build_script_' in block, block.splitlines()[0]
    assert '"cdylib"' not in generated
    check_buildscript_link_projection(module)
    check_workflow_graph_schema_discovery(module)
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


def check_buildscript_link_projection(module) -> None:
    """The generator forwards link output by default and honors documented opt-outs."""
    block = (
        "third_party_buildscript_run(\n"
        '    name = "{name}-build-script-run",\n'
        '    package_name = "{name}",\n'
        '    buildscript_rule = ":{name}-build-script-build",\n'
        '    version = "1.0.0",\n'
        ")\n"
    )
    content = block.format(name="native") + block.format(name="quiet")
    rendered = module.enable_buildscript_link_directives(
        content, {"quiet": frozenset({"rustc_link_search"})}
    )
    assert rendered == (
        block.format(name="native").replace(
            '    version = "1.0.0"',
            "    rustc_link_lib = True,\n    rustc_link_search = True,\n    version = \"1.0.0\"",
        )
        + block.format(name="quiet").replace(
            '    version = "1.0.0"', '    rustc_link_lib = True,\n    version = "1.0.0"'
        )
    )
    for bad, opt_outs in (
        (block.format(name="native").replace("    version", "    rustc_link_lib = True,\n    version"), {}),
        (content, {"absent": frozenset({"rustc_link_lib"})}),
    ):
        try:
            module.enable_buildscript_link_directives(bad, opt_outs)
        except SystemExit:
            pass
        else:
            raise AssertionError("link projection accepted a conflicting or stale setting")

    with tempfile.TemporaryDirectory(prefix="lash-link-fixups-") as directory:
        root = pathlib.Path(directory)
        fixup = root / "tools/buck2/fixups/quiet/fixups.toml"
        fixup.parent.mkdir(parents=True)
        original = module.ROOT, module.BUCK2
        module.ROOT, module.BUCK2 = root, root / "tools/buck2"
        try:
            fixup.write_text(
                "[buildscript.run]\n# The search path names a host-only directory.\n"
                "rustc_link_search = false\n",
                encoding="utf-8",
            )
            assert module.buildscript_link_opt_outs() == {
                "quiet": frozenset({"rustc_link_search"})
            }
            for text in (
                "[buildscript.run]\nrustc_link_search = false\n",
                "[buildscript.run]\nrustc_link_lib = true\n",
            ):
                fixup.write_text(text, encoding="utf-8")
                try:
                    module.buildscript_link_opt_outs()
                except SystemExit:
                    pass
                else:
                    raise AssertionError(f"accepted link fixup: {text!r}")
        finally:
            module.ROOT, module.BUCK2 = original


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
    # The registry protocol replaces the source-fingerprint build scripts.
    # Client and worker still export source groups for consumers, but neither
    # may retain the retired workspace-root environment contract.
    for generated in (client, worker):
        assert 'name = "build_script"' not in generated
        assert "LASH_VM_WORKER_SOURCE_ROOT" not in generated
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


def check_schema_source_inputs() -> None:
    """Schema rules run and compare single files at their repository paths.

    A filegroup's output is a symlinked directory, so `python3 <filegroup>`
    fails and a check would compare against buck-out instead of the source.
    """
    root = (ROOT / "BUCK").read_text(encoding="utf-8")
    calls = re.findall(r"(?ms)^schema_(?:documents|check)\(\n.*?^\)\n", root)
    assert len(calls) == 4, f"expected four schema rule calls, found {len(calls)}"
    labels = set()
    for call in calls:
        labels.update(re.findall(r"^\s+script = \"(//[^\"]+)\"", call, re.M))
        checked = re.search(r"(?ms)^    checked = \[(.*?)^    \]", call)
        if checked:
            labels.update(re.findall(r"\"(//[^\"]+)\"", checked.group(1)))
    assert labels, "schema rules name no cross-package source inputs"
    for label in sorted(labels):
        package, name = label.removeprefix("//").split(":", 1)
        build = (ROOT / package / "BUCK").read_text(encoding="utf-8")
        target = re.search(
            rf"(?ms)^(\w+)\(\n    name = {re.escape(json.dumps(name))},\n(.*?)^\)\n", build
        )
        assert target, f"{label} is not defined"
        assert target.group(1) == "export_file" and 'mode = "reference",' in target.group(2), (
            f"{label} must be export_file(mode = \"reference\"), not {target.group(1)}"
        )


def check_workflow_graph_schema_discovery(module) -> None:
    """The root filegroup names the one workflow-graph schema, whatever its version."""
    with tempfile.TemporaryDirectory(prefix="lash-graph-schema-") as directory:
        root = pathlib.Path(directory)
        schemas = root / module.WORKFLOW_GRAPH_SCHEMA_DIRECTORY
        schemas.mkdir(parents=True)
        original = module.ROOT
        module.ROOT = root
        try:
            for present, expected in (
                (["v21"], "v21"),
                (["v1"], "v1"),
                ([], None),
                (["v1", "v21"], None),
            ):
                for stale in schemas.iterdir():
                    stale.unlink()
                names = [f"{version}.schema.json" for version in present]
                for name in names:
                    (schemas / name).write_text("{}\n", encoding="utf-8")
                try:
                    found = module.workflow_graph_schema()
                except SystemExit as error:
                    assert expected is None, f"{present} failed: {error}"
                    message = str(error)
                    assert "exactly one" in message, message
                    for name in names or ["none"]:
                        assert name in message, f"{message!r} does not name {name}"
                    continue
                assert expected is not None, f"{present} selected {found}"
                path = f"{module.WORKFLOW_GRAPH_SCHEMA_DIRECTORY}/{expected}.schema.json"
                assert found == path, found
                inventory = defaultdict(list)
                assert (
                    'filegroup(\n    name = "workflow_graph_schema",\n'
                    f'    srcs = ["{path}"],\n    copy = False,\n'
                ) in module.root_buck(inventory)
        finally:
            module.ROOT = original


def check_feature_lane_executable_selection() -> None:
    """A filtered lane test names the executables it filters (FIG-4470).

    `cargo test -p X <filter>` compiles every test target and runs the filter in
    each, so a variant per target would carry the filter into integration
    binaries that never match it. Lanes compile those with `cargo check --tests`
    and execute the filter only in an explicitly selected harness.
    """
    from unittest import mock

    sys.path.insert(0, str(HERE))
    import generate_model as generator

    def emit(arguments, subcommand="test"):
        library = {"name": "example", "kind": ["lib"], "test": True}
        targets = [library, *(
            {"name": name, "kind": ["test"], "test": True}
            for name in ("process_model", "other")
        ), {"name": "app", "kind": ["bin"], "test": True}]
        graph = generator.FeatureLaneGraph.__new__(generator.FeatureLaneGraph)
        graph.by_name = {"example": {"targets": targets}}
        graph.library_of = mock.Mock(return_value=library)
        graph.emit_target = mock.Mock(
            side_effect=lambda package, resolution, target, kind, runnable, args:
            f"{target['name']}:{kind}"
        )
        graph.units = []
        graph.test_args = {}
        command = generator.feature_variants.parse_command(
            ["cargo", subcommand, "-p", "example", "--no-default-features", *arguments]
        )
        tests: list[str] = []
        with mock.patch.object(generator, "cargo_test_policy", return_value=(False, "")):
            compiled = graph.emit_root_targets(command, {"example": []}, tests)
        return graph, compiled, tests, graph.emit_target.call_args_list

    for flags in (["conformance"], ["--tests", "conformance"],
                  ["--all-targets", "--", "conformance"],
                  ["--tests", "--test", "process_model", "conformance"]):
        try:
            emit(flags)
        except ValueError as error:
            assert re.search("filtered feature tests require.*--lib.*--bins.*--test", str(error))
        else:
            raise AssertionError(f"unselected filtered feature test accepted: {flags}")
    graph, compiled, tests, calls = emit(["--lib", "conformance"])
    assert compiled == tests == ["example:unit-test"]
    assert calls[0].args[-1] == ["conformance"]
    assert graph.test_args == {"example:unit-test": ["conformance"]}

    graph, compiled, tests, calls = emit(["--tests"], subcommand="check")
    assert "other:test" in compiled and tests == [] and graph.test_args == {}
    for call in calls:
        assert not call.args[4] and call.args[-1] == []

    for order in ((False, True), (True, False)):
        graph = generator.FeatureLaneGraph.__new__(generator.FeatureLaneGraph)
        graph.chunks = {}
        graph._chunk_names = set()
        graph._runnable_chunks = set()
        for runnable in order:
            graph.add_chunk("example", "unit", "filtered" if runnable else "build",
                            runnable=runnable)
        assert graph.chunks == {"example": [("unit", "filtered")]}
        graph.add_chunk("example", "unit", "filtered", runnable=True)
        try:
            graph.add_chunk("example", "unit", "another filter", runnable=True)
        except ValueError as error:
            assert "conflicting executable selections" in str(error)
        else:
            raise AssertionError("conflicting executable selections accepted")

    inventory = load_json("target-inventory.json")
    assert set(inventory["feature_lane_test_args"]) <= set(
        inventory["feature_lane_test_targets"]
    )


def check_documentation_targets_are_not_tests() -> None:
    """Cargo runs no doctests here, so a doc label must not carry the prelude's."""
    rules = (HERE / "lash_rust.bzl").read_text(encoding="utf-8")
    macro = rules[rules.index("def lash_rust_doc("):].split("\ndef ", 1)[0]
    assert '_rust_doc(name = name, doc = crate + "[doc]"' in macro
    assert "alias" not in macro
    rule = rules[rules.index("def _rust_doc_impl("):rules.index("def lash_rust_doc(")]
    assert "ExternalRunnerTestInfo" not in rule and "sub_targets" not in rule
    for manifest in sorted(ROOT.glob("crates/*/Cargo.toml")):
        text = manifest.read_text(encoding="utf-8")
        assert "[lib]" not in text or "doctest = false" in text, manifest


def check_feature_lane_dependency_edges() -> None:
    """A feature-lane variant links what Cargo activates for it, and no more.

    The variant macros start from the workspace resolution's `PACKAGE_DEPS`,
    which has every optional dependency the workspace turns on. Rebuilt here
    from the generated BUCK files the way `_variant_named_deps` builds them,
    each variant's edges must satisfy what Cargo's resolver guarantees:

    * no edge to an optional dependency the variant's features leave off --
      rustc would accept a missing `#[cfg(feature = ...)]` gate Cargo rejects;
    * an edge to the variant's own package (a self dev-dependency) names the
      library at the variant's features, not the workspace's;
    * one label per first-party package in the closure, so no unloaded second
      copy of a dependency subgraph rides along at workspace features.

    The activated set is read from each Cargo.toml, not from the generator.
    """
    package_deps = bzl_value((HERE / "deps.bzl").read_text(encoding="utf-8"), "PACKAGE_DEPS")
    rules_text = (HERE / "lash_rust.bzl").read_text(encoding="utf-8")
    assert "for extern in pruned_deps:\n        result.pop(extern)" in rules_text, (
        "the variant macros no longer prune the way this contract rebuilds them"
    )
    inventory = load_json("target-inventory.json")
    ordinary, variant_labels = labels(inventory)
    variant_rules = (
        "lash_rust_feature_library", "lash_rust_feature_binary", "lash_rust_feature_test",
    )
    calls = {}
    for directory in sorted({label[2:].split(":", 1)[0] for label in ordinary | variant_labels}):
        tree = ast.parse((ROOT / directory / "BUCK").read_text(encoding="utf-8"))
        for node in tree.body:
            if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
                    and isinstance(node.value.func, ast.Name)
                    and node.value.func.id in ("lash_rust_library", *variant_rules)):
                continue
            args = {}
            for keyword in node.value.keywords:
                try:
                    args[keyword.arg] = ast.literal_eval(keyword.value)
                except ValueError:
                    pass
            calls[f"//{directory}:{args['name']}"] = (node.value.func.id, args)
    assert variant_labels <= set(calls) | ordinary, sorted(variant_labels - set(calls))[:3]

    def named_deps(label: str) -> dict[str, str]:
        rule, args = calls[label]
        groups = package_deps[args["package_name"]]
        result = dict(groups["normal"])
        if rule == "lash_rust_library":
            return result
        if rule == "lash_rust_feature_test" or args.get("include_dev_deps", False):
            result.update(groups["dev"])
        for extern in args.get("pruned_deps", []):
            result.pop(extern)
        swaps = args.get("variant_deps", {})
        result = {extern: swaps.get(target, target) for extern, target in result.items()}
        for target, extern in args.get("extra_deps", {}).items():
            result[extern] = target
        if args.get("library"):
            result[args["library_crate_name"]] = args["library"]
        return result

    libraries = {
        label: args["package_name"]
        for label, (rule, args) in calls.items()
        if rule in ("lash_rust_library", "lash_rust_feature_library")
    }
    closures: dict[str, frozenset[str]] = {}

    def closure(label: str) -> frozenset[str]:
        if label not in closures:
            reached = set()
            for target in named_deps(label).values():
                if target in libraries:
                    reached.add(target)
                    reached |= closure(target)
            closures[label] = frozenset(reached)
        return closures[label]

    manifests = {}
    requested: dict[str, set[str]] = {}
    with (ROOT / "scripts/feature-coverage.toml").open("rb") as handle:
        for lane in tomllib.load(handle)["lane"]:
            for argv in lane["commands"]:
                package = argv[argv.index("-p") + 1]
                for index, token in enumerate(argv):
                    if token == "--features":
                        requested.setdefault(package, set()).update(
                            value.split("/", 1)[0].removesuffix("?")
                            for value in argv[index + 1].split(",") if "/" in value
                        )

    unactivated, foreign_self, duplicated, optional_edges = [], [], [], 0
    for label in sorted(variant_labels & set(calls)):
        rule, args = calls[label]
        package = args["package_name"]
        if package not in manifests:
            with (ROOT / args["manifest_dir"] / "Cargo.toml").open("rb") as handle:
                manifests[package] = tomllib.load(handle)
        manifest = manifests[package]
        optional = {
            alias for alias, value in manifest.get("dependencies", {}).items()
            if isinstance(value, dict) and value.get("optional", False)
        }
        # `--features dep/feature` would switch an optional dependency on with
        # no feature of the package recording it; no lane does, and the
        # manifest-only derivation below is exact only while that holds.
        assert not optional & requested.get(package, set()), (
            f"{package}: a lane requests a feature of an optional dependency directly"
        )
        declared = manifest.get("features", {})
        activated = set()
        for feature in args["crate_features"]:
            for value in declared.get(feature, [f"dep:{feature}"]):
                if value.startswith("dep:"):
                    activated.add(value[4:])
                elif "/" in value and not value.split("/", 1)[0].endswith("?"):
                    activated.add(value.split("/", 1)[0])
        with_dev = rule == "lash_rust_feature_test" or args.get("include_dev_deps", False)
        allowed = activated | (set(manifest.get("dev-dependencies", {})) if with_dev else set())
        deps = named_deps(label)
        optional_edges += len(optional)
        for alias in sorted(optional - allowed):
            if alias.replace("-", "_") in deps:
                unactivated.append(f"{label} -> {alias}")
        own = [
            target for target in deps.values()
            if libraries.get(target) == package and target != label
        ]
        for target in own:
            if calls[target][1]["crate_features"] != args["crate_features"]:
                foreign_self.append(f"{label} -> {target}")
        reached = set(own) | ({label} if label in libraries else set())
        for target in deps.values():
            if target in libraries:
                reached |= {target} | closure(target)
        copies: dict[str, set[str]] = {}
        for target in reached:
            copies.setdefault(libraries[target], set()).add(target)
        for name, targets in sorted(copies.items()):
            if len(targets) > 1:
                duplicated.append(f"{label}: {name} as {sorted(targets)}")
    assert optional_edges, "no variant of a package with optional dependencies was checked"
    failures = []
    for title, found in (
        ("link an optional dependency their features leave off", unactivated),
        ("link their own package at other features", foreign_self),
        ("link two copies of one first-party package", duplicated),
    ):
        if found:
            variants = {entry.split(" ", 1)[0].rstrip(":") for entry in found}
            failures.append(f"{len(variants)} feature-lane variants {title}, e.g. {found[0]}")
    assert not failures, "; ".join(failures)


def check_feature_lane_test_policy() -> None:
    """A test variant runs under its ordinary label's shard count and timeout.

    The variant is the same binary at another resolution. Run whole with the
    default bound, a suite that package-policy.toml shards because it does not
    fit one action times out in the lane (the RLM unit tests did, at 300 s).
    """
    inventory = load_json("target-inventory.json")
    ordinary, variant_labels = labels(inventory)
    calls = {}
    for directory in sorted({label[2:].split(":", 1)[0] for label in ordinary | variant_labels}):
        tree = ast.parse((ROOT / directory / "BUCK").read_text(encoding="utf-8"))
        for node in tree.body:
            if not (isinstance(node, ast.Expr) and isinstance(node.value, ast.Call)
                    and isinstance(node.value.func, ast.Name)
                    and node.value.func.id in (
                        "lash_rust_unit_test", "lash_rust_integration_test",
                        "lash_rust_feature_test",
                    )):
                continue
            args = {
                keyword.arg: ast.literal_eval(keyword.value)
                for keyword in node.value.keywords
                if keyword.arg in ("name", "shard_count", "timeout")
            }
            calls[f"//{directory}:{args['name']}"] = args
    sharded = 0
    for label in sorted(variant_labels & set(calls)):
        base = calls.get(label.split("__fv_", 1)[0])
        if base is None:
            continue
        for key in ("shard_count", "timeout"):
            assert calls[label].get(key) == base.get(key), (
                f"{label} has {key} {calls[label].get(key)}, its ordinary label {base.get(key)}"
            )
        sharded += "shard_count" in base
    assert sharded, "no feature-lane variant of a sharded test was checked"
    units = {unit["label"]: unit for unit in inventory["feature_lane_units"]}
    for label, args in calls.items():
        if label in units:
            assert units[label].get("shard_count") == args.get("shard_count"), label
    rules = (HERE / "test_rules.bzl").read_text(encoding="utf-8")
    wrapper = rules[rules.index("def lash_test_wrapper("):]
    # Every shard wrapper carries the lane's libtest arguments.
    assert "for index in range(count):" in wrapper and "args = args," in wrapper


def check_shard_weights() -> None:
    """The committed shard weights name sharded tests and reach their wrappers.

    `test_shard.py` balances a test's shards by `test-shard-weights.json`. Each
    row belongs to an ordinary sharded label, which its feature-lane variants
    share; a row for anything else balances nothing and still looks like policy.
    The table is `shard_weights.py --refresh`'s rendering, so a refresh is the
    only diff, and every shard wrapper is handed the table and its own label.
    """
    sys.path.insert(0, str(HERE))
    import shard_weights

    table = load_json("test-shard-weights.json")
    assert (HERE / "test-shard-weights.json").read_text(encoding="utf-8") == shard_weights.render(table), (
        "test-shard-weights.json is not shard_weights.py's rendering"
    )
    inventory = load_json("target-inventory.json")
    sharded = {
        target["label"]
        for package in inventory["packages"]
        for target in package["targets"]
        if target.get("label") and target.get("shard_count", 0) > 1
    }
    assert sharded, "no sharded test in the inventory"
    assert set(table) <= sharded, f"shard weights name no sharded test: {sorted(set(table) - sharded)}"
    for label, row in table.items():
        assert row, f"{label} has an empty shard-weights row"
        for name, weight in row.items():
            assert type(weight) is int and weight > 0, f"{label} {name} weighs {weight!r}, not positive milliseconds"
    assert 'export_file(name = "test_shard_weights", src = "test-shard-weights.json"' in (HERE / "BUCK").read_text(encoding="utf-8")
    rules = (HERE / "test_rules.bzl").read_text(encoding="utf-8")
    wrapper = rules[rules.index("def lash_test_wrapper("):]
    for argument in (
        '"--weights"',
        '"$(location //tools/buck2:test_shard_weights)"',
        '"//{}:{}".format(native.package_name(), name)',
    ):
        assert argument in wrapper, f"shard wrappers no longer pass {argument}"


def check_no_first_party_build_dependency() -> None:
    """No first-party package is a build-dependency of another.

    Ordinary targets take their features from `cargo metadata`'s resolve
    nodes, which report one unified feature set per package. Resolver 2
    resolves a build-dependency separately from the normal edge, so a
    first-party build-dependency would compile at features the generator
    cannot see.
    """
    package_deps = bzl_value((HERE / "deps.bzl").read_text(encoding="utf-8"), "PACKAGE_DEPS")
    first_party = sorted(
        f"{package} -> {target}"
        for package, groups in package_deps.items()
        for target in groups["build"].values()
        if not target.startswith("//third-party/")
    )
    assert not first_party, f"first-party build-dependency edges: {first_party}"


def check_facade_completeness() -> None:
    """The facade test documents exactly the facade's first-party closure."""

    policy = tomllib.loads((HERE / "package-policy.toml").read_text())
    inventory = load_json("target-inventory.json")
    libraries = {
        target["label"]: package["package"]
        for package in inventory["packages"]
        for target in package["targets"]
        if target["kind"] == "lib"
    }
    facade = next(
        label for label, name in libraries.items() if name == policy["facade"]["package"]
    )
    package_deps = bzl_value((HERE / "deps.bzl").read_text(encoding="utf-8"), "PACKAGE_DEPS")
    closure: set[str] = set()
    pending = [facade]
    while pending:
        for label in package_deps[libraries[pending.pop()]]["normal"].values():
            if label in libraries and label not in closure:
                closure.add(label)
                pending.append(label)
    package_dir, name = facade.removeprefix("//").split(":")
    rules = (ROOT / package_dir / "BUCK").read_text(encoding="utf-8")
    calls = [
        {keyword.arg: ast.literal_eval(keyword.value) for keyword in statement.value.keywords}
        for statement in ast.parse(rules).body
        if isinstance(statement, ast.Expr)
        and isinstance(statement.value, ast.Call)
        and getattr(statement.value.func, "id", "") == "facade_completeness_test"
    ]
    assert len(calls) == 1, f"expected one facade_completeness_test in {package_dir}/BUCK"
    (call,) = calls
    assert call["name"] == "facade_completeness" and call["facade"] == f":{name}"
    assert "manual" in call["tags"]
    assert set(call["libraries"]) == closure, (
        "facade_completeness libraries differ from the facade's first-party closure: "
        f"missing {sorted(closure - set(call['libraries']))}, "
        f"extra {sorted(set(call['libraries']) - closure)}"
    )
    root = (ROOT / "BUCK").read_text(encoding="utf-8")
    assert 'name = "scripts/facade_completeness.py"' in root
    rule = (HERE / "facade_completeness.bzl").read_text(encoding="utf-8")
    assert 'sub_targets["doc-json"]' in rule
    assert "run `kiln sync`" in rule
    assert "supports_test_execution_caching = True" in rule
    overlay = (HERE / "prelude_overlay.py").read_text(encoding="utf-8")
    for flag in ("--output-format=json", "--document-hidden-items", "RUSTC_BOOTSTRAP"):
        assert flag in overlay.split("RUSTDOC_JSON_FLAGS = ", 1)[1].split("\n'''", 1)[0]
    assert 'targets["doc-json"] = rustdoc_json' in overlay


def check_vm_worker_runfiles() -> None:
    """Every linked spawner consumer declares its matching runtime helper."""
    import sync
    import vm_worker_runfiles

    metadata = sync.metadata()
    members = set(metadata["workspace_members"])
    outputs = {}
    for package in metadata["packages"]:
        if package["id"] in members:
            path = pathlib.Path(package["manifest_path"]).parent / "BUCK"
            outputs[path] = path.read_text(encoding="utf-8")
    failures = vm_worker_runfiles.check(metadata, outputs, ROOT)
    assert not failures, "\n".join(failures)


def main() -> int:
    checks = [
        check_inventory,
        check_sizing,
        check_queue_priority,
        check_action_categories,
        check_ownership,
        check_action_bridge,
        check_transitive_source_inputs,
        check_repo_rooted_source_remap,
        check_failure_filter_runs_in_daemon,
        check_measurement_filter,
        check_target_kind_rule,
        check_native_inputs,
        check_dependency_and_profile_projection,
        check_sync_receipt,
        check_clippy_receipt_inputs,
        check_rust_edit_receipt_inputs,
        check_external_buildscripts,
        check_buildscript_metadata_bridge,
        check_direct_buck_generator,
        check_schema_source_inputs,
        check_feature_lane_executable_selection,
        check_documentation_targets_are_not_tests,
        check_feature_lane_dependency_edges,
        check_feature_lane_test_policy,
        check_shard_weights,
        check_no_first_party_build_dependency,
        check_facade_completeness,
        check_vm_worker_runfiles,
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
