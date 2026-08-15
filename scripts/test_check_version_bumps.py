from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest


SCRIPT = Path(__file__).with_name("check_version_bumps.py")
SPEC = importlib.util.spec_from_file_location("check_version_bumps", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


CONFIG = """
[[surface]]
constant = "WIRE_VERSION"
constant_path = "src/lib.rs"
description = "fixture wire enum"

[[surface.guard]]
kind = "rust_serde_shapes"
paths = ["src/wire.rs"]
must_cover = ["WireMessage"]
"""

REMOTE_CONFIG = """
[[surface]]
constant = "REMOTE_PROTOCOL_VERSION"
constant_path = "src/lib.rs"
description = "incident 1 remote wire surface"

[[surface.guard]]
kind = "rust_serde_shapes"
paths = ["src/usage_activity.rs"]
must_cover = ["RemoteTurnEvent"]
"""

POSTGRES_CONFIG = """
[[surface]]
constant = "SCHEMA_VERSION"
constant_path = "src/lib.rs"
description = "incident 2 PostgreSQL schema surface"

[[surface.guard]]
kind = "file"
paths = ["schema.sql"]
must_cover = ["lash_session_execution_leases"]
"""

TRACE_CONFIG = """
[[surface]]
constant = "TRACE_SCHEMA_VERSION"
constant_path = "src/lib.rs"
description = "incident 3 trace event surface"

[[surface.guard]]
kind = "rust_serde_shapes"
paths = ["src/trace.rs"]
must_cover = ["TraceEvent"]
"""

LIB_V1 = "pub const WIRE_VERSION: u32 = 1;\n"
LIB_V2 = "pub const WIRE_VERSION: u32 = 2;\n"
WIRE_BASE = """
#[derive(Serialize, Deserialize)]
pub enum WireMessage {
    Existing,
}
"""
WIRE_CHANGED = """
#[derive(Serialize, Deserialize)]
pub enum WireMessage {
    Existing,
    Added,
}
"""

TRACE_EVENT_BASE = """
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "TraceEvent is a public DTO; keeping event payloads inline preserves ergonomic pattern matching"
)]
pub enum TraceEvent {
    SessionStarted,
    PromptBuilt {
        prompt_hash: String,
        components: Vec<TracePromptComponent>,
    },
}
"""
TRACE_EVENT_COMPOSITION_CHANGED = TRACE_EVENT_BASE.replace(
    "    PromptBuilt {",
    """    /// Complete model-facing composition captured only when its fingerprint
    /// changes for a resident session.
    CompositionChanged {
        /// SHA-256 of the rendered system prompt plus ordered fingerprints of
        /// the model-facing tool contracts.
        fingerprint: String,
        rendered_system_prompt: String,
        /// Full model-facing tool contracts in request order. This is kept
        /// even when empty so the event is a self-contained snapshot.
        tool_schemas: Vec<TraceToolSpec>,
    },
    PromptBuilt {""",
)

REMOTE_PROCESS_INPUT = """
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
// justification: this public remote DTO preserves its source-compatible inline SessionTurn construction and matching API.
#[allow(clippy::large_enum_variant)]
pub enum RemoteProcessInput {
    ToolCall { prepared_tool_call: serde_json::Value },
}
"""

REMOTE_TURN_EVENT_BASE = """
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteTurnEvent {
    FinalValue { value: serde_json::Value },
}
"""
REMOTE_TURN_EVENT_TOOL_INTENT = REMOTE_TURN_EVENT_BASE.replace(
    "    FinalValue",
    """    ToolIntentOutcome {
        call_id: String,
        outcome: RemoteToolIntentExecutionOutcome,
    },
    FinalValue""",
)

POSTGRES_SCHEMA_BASE = """
CREATE TABLE IF NOT EXISTS lash_session_execution_leases (
    session_id TEXT PRIMARY KEY,
    lease_owner_id TEXT,
    lease_owner_incarnation_id TEXT,
    lease_executor_id TEXT,
    lease_owner_liveness_json TEXT,
    lease_token TEXT,
    lease_fencing_token BIGINT NOT NULL DEFAULT 0,
    lease_claimed_at_ms BIGINT NOT NULL DEFAULT 0,
    lease_term_ms BIGINT NOT NULL DEFAULT 0,
    lease_expires_at_ms BIGINT NOT NULL DEFAULT 0
);
"""
POSTGRES_SCHEMA_CHANGED = POSTGRES_SCHEMA_BASE.replace(
    "    lease_executor_id TEXT,\n", ""
).replace("    lease_term_ms BIGINT NOT NULL DEFAULT 0,\n", "")


def run(repo: Path, *args: str) -> str:
    result = subprocess.run(
        [*args], cwd=repo, check=True, capture_output=True, text=True
    )
    return result.stdout.strip()


class FixtureRepository:
    def __init__(self, config: str = CONFIG) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        run(self.root, "git", "init", "-q")
        run(self.root, "git", "config", "user.name", "Fixture")
        run(self.root, "git", "config", "user.email", "fixture@example.invalid")
        (self.root / "src").mkdir()
        (self.root / "surface.toml").write_text(
            textwrap.dedent(config), encoding="utf-8"
        )

    def write_file(self, path: str, content: str) -> None:
        destination = self.root / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(textwrap.dedent(content), encoding="utf-8")

    def write(self, version: str, wire: str) -> None:
        self.write_file("src/lib.rs", version)
        self.write_file("src/wire.rs", wire)

    def commit(self, message: str) -> str:
        run(self.root, "git", "add", ".")
        run(self.root, "git", "commit", "-q", "-m", message)
        return run(self.root, "git", "rev-parse", "HEAD")

    def close(self) -> None:
        self.temporary.cleanup()


class VersionBumpFixtureTest(unittest.TestCase):
    def fixture(self) -> FixtureRepository:
        fixture = FixtureRepository()
        self.addCleanup(fixture.close)
        return fixture

    def check(self, fixture: FixtureRepository, base: str, head: str):
        surfaces = MODULE.load_config(fixture.root / "surface.toml")
        return MODULE.check_surfaces(fixture.root, base, head, surfaces)

    def check_cli(
        self, fixture: FixtureRepository, base: str, head: str
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--repo",
                str(fixture.root),
                "--config",
                str(fixture.root / "surface.toml"),
                "--base",
                base,
                "--head",
                head,
            ],
            cwd=fixture.root,
            capture_output=True,
            text=True,
        )

    def test_cli_exit_contract(self) -> None:
        cases = (
            (
                "surface error",
                2,
                LIB_V1,
                WIRE_BASE.replace(
                    "#[derive(Serialize, Deserialize)]", "#[derive(Clone, Debug)]"
                ),
            ),
            ("bump violation", 1, LIB_V1, WIRE_CHANGED),
            ("clean", 0, LIB_V2, WIRE_CHANGED),
        )
        for name, expected_exit, head_version, head_wire in cases:
            with self.subTest(name=name):
                fixture = self.fixture()
                fixture.write(LIB_V1, WIRE_BASE)
                base = fixture.commit("base")
                fixture.write(head_version, head_wire)
                head = fixture.commit(name)

                result = self.check_cli(fixture, base, head)

                self.assertEqual(result.returncode, expected_exit, result.stderr)

    def test_wire_variant_without_bump_fails(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(LIB_V1, WIRE_CHANGED)
        head = fixture.commit("add wire variant without bump")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)
        self.assertEqual(result.failures[0].surface.constant, "WIRE_VERSION")
        self.assertEqual(result.failures[0].base_version, 1)
        self.assertEqual(result.failures[0].head_version, 1)

    def test_wire_variant_with_bump_passes(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(LIB_V2, WIRE_CHANGED)
        head = fixture.commit("add wire variant and bump")

        self.assertEqual(self.check(fixture, base, head), MODULE.CheckResult((), ()))

    def test_bump_without_wire_change_passes(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(LIB_V2, WIRE_BASE)
        head = fixture.commit("reserve next version")

        self.assertEqual(self.check(fixture, base, head), MODULE.CheckResult((), ()))

    def test_unrelated_code_in_wire_file_passes(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE + "\nfn helper() -> u8 { 1 }\n")
        base = fixture.commit("base")
        fixture.write(LIB_V1, WIRE_BASE + "\nfn helper() -> u8 { 2 }\n")
        head = fixture.commit("change unrelated helper")

        self.assertEqual(self.check(fixture, base, head), MODULE.CheckResult((), ()))

    def test_trace_event_multiline_attribute_is_detected(self) -> None:
        shapes = MODULE.serde_shapes(textwrap.dedent(TRACE_EVENT_BASE))

        self.assertIn("TraceEvent", shapes)

    def test_remote_process_input_comment_between_attributes_is_detected(self) -> None:
        shapes = MODULE.serde_shapes(textwrap.dedent(REMOTE_PROCESS_INPUT))

        self.assertIn("RemoteProcessInput", shapes)

    def test_rust_items_share_comment_skipping_attribute_walk(self) -> None:
        items = MODULE.named_rust_items(
            textwrap.dedent(REMOTE_PROCESS_INPUT), ["RemoteProcessInput"]
        )

        self.assertIn("#[serde(tag=\"type\",rename_all=\"snake_case\")]", items["RemoteProcessInput"])

    def test_incident_1_remote_wire_change_without_bump_fails(self) -> None:
        fixture = FixtureRepository(REMOTE_CONFIG)
        self.addCleanup(fixture.close)
        fixture.write_file(
            "src/lib.rs", "pub const REMOTE_PROTOCOL_VERSION: u32 = 34;\n"
        )
        fixture.write_file("src/usage_activity.rs", REMOTE_TURN_EVENT_BASE)
        base = fixture.commit("incident 1 base")
        fixture.write_file("src/usage_activity.rs", REMOTE_TURN_EVENT_TOOL_INTENT)
        head = fixture.commit("typed tool-intent wire change without bump")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)
        self.assertEqual(result.failures[0].base_version, 34)
        self.assertEqual(result.failures[0].head_version, 34)

    def test_incident_2_postgres_schema_change_without_bump_fails(self) -> None:
        fixture = FixtureRepository(POSTGRES_CONFIG)
        self.addCleanup(fixture.close)
        fixture.write_file("src/lib.rs", "const SCHEMA_VERSION: i32 = 50;\n")
        fixture.write_file("schema.sql", POSTGRES_SCHEMA_BASE)
        base = fixture.commit("incident 2 base")
        fixture.write_file("schema.sql", POSTGRES_SCHEMA_CHANGED)
        head = fixture.commit("lease column removals without bump")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)
        self.assertEqual(result.failures[0].base_version, 50)
        self.assertEqual(result.failures[0].head_version, 50)

    def test_incident_3_composition_changed_without_bump_fails(self) -> None:
        fixture = FixtureRepository(TRACE_CONFIG)
        self.addCleanup(fixture.close)
        fixture.write_file(
            "src/lib.rs", "pub const TRACE_SCHEMA_VERSION: u32 = 4;\n"
        )
        fixture.write_file("src/trace.rs", TRACE_EVENT_BASE)
        base = fixture.commit("incident 3 base")
        fixture.write_file("src/trace.rs", TRACE_EVENT_COMPOSITION_CHANGED)
        head = fixture.commit("composition_changed with version pinned to 4")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)
        self.assertEqual(result.failures[0].surface.constant, "TRACE_SCHEMA_VERSION")
        self.assertEqual(result.failures[0].base_version, 4)
        self.assertEqual(result.failures[0].head_version, 4)

    def test_missing_must_cover_shape_at_head_is_an_error(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(
            LIB_V1,
            WIRE_BASE.replace(
                "#[derive(Serialize, Deserialize)]", "#[derive(Clone, Debug)]"
            ),
        )
        head = fixture.commit("remove must-cover shape from detection")

        result = self.check(fixture, base, head)

        self.assertEqual(result.failures, ())
        self.assertEqual(len(result.errors), 1)
        self.assertIn("WireMessage", result.errors[0].detail)

    def test_missing_file_must_cover_marker_at_head_is_an_error(self) -> None:
        fixture = FixtureRepository(POSTGRES_CONFIG)
        self.addCleanup(fixture.close)
        fixture.write_file("src/lib.rs", "const SCHEMA_VERSION: i32 = 50;\n")
        fixture.write_file("schema.sql", POSTGRES_SCHEMA_BASE)
        base = fixture.commit("base with required file marker")
        fixture.write_file(
            "schema.sql", POSTGRES_SCHEMA_BASE.replace(
                "lash_session_execution_leases", "unrelated_table"
            )
        )
        head = fixture.commit("remove required file marker")

        result = self.check(fixture, base, head)

        self.assertEqual(result.failures, ())
        self.assertEqual(len(result.errors), 1)
        self.assertIn("lash_session_execution_leases", result.errors[0].detail)

    def test_missing_guarded_symbol_at_base_requires_bump_without_error(self) -> None:
        config = """
        [[surface]]
        constant = "WIRE_VERSION"
        constant_path = "src/lib.rs"
        description = "new guarded symbol"

        [[surface.guard]]
        kind = "rust_items"
        paths = ["src/wire.rs"]
        symbols = ["Existing", "Introduced"]
        """
        fixture = FixtureRepository(config)
        self.addCleanup(fixture.close)
        fixture.write_file("src/lib.rs", LIB_V1)
        fixture.write_file("src/wire.rs", "pub struct Existing;\n")
        base = fixture.commit("base without new guarded symbol")
        fixture.write_file(
            "src/wire.rs", "pub struct Existing;\npub struct Introduced;\n"
        )
        head = fixture.commit("introduce guarded symbol without bump")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)

    def test_surface_errors_are_aggregated(self) -> None:
        config = """
        [[surface]]
        constant = "FIRST_VERSION"
        constant_path = "src/lib.rs"
        description = "first invalid surface"
        [[surface.guard]]
        kind = "rust_serde_shapes"
        paths = ["src/first.rs"]
        must_cover = ["MissingFirst"]

        [[surface]]
        constant = "SECOND_VERSION"
        constant_path = "src/lib.rs"
        description = "second invalid surface"
        [[surface.guard]]
        kind = "rust_serde_shapes"
        paths = ["src/second.rs"]
        must_cover = ["MissingSecond"]
        """
        fixture = FixtureRepository(config)
        self.addCleanup(fixture.close)
        fixture.write_file(
            "src/lib.rs",
            "pub const FIRST_VERSION: u32 = 1;\npub const SECOND_VERSION: u32 = 1;\n",
        )
        fixture.write_file("src/first.rs", "pub struct UnrelatedFirst;\n")
        fixture.write_file("src/second.rs", "pub struct UnrelatedSecond;\n")
        revision = fixture.commit("two invalid surfaces")

        result = self.check(fixture, revision, revision)

        self.assertEqual(result.failures, ())
        self.assertEqual(len(result.errors), 2)
        self.assertIn("MissingFirst", result.errors[0].detail)
        self.assertIn("MissingSecond", result.errors[1].detail)

    def test_qualified_surface_key_selects_duplicate_constant(self) -> None:
        config = """
        [[surface]]
        constant = "SCHEMA_VERSION"
        constant_path = "postgres.rs"
        description = "PostgreSQL"
        [[surface.guard]]
        kind = "file"
        paths = ["postgres.sql"]

        [[surface]]
        constant = "SCHEMA_VERSION"
        constant_path = "sqlite.rs"
        description = "SQLite"
        [[surface.guard]]
        kind = "rust_items"
        paths = ["sqlite.rs"]
        symbols = ["SCHEMA"]
        """
        fixture = FixtureRepository(config)
        self.addCleanup(fixture.close)
        surfaces = MODULE.load_config(fixture.root / "surface.toml")

        selected = MODULE.select_surfaces(
            surfaces, ["postgres.rs:SCHEMA_VERSION"]
        )

        self.assertEqual(len(selected), 1)
        self.assertEqual(selected[0].constant_path, "postgres.rs")
        with self.assertRaisesRegex(MODULE.CheckError, "use one of"):
            MODULE.select_surfaces(surfaces, ["SCHEMA_VERSION"])


if __name__ == "__main__":
    unittest.main()
