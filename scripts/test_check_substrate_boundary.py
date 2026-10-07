#!/usr/bin/env python3

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check-substrate-boundary.sh"


# Every path the script hands to rg must exist in a fixture tree, or the
# search exits nonzero and the check reports a search failure instead of a
# rule verdict. Glob entries need a file that satisfies them.
FIXTURE_DIRS = [
    "crates/lash-core/src/runtime/turn_loop",
    "crates/lash-core/src/runtime/turn_driver",
    "crates/lash-core-ids/src",
    "crates/lash-core-llm/src",
    "crates/lash/src",
    "crates/lash-core-execution/src/session",
    "crates/lash-core-execution/src/tool_dispatch",
    "crates/lash-core-execution/src/runtime/effect",
    "crates/lash-protocol-rlm/src/executor",
    "crates/lash-protocol-rlm/src/projection",
    "crates/lashlang/src",
    "crates/lash-lashlang-runtime/src",
]
FIXTURE_FILES = [
    "crates/lash-core/src/runtime/logical_turn.rs",
    "crates/lash-core/src/runtime/turn_boundary.rs",
    "crates/lash-core-execution/src/session.rs",
    "crates/lash-core-execution/src/tool_dispatch.rs",
]
COLLAPSED_NAMES = (
    "EffectEngine", "EffectHost", "RuntimeEffectController", "ScopedEffectController",
    "EffectTaskController", "LayeredEngine", "EffectLayer", "LayeredEffectHost",
    "AwaitEventResolver",
)
FIXTURE_COLLAPSE_FILE = "crates/lash-core/src/runtime/turn_loop/drive.rs"
GENERATION_NAMES = (
    "BuildGeneration", "EngineGeneration", "JOURNAL_LOGIC_EPOCH", "generation_drain",
    "fleet_finalize", "DeploymentRegistry", "draining_generations", "generation_fence",
)
FIXTURE_GENERATION_FILE = "crates/lash-core-store/src/store/park.rs"
SHIFT_NAMES = (
    "ShiftFence", "seal_shift_epoch", "RunStartNonce", "RunHold", "ShiftHold", "ShiftLoop",
    "ShiftRequest", "shift_epoch", "lash_session_shift_admissions",
    "ck_session_meta_shift_authority", "lash_turn_parks", "lash_turn_park_clock",
    "lash_turn_park_events", "TurnParkWrite",
)
FIXTURE_DEFAULTS_FILE = "crates/lash-core-execution/src/runtime/effect/engine.rs"


