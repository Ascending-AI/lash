import importlib.util
import pathlib
import shutil
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "check_judged_build_geometry", ROOT / "scripts" / "check_judged_build_geometry.py"
)
assert SPEC and SPEC.loader
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)


class JudgedBuildGeometryTests(unittest.TestCase):
    def setUp(self) -> None:
        self._original_root = GATE.ROOT
        self._tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self._tmp.name)
        for name in ("Cargo.toml", "justfile"):
            shutil.copy2(ROOT / name, self.root / name)
        (self.root / "tools/buck2").mkdir(parents=True)
        for name in (
            "BUCK",
            "clippy_policy.bzl",
            "host_transition.bzl",
            "lash_rust.bzl",
            "profile.bzl",
            "third_party.bzl",
        ):
            shutil.copy2(ROOT / "tools/buck2" / name, self.root / "tools/buck2" / name)
        # The gate imports `driver.py`, which imports its sibling modules, so
        # the fixture carries every Python module of the directory.
        for module in sorted((ROOT / "tools/buck2").glob("*.py")):
            shutil.copy2(module, self.root / "tools/buck2" / module.name)
        (self.root / "tools/buck2/toolchains").mkdir()
        shutil.copy2(
            ROOT / "tools/buck2/toolchains/rust.bzl",
            self.root / "tools/buck2/toolchains/rust.bzl",
        )
        for relative in ("examples", "scripts", "runbooks"):
            shutil.copytree(
                ROOT / relative,
                self.root / relative,
                ignore=shutil.ignore_patterns(
                    "target", "node_modules", "frontend", "__pycache__", "*.pyc"
                ),
            )
        GATE.ROOT = self.root

    def tearDown(self) -> None:
        GATE.ROOT = self._original_root
        self._tmp.cleanup()

    def run_gate(self) -> list[str]:
        failures: list[str] = []
        GATE.check_profile(failures)
        GATE.check_no_runtime_testing_features(failures)
        GATE.check_boot_sites(failures)
        GATE.check_artifact_dirs(failures)
        GATE.check_profile_overrides_exported(failures)
        GATE.check_buck2_judged_config(failures)
        GATE.check_buck2_optimized_config(failures)
        GATE.check_monty_optimized_config(failures)
        GATE.check_buck2_boot_sites(failures)
        GATE.check_build_precedes_launcher_locks(failures)
        return failures

    def test_repository_tree_is_clean(self) -> None:
        self.assertEqual(self.run_gate(), [])

    def test_missing_judged_profile_fails(self) -> None:
        manifest = self.root / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace("[profile.judged", "[profile.unused"),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("[profile.judged] is missing" in f for f in failures), failures)

    def test_debug_assertions_left_on_fails(self) -> None:
        manifest = self.root / "Cargo.toml"
        manifest.write_text(
            manifest.read_text(encoding="utf-8").replace(
                "debug-assertions = false", "debug-assertions = true"
            ),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("debug-assertions" in f for f in failures), failures)

    def test_runtime_testing_feature_on_a_judged_host_fails(self) -> None:
        manifest = self.root / "examples" / "agent-workbench" / "Cargo.toml"
        text = manifest.read_text(encoding="utf-8")
        text = text.replace(
            'lash = { workspace = true, features = ["rlm", "sqlite", "postgres", "mcp", "subagents", "http-transport", "openai"] }',
            'lash = { workspace = true, features = ["rlm", "sqlite", "postgres", "mcp", "subagents", "http-transport", "openai", "testing"] }',
            1,
        )
        manifest.write_text(text, encoding="utf-8")
        failures = self.run_gate()
        self.assertTrue(
            any("enables the `testing` feature" in f for f in failures), failures
        )









    def test_buck2_judged_config_without_debug_assertions_fails(self) -> None:
        # The defect this exists for: rustc turns debug assertions ON at
        # `-C opt-level=0`, so a Buck2 config that simply omits the flag ships
        # the geometry `[profile.judged]` was created to remove — and the host
        # still boots, so nothing else notices.
        rules = self.root / "tools/buck2/lash_rust.bzl"
        text = rules.read_text(encoding="utf-8")
        flag_line = '"-Cdebug-assertions=no",'
        self.assertIn(flag_line, text)
        rules.write_text(text.replace(flag_line, "", 1), encoding="utf-8")
        failures = self.run_gate()
        self.assertTrue(any("first-party target rustc_flags" in f for f in failures), failures)

    def test_unused_judged_flag_strings_do_not_satisfy_the_gate(self) -> None:
        rules = self.root / "tools/buck2/lash_rust.bzl"
        rules.write_text(
            'unused = ["-Cdebug-assertions=no", "-Coverflow-checks=no"]\n',
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("first-party target rustc_flags" in f for f in failures), failures)

    def test_judged_select_cannot_change_optimization_geometry(self) -> None:
        rules = self.root / "tools/buck2/lash_rust.bzl"
        text = rules.read_text(encoding="utf-8")
        flag = '"-Cdebug-assertions=no",'
        self.assertIn(flag, text)
        rules.write_text(
            text.replace(flag, flag + '\n            "-Copt-level=3",', 1),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("first-party target rustc_flags" in f for f in failures), failures)

    def test_judged_select_not_returned_to_rustc_flags_fails(self) -> None:
        rules = self.root / "tools/buck2/lash_rust.bzl"
        text = rules.read_text(encoding="utf-8")
        self.assertIn(" + judged", text)
        rules.write_text(text.replace(" + judged", "", 1), encoding="utf-8")
        failures = self.run_gate()
        self.assertTrue(any("first-party target rustc_flags" in f for f in failures), failures)

    def test_third_party_judged_select_not_appended_fails(self) -> None:
        rules = self.root / "tools/buck2/third_party.bzl"
        text = rules.read_text(encoding="utf-8")
        constraint = '"//tools/buck2:profile_judged"'
        self.assertIn(constraint, text)
        rules.write_text(
            text.replace(constraint, '"//tools/buck2:profile_unused"', 1),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("third-party target rustc_flags" in f for f in failures), failures)

    def test_driver_judged_config_must_select_the_judged_platform(self) -> None:
        driver = self.root / "tools/buck2/driver.py"
        text = driver.read_text(encoding="utf-8")
        expression = "'//tools/buck2:' + options.config"
        self.assertIn(expression, text)
        driver.write_text(
            text.replace(expression, "'//tools/buck2:ordinary'", 1),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("--config=judged does not select" in f for f in failures), failures)

    def test_monty_uses_the_optimized_configuration(self) -> None:
        script = self.root / "scripts/profile_monty_comparison.sh"
        text = script.read_text(encoding="utf-8")
        self.assertIn("--config=optimized", text)
        script.write_text(
            text.replace("--config=optimized", "--config=judged", 1),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("Buck2 optimized configuration" in f for f in failures), failures)

    def test_optimized_profile_flags_match_the_repository_profile(self) -> None:
        rules = self.root / "tools/buck2/lash_rust.bzl"
        text = rules.read_text(encoding="utf-8")
        self.assertIn('"-Copt-level=3",', text)
        rules.write_text(
            text.replace('"-Copt-level=3",', '"-Copt-level=2",', 1),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("optimized Rust flags" in f for f in failures), failures)

    def test_toolchain_cannot_override_package_opt_level_after_rules(self) -> None:
        rules = self.root / "tools/buck2/toolchains/rust.bzl"
        text = rules.read_text(encoding="utf-8")
        assignment = "extra_rustc_flags = []"
        self.assertIn(assignment, text)
        rules.write_text(
            text.replace(assignment, 'extra_rustc_flags = ["-Copt-level=3"]', 1),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("after package overrides" in f for f in failures), failures)

    def test_lash_regress_target_override_remains_effective(self) -> None:
        profile = self.root / "tools/buck2/profile.bzl"
        text = profile.read_text(encoding="utf-8")
        self.assertIn('"lash-regress": 2', text)
        profile.write_text(
            text.replace('"lash-regress": 2', '"lash-regress": 3', 1),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("lash-regress target opt2" in f for f in failures), failures)

    def test_host_profile_suppresses_target_package_override(self) -> None:
        rules = self.root / "tools/buck2/lash_rust.bzl"
        text = rules.read_text(encoding="utf-8")
        host = '"//tools/buck2:profile_host": [],'
        self.assertIn(host, text)
        rules.write_text(
            text.replace(host, '"//tools/buck2:profile_host": ["-Copt-level=2"],', 1),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("target/host opt3" in f for f in failures), failures)

    def test_host_transition_applies_the_optimized_host_profile(self) -> None:
        transition = self.root / "tools/buck2/host_transition.bzl"
        text = transition.read_text(encoding="utf-8")
        assignment = "constraints[host.setting.label] = host"
        self.assertIn(assignment, text)
        transition.write_text(
            text.replace(assignment, "constraints.pop(host.setting.label, None)", 1),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("profile_host geometry" in f for f in failures), failures)

    def test_third_party_optimized_profile_reaches_rustc_flags(self) -> None:
        rules = self.root / "tools/buck2/third_party.bzl"
        text = rules.read_text(encoding="utf-8")
        constraint = '"//tools/buck2:profile_optimized": _OPTIMIZED_FLAGS,'
        self.assertIn(constraint, text)
        rules.write_text(
            text.replace(
                constraint,
                '"//tools/buck2:profile_optimized": [],',
                1,
            ),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("target/host opt3" in f for f in failures), failures)

    def test_optimized_flags_reach_build_script_commands_and_environment(self) -> None:
        overlay = self.root / "tools/buck2/prelude_overlay.py"
        text = overlay.read_text(encoding="utf-8")
        combined = "rust_toolchain_info.rustc_flags + ctx.attrs.cargo_rustc_flags"
        self.assertIn(combined, text)
        overlay.write_text(
            text.replace(combined, "rust_toolchain_info.rustc_flags", 1),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("effective order" in f for f in failures), failures)

    def test_optimized_driver_preserves_profile_across_exec_transitions(self) -> None:
        driver = self.root / "tools/buck2/driver.py"
        text = driver.read_text(encoding="utf-8")
        self.assertIn("kiln.rust_profile=optimized", text)
        driver.write_text(
            text.replace("kiln.rust_profile=optimized", "kiln.rust_profile=ordinary", 1),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("across Rust exec transitions" in f for f in failures), failures)

    def test_buck2_build_without_the_judged_config_fails(self) -> None:
        script = self.root / "scripts" / "agent-workbench-dev.sh"
        text = script.read_text(encoding="utf-8")
        invocation = '"${build_command[@]}" --config=judged '
        self.assertIn(invocation, text)
        script.write_text(
            text.replace(invocation, '"${build_command[@]}" ', 1), encoding="utf-8"
        )
        failures = self.run_gate()
        self.assertTrue(
            any("without `--config=judged`" in f for f in failures), failures
        )

    def test_build_taken_inside_a_launcher_lock_fails(self) -> None:
        # The measured defect: the launcher compiled while holding
        # `/tmp/lash-agent-workbench-$UID/data-ownership.lock`, which every
        # checkout on the box shares, so one lane's cold build was every other
        # stack's boot latency.
        script = self.root / "scripts" / "agent-workbench-dev.sh"
        text = script.read_text(encoding="utf-8")
        call = (
            "      foreground|run|restart)\n"
            "        prepare_workbench_binary\n"
            "        ;;\n"
        )
        self.assertIn(call, text)
        anchor = '    exec {launcher_data_lock_fd}>"$launcher_data_lock_file"\n'
        self.assertIn(anchor, text)
        text = text.replace(call, "", 1).replace(anchor, anchor + call, 1)
        script.write_text(text, encoding="utf-8")
        failures = self.run_gate()
        self.assertTrue(
            any("builds the host after a launcher lock is taken" in f for f in failures),
            failures,
        )


if __name__ == "__main__":
    unittest.main()
