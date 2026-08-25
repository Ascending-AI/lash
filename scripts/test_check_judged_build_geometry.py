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
        for relative in ("examples", "scripts", "runbooks"):
            shutil.copytree(
                ROOT / relative,
                self.root / relative,
                ignore=shutil.ignore_patterns("target", "node_modules", "frontend"),
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
            'lash = { workspace = true, features = ["rlm"] }',
            'lash = { workspace = true, features = ["rlm", "testing"] }',
            1,
        )
        manifest.write_text(text, encoding="utf-8")
        failures = self.run_gate()
        self.assertTrue(
            any("enables the `testing` feature" in f for f in failures), failures
        )

    def test_shared_launcher_defaulting_away_from_judged_fails(self) -> None:
        script = self.root / "scripts" / "slack-clone-dev.sh"
        script.write_text(
            script.read_text(encoding="utf-8").replace(
                'cargo_profile="${SLACK_CLONE_CARGO_PROFILE:-judged}"',
                'cargo_profile="${SLACK_CLONE_CARGO_PROFILE:-dev}"',
            ),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("without `--profile judged`" in f for f in failures), failures)

    def test_boot_without_the_judged_profile_fails(self) -> None:
        script = self.root / "scripts" / "agent-workbench-dev.sh"
        script.write_text(
            script.read_text(encoding="utf-8").replace(
                "cargo build -p agent-workbench --profile judged",
                "cargo build -p agent-workbench",
            ),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(any("without `--profile judged`" in f for f in failures), failures)


    def test_profile_pasted_into_the_artifact_path_fails(self) -> None:
        # The exact defect this check exists for: `--profile dev` puts artifacts
        # in `target/debug`, so a launcher that pastes the profile name straight
        # into the path boots `target/dev/<bin>`, which never exists.
        script = self.root / "scripts" / "slack-clone-dev.sh"
        text = script.read_text(encoding="utf-8")
        text = text.replace(
            '"$(profile_artifact_dir)"', '"$cargo_profile"'
        ).replace("    dev) printf 'debug' ;;\n", "")
        script.write_text(text, encoding="utf-8")
        failures = self.run_gate()
        self.assertTrue(
            any("never maps `dev` to" in f for f in failures), failures
        )

    def test_literal_dev_artifact_directory_fails(self) -> None:
        script = self.root / "scripts" / "agent-workbench-dev.sh"
        script.write_text(
            script.read_text(encoding="utf-8").replace(
                '$repo_root/target}/judged/agent-workbench',
                '$repo_root/target}/dev/agent-workbench',
            ),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(
            any("cargo writes the `dev` profile" in f for f in failures), failures
        )

    def test_undeclared_artifact_directory_fails(self) -> None:
        script = self.root / "scripts" / "agent-workbench-dev.sh"
        script.write_text(
            script.read_text(encoding="utf-8").replace(
                '$repo_root/target}/judged/agent-workbench',
                '$repo_root/target}/shipping/agent-workbench',
            ),
            encoding="utf-8",
        )
        failures = self.run_gate()
        self.assertTrue(
            any("not a cargo artifact directory" in f for f in failures), failures
        )


    def test_profile_override_scoped_to_one_command_fails(self) -> None:
        # A `VAR=value cmd` prefix reaches the boot command only; the Python
        # driver's mid-gate restart then rebuilds under the other profile.
        script = self.root / "scripts" / "slack-clone-full-host-e2e.sh"
        text = script.read_text(encoding="utf-8")
        text = text.replace(
            "export SLACK_CLONE_CARGO_PROFILE=dev",
            "",
        ).replace(
            'bash "$repo/scripts/slack-clone-dev.sh" up --port "$port"',
            'SLACK_CLONE_CARGO_PROFILE=dev bash "$repo/scripts/slack-clone-dev.sh" up --port "$port"',
        )
        script.write_text(text, encoding="utf-8")
        failures = self.run_gate()
        self.assertTrue(
            any("without `export`" in f for f in failures), failures
        )


if __name__ == "__main__":
    unittest.main()