class SubstrateBoundaryTests(unittest.TestCase):
    def run_check(self, cwd: Path) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(cwd / "scripts" / SCRIPT.name)],
            cwd=cwd,
            text=True,
            capture_output=True,
            check=False,
        )

    def build_fixture(self, root: Path) -> None:
        scripts = root / "scripts"
        scripts.mkdir(parents=True)
        shutil.copy2(SCRIPT, scripts / SCRIPT.name)
        for directory in FIXTURE_DIRS:
            (root / directory).mkdir(parents=True)
        for file in FIXTURE_FILES:
            path = root / file
            path.parent.mkdir(parents=True, exist_ok=True)
            path.touch()

    def test_the_boundary_passes_on_the_tree(self) -> None:
        result = subprocess.run(
            ["bash", str(SCRIPT)],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def write_rust(self, root: Path, relative: str, lines: list[str]) -> None:
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("\n".join(lines) + "\n")

    def test_a_deleted_effect_seam_name_fails(self) -> None:
        for name in COLLAPSED_NAMES:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                self.build_fixture(root)
                self.write_rust(root, FIXTURE_COLLAPSE_FILE, [f"fn drive(host: &dyn {name}) {{}}"])
                result = self.run_check(root)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("rule 7 failed: a deleted effect-seam name", result.stderr)

    def test_a_deleted_shift_fence_name_fails(self) -> None:
        for name in SHIFT_NAMES:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                self.build_fixture(root)
                self.write_rust(root, FIXTURE_COLLAPSE_FILE, [f"fn admit(fence: &{name}) {{}}"])
                result = self.run_check(root)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("rule 7d failed: a deleted shift-fence or turn-park name", result.stderr)

    def test_a_shift_fence_name_in_a_comment_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.build_fixture(root)
            self.write_rust(root, FIXTURE_COLLAPSE_FILE, [
                "// Version 128 reshaped `lash_turn_parks`; the shift_fence is gone.",
                "fn admit() {}",
            ])
            result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_the_effect_controller_error_variants_pass(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.build_fixture(root)
            self.write_rust(root, FIXTURE_COLLAPSE_FILE, [
                "fn widen(error: RuntimeEffectControllerError) -> PluginError {",
                "    crate::PluginError::RuntimeEffectController(error)",
                "}",
                "fn kind(error: &PluginError) -> TurnFailureKind {",
                "    match error {",
                "        Self::RuntimeEffectController(_) => TurnFailureKind::RuntimeEffectController,",
                "    }",
                "}",
                "fn lift(result: Result<(), RuntimeEffectControllerError>) {",
                "    let _ = result.map_err(crate::PluginError::RuntimeEffectController);",
                "}",
            ])
            result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_deleted_generation_name_fails(self) -> None:
        for name in GENERATION_NAMES:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                self.build_fixture(root)
                self.write_rust(root, FIXTURE_GENERATION_FILE, [f"fn stamp(_: &{name}) {{}}"])
                result = self.run_check(root)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("rule 7c failed: a deleted generation-lane name", result.stderr)

    def test_a_table_name_embedding_a_deleted_generation_name_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.build_fixture(root)
            self.write_rust(root, FIXTURE_GENERATION_FILE, [
                'const TABLE: &str = "lash_draining_generations";',
            ])
            result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rule 7c failed", result.stderr)

    def test_surviving_generation_vocabulary_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.build_fixture(root)
            self.write_rust(root, FIXTURE_GENERATION_FILE, [
                "fn park(_: ExecutableGeneration, _: SessionStateGeneration) {}",
                "fn reason(_: ParkReason) -> bool { matches!(_, ParkReason::RetiredGeneration { .. }) }",
            ])
            result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_default_body_on_a_no_default_trait_fails(self) -> None:
        for trait in ("ProcessEngine", "ProjectionProvider", "DurableStore"):
            with self.subTest(trait=trait), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                self.build_fixture(root)
                self.write_rust(root, FIXTURE_DEFAULTS_FILE, [
                    "#[async_trait::async_trait]",
                    f"pub trait {trait}: Send + Sync {{",
                    "    fn kind(&self) -> &'static str;",
                    "    async fn resolve(",
                    "        &self,",
                    "        reference: &Reference,",
                    "    ) -> Result<Resolution, Refusal> {",
                    "        let _ = reference;",
                    "        Ok(Resolution::unknown())",
                    "    }",
                    "}",
                ])
                result = self.run_check(root)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"{FIXTURE_DEFAULTS_FILE}:4:{trait} has a default method body", result.stderr)

    def test_required_methods_and_other_traits_defaults_pass(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.build_fixture(root)
            self.write_rust(root, FIXTURE_DEFAULTS_FILE, [
                "pub trait ProcessEngine: Send + Sync {",
                "    fn kind(&self) -> &'static str;",
                "    fn advance(",
                "        &self,",
                "        state: EngineState,",
                "    ) -> Result<(EngineState, EngineAction), ProcessInfraError>;",
                "}",
                "",
                "impl ProcessEngine for Held {",
                "    fn kind(&self) -> &'static str {",
                "        \"held\"",
                "    }",
                "}",
                "",
                "pub trait ProcessEngineExt {",
                "    fn label(&self) -> String {",
                "        String::new()",
                "    }",
                "}",
            ])
            result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
