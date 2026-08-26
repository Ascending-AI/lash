from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest
import unittest.mock


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

UNRELATED_SURFACE_ENTRY = """
[[surface]]
constant = "OTHER_VERSION"
constant_path = "src/other_version.rs"
description = "a surface the base inventory already declared"

[[surface.guard]]
kind = "rust_items"
paths = ["src/other.rs"]
symbols = ["Unrelated"]
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


SQLITE_CONFIG = """
[[surface]]
constant = "SCHEMA_VERSION"
constant_path = "src/lib.rs"
description = "SQLite catalog with the index-only carve-out"

[[surface.guard]]
kind = "rust_items"
paths = ["src/schema.rs"]
symbols = ["SCHEMA"]
elide = "sql_idempotent_index"
"""

SQLITE_SCHEMA_BASE = '''
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    enqueued_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sessions_enqueued
    ON sessions(enqueued_at_ms);
";
'''
# The carve-out case: one more idempotent, non-unique index and nothing else.
SQLITE_SCHEMA_INDEX_ADDED = SQLITE_SCHEMA_BASE.replace(
    '    ON sessions(enqueued_at_ms);\n',
    '    ON sessions(enqueued_at_ms);\n\n'
    'CREATE INDEX IF NOT EXISTS idx_sessions_order\n'
    '    ON sessions(session_id, enqueued_at_ms);\n',
)
SQLITE_SCHEMA_INDEX_REDEFINED = SQLITE_SCHEMA_BASE.replace(
    '    ON sessions(enqueued_at_ms);\n',
    '    ON sessions(session_id, enqueued_at_ms);\n',
)
SQLITE_SCHEMA_INDEX_REMOVED = SQLITE_SCHEMA_BASE.replace(
    '\nCREATE INDEX IF NOT EXISTS idx_sessions_enqueued\n'
    '    ON sessions(enqueued_at_ms);\n',
    '',
)
# Not the carve-out: a unique index is a constraint, and `IF NOT EXISTS` will not
# replace a differently-shaped one already in the file.
SQLITE_SCHEMA_UNIQUE_INDEX_ADDED = SQLITE_SCHEMA_BASE.replace(
    '    ON sessions(enqueued_at_ms);\n',
    '    ON sessions(enqueued_at_ms);\n\n'
    'CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_unique\n'
    '    ON sessions(session_id);\n',
)
SQLITE_SCHEMA_COLUMN_ADDED = SQLITE_SCHEMA_BASE.replace(
    "    enqueued_at_ms INTEGER NOT NULL\n",
    "    enqueued_at_ms INTEGER NOT NULL,\n    enqueue_seq INTEGER NOT NULL\n",
)


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

    def write_config(self, config: str) -> None:
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
        run(self.root, "git", "commit", "--allow-empty", "-q", "-m", message)
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

    def test_test_only_serde_shape_needs_no_bump(self) -> None:
        test_modules = (
            """
            #[cfg(test)]
            mod tests {
                #[derive(Serialize, Deserialize)]
                struct TestFixture {
                    added: bool,
                }
            }
            """,
            """
            #[cfg(all(test, feature = "testing"))]
            mod tests {
                #[derive(Serialize, Deserialize)]
                struct TestFixture {
                    added: bool,
                }
            }
            """,
        )
        for test_module in test_modules:
            with self.subTest(test_module=test_module):
                fixture = self.fixture()
                fixture.write(LIB_V1, WIRE_BASE)
                base = fixture.commit("base")
                fixture.write(LIB_V1, WIRE_BASE + test_module)
                head = fixture.commit("add a test-only Serde fixture")

                self.assertEqual(
                    self.check(fixture, base, head), MODULE.CheckResult((), ())
                )

    def test_production_serde_shape_still_needs_a_bump(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(
            LIB_V1,
            WIRE_BASE
            + """
            #[derive(Serialize, Deserialize)]
            struct ProductionShape {
                added: bool,
            }
            """,
        )
        head = fixture.commit("add a production Serde shape without a bump")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)

    def test_serde_shape_in_cfg_any_test_module_still_needs_a_bump(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(
            LIB_V1,
            WIRE_BASE
            + """
            #[cfg(any(feature = "core-conversions", test))]
            mod core_conversions {
                #[derive(Serialize, Deserialize)]
                struct FeatureShape {
                    added: bool,
                }
            }
            """,
        )
        head = fixture.commit("add a feature-enabled Serde shape without a bump")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)

    def test_production_shape_after_test_module_still_needs_a_bump(self) -> None:
        test_module = r'''
        #[cfg(test)]
        mod tests {
            const STRING_BRACE: &str = "}";
            const RAW_BRACE: &str = r#"}"#;
            // }
            /* { nested /* } */ } */
            #[derive(Serialize, Deserialize)]
            struct TestFixture;
        }
        '''
        fixture = self.fixture()
        fixture.write(LIB_V1, test_module + WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(LIB_V1, test_module + WIRE_CHANGED)
        head = fixture.commit("change a production shape after a test module")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)

    def test_cfg_test_only_detection_is_conservative(self) -> None:
        cases = (
            ("#[cfg(test)]", True),
            ('#[cfg(all(feature = "testing", test))]', True),
            ('#[cfg(any(feature = "core-conversions", test))]', False),
            ("#[cfg(not(test))]", False),
            ("#[cfg_attr(test, allow(dead_code))]", False),
            ('#[cfg(all(test, feature = r"testing"))]', False),
            ('#[cfg(all(test, feature = "x-y.z_1"))]', True),
            ('#[cfg(all(test, feature = "with space"))]', False),
            ('#[cfg(all(test, feature = "esc\\nape"))]', False),
            ("#[cfg(all (test))]", True),
            ('#[cfg(all(test, feature = "\\u{110000}"))]', False),
            ('#[cfg(all(test, feature = "\\u{D800}"))]', False),
            ('#[cfg(all(test, feature = "\\xFF"))]', False),
            ("#[cfg(all (test))]", False),
        )
        for attribute, expected in cases:
            with self.subTest(attribute=attribute):
                self.assertEqual(MODULE._test_only_cfg(attribute), expected)

    def test_malformed_test_module_keeps_serde_shapes_in_sweep(self) -> None:
        cases = (
            (
                "comment-split test predicate",
                r"""
                #[cfg(te /* boundary */ st)]
                mod tests {
                    #[derive(Serialize, Deserialize)]
                    struct CommentSplitShape;
                }
                """,
                "CommentSplitShape",
            ),
            (
                "invalid pub restriction",
                r"""
                #[cfg(test)]
                pub() mod tests {
                    #[derive(Serialize, Deserialize)]
                    struct InvalidVisibilityShape;
                }
                """,
                "InvalidVisibilityShape",
            ),
            (
                "invalid cfg string escape",
                r'''
                #[cfg(all(test, feature = "bad\qescape"))]
                mod tests {
                    #[derive(Serialize, Deserialize)]
                    struct InvalidEscapeShape;
                }
                ''',
                "InvalidEscapeShape",
            ),
            (
                "unterminated string before derive",
                r'''
                #[cfg(test)]
                mod tests {
                    const BROKEN: &str = "unterminated
                    #[derive(Serialize, Deserialize)]
                    struct AfterUnterminatedString;
                }
                ''',
                "AfterUnterminatedString",
            ),
            (
                "unterminated block comment before derive",
                r"""
                #[cfg(test)]
                mod tests {
                    /* unterminated
                    #[derive(Serialize, Deserialize)]
                    struct AfterUnterminatedComment;
                }
                """,
                "AfterUnterminatedComment",
            ),
            (
                "malformed cfg predicate",
                r"""
                #[cfg(all(test feature = "testing"))]
                mod tests {
                    #[derive(Serialize, Deserialize)]
                    struct MalformedCfgShape;
                }
                """,
                "MalformedCfgShape",
            ),
            (
                "unbalanced module body",
                r"""
                #[cfg(test)]
                mod tests {
                    #[derive(Serialize, Deserialize)]
                    struct UnbalancedBodyShape;
                """,
                "UnbalancedBodyShape",
            ),
        )
        for name, source, shape in cases:
            with self.subTest(name=name):
                shapes = MODULE.serde_shapes(textwrap.dedent(source))

                self.assertIn(shape, shapes)

    def test_unterminated_raw_string_in_module_fails_closed(self) -> None:
        source = textwrap.dedent(
            r"""
            #[cfg(test)]
            mod tests {
                const BROKEN: &str = r##"never closes
                #[derive(Serialize, Deserialize)]
                struct AfterUnterminatedRawString;
            }
            """
        )
        with self.assertRaises(MODULE.CheckError):
            MODULE.serde_shapes(source)

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

    def sqlite_check(self, head_schema: str):
        fixture = FixtureRepository(SQLITE_CONFIG)
        self.addCleanup(fixture.close)
        fixture.write_file("src/lib.rs", "pub(crate) const SCHEMA_VERSION: i32 = 37;\n")
        fixture.write_file("src/schema.rs", SQLITE_SCHEMA_BASE)
        base = fixture.commit("sqlite carve-out base")
        fixture.write_file("src/schema.rs", head_schema)
        head = fixture.commit("sqlite catalog change with the version pinned to 37")
        return self.check(fixture, base, head)

    def test_idempotent_index_addition_needs_no_bump(self) -> None:
        # The ratified SQLite carve-out: open runs the whole schema with
        # `IF NOT EXISTS`, so old and new binaries stay mutually compatible on one
        # file and rejecting live stores would buy nothing.
        result = self.sqlite_check(SQLITE_SCHEMA_INDEX_ADDED)

        self.assertEqual(result.errors, ())
        self.assertEqual(result.failures, ())

    def test_idempotent_index_byte_identical_reemission_needs_no_bump(self) -> None:
        # Negative control: re-emitting an existing elided index with a
        # byte-identical definition demands no bump.
        result = self.sqlite_check(SQLITE_SCHEMA_BASE)

        self.assertEqual(result.errors, ())
        self.assertEqual(result.failures, ())

    def test_idempotent_index_redefinition_still_needs_a_bump(self) -> None:
        # Redefining an existing index under the same name with a changed column
        # list must demand a bump because SQLite's `IF NOT EXISTS` will not
        # replace an existing index with a different shape on an opened database.
        result = self.sqlite_check(SQLITE_SCHEMA_INDEX_REDEFINED)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)
        self.assertEqual(result.failures[0].head_version, 37)

    def test_idempotent_index_removal_demands_a_bump_by_rule(self) -> None:
        # Deliberate mechanical rule: elision applies only to statements that
        # introduce NEW index names relative to the base revision. Existing
        # indexes on the base side are not elided, so index removal leaves base
        # and head signatures differing and demands a bump explicitly by design
        # rather than via regex whitespace artifact.
        result = self.sqlite_check(SQLITE_SCHEMA_INDEX_REMOVED)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)
        self.assertEqual(result.failures[0].head_version, 37)

    def test_unique_index_addition_still_needs_a_bump(self) -> None:
        result = self.sqlite_check(SQLITE_SCHEMA_UNIQUE_INDEX_ADDED)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)
        self.assertEqual(result.failures[0].head_version, 37)

    def test_column_addition_still_needs_a_bump_under_the_carve_out(self) -> None:
        result = self.sqlite_check(SQLITE_SCHEMA_COLUMN_ADDED)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)
        self.assertEqual(result.failures[0].head_version, 37)

    def test_unknown_elision_is_a_configuration_error(self) -> None:
        with self.assertRaises(MODULE.CheckError) as raised:
            MODULE.load_config(self.elision_config("no_such_elision"))
        self.assertIn("unsupported elide", str(raised.exception))

    def elision_config(self, elide: str) -> Path:
        fixture = FixtureRepository(SQLITE_CONFIG.replace("sql_idempotent_index", elide))
        self.addCleanup(fixture.close)
        return fixture.root / "surface.toml"

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

    def registration_fixture(self, base_lib: str) -> tuple[FixtureRepository, str]:
        """A base that predates the WIRE_VERSION surface entry entirely."""
        fixture = FixtureRepository()
        self.addCleanup(fixture.close)
        fixture.write_config(UNRELATED_SURFACE_ENTRY)
        fixture.write_file("src/other_version.rs", "pub const OTHER_VERSION: u32 = 1;\n")
        fixture.write_file("src/other.rs", "pub struct Unrelated;\n")
        fixture.write_file("src/lib.rs", base_lib)
        fixture.write_file("src/wire.rs", WIRE_BASE)
        return fixture, fixture.commit("base before the surface was registered")

    def registered_check(
        self, fixture: FixtureRepository, base: str, head: str, baselines: dict[str, str]
    ):
        config = fixture.root / "surface.toml"
        surfaces = MODULE.load_config(config)
        base_keys = MODULE.base_inventory_keys(fixture.root, base, config)
        with unittest.mock.patch.dict(
            MODULE.REGISTRATION_BASELINES, baselines, clear=True
        ):
            return MODULE.check_surfaces(
                fixture.root, base, head, surfaces, base_keys
            )

    def test_registering_a_surface_without_a_burned_baseline_fails(self) -> None:
        fixture, base = self.registration_fixture("")
        fixture.write_config(CONFIG + UNRELATED_SURFACE_ENTRY)
        fixture.write_file("src/lib.rs", LIB_V1)
        fixture.write_file("src/wire.rs", WIRE_CHANGED)
        head = fixture.commit("register the surface and stamp the shape")

        result = self.check_cli(fixture, base, head)

        self.assertEqual(result.returncode, 1)
        self.assertIn("no burned registration baseline", result.stderr)
        self.assertIn("'src/lib.rs:WIRE_VERSION': 'sha256:", result.stderr)

    def test_registering_a_surface_with_its_burned_baseline_passes(self) -> None:
        fixture, base = self.registration_fixture("")
        fixture.write_config(CONFIG + UNRELATED_SURFACE_ENTRY)
        fixture.write_file("src/lib.rs", LIB_V1)
        fixture.write_file("src/wire.rs", WIRE_CHANGED)
        head = fixture.commit("register the surface and stamp the shape")

        unburned = self.registered_check(fixture, base, head, {})
        self.assertEqual(len(unburned.unregistered), 1)

        result = self.registered_check(
            fixture,
            base,
            head,
            {"src/lib.rs:WIRE_VERSION": unburned.unregistered[0].fingerprint},
        )

        self.assertEqual(result.failures, ())
        self.assertEqual(result.errors, ())
        self.assertEqual(result.unregistered, ())
        self.assertEqual(
            tuple(surface.key for surface in result.registrations),
            ("src/lib.rs:WIRE_VERSION",),
        )

    def test_a_burned_baseline_does_not_cover_a_different_shape(self) -> None:
        fixture, base = self.registration_fixture("")
        fixture.write_config(CONFIG + UNRELATED_SURFACE_ENTRY)
        fixture.write_file("src/lib.rs", LIB_V1)
        fixture.write_file("src/wire.rs", WIRE_CHANGED)
        head = fixture.commit("register the surface and stamp the shape")

        result = self.registered_check(
            fixture,
            base,
            head,
            {"src/lib.rs:WIRE_VERSION": "sha256:" + "0" * 64},
        )

        self.assertEqual(result.registrations, ())
        self.assertEqual(len(result.unregistered), 1)

    def test_renaming_a_live_constant_is_not_a_registration(self) -> None:
        """The bypass a category-shaped registration rule would have opened.

        Both halves of the old inference read "new" for a rename: the surface
        key changed, so the base inventory has never seen it, and the renamed
        constant is nowhere in the base tree either. Only a baseline pinned to
        the enrolled key and bytes can tell the two apart.
        """
        fixture = self.fixture()
        fixture.write_config(CONFIG)
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("a live, registered surface")

        renamed_config = CONFIG.replace("WIRE_VERSION", "WIRE_GENERATION")
        fixture.write_config(renamed_config)
        fixture.write(
            LIB_V1.replace("WIRE_VERSION", "WIRE_GENERATION"), WIRE_CHANGED
        )
        head = fixture.commit("rename the constant and change the shape")

        result = self.registered_check(
            fixture,
            base,
            head,
            {"src/lib.rs:WIRE_VERSION": "sha256:" + "0" * 64},
        )

        self.assertEqual(result.registrations, ())
        self.assertEqual(len(result.unregistered), 1)
        self.assertEqual(result.unregistered[0].surface.constant, "WIRE_GENERATION")

        cli = self.check_cli(fixture, base, head)
        self.assertEqual(cli.returncode, 1)
        self.assertIn("no burned registration baseline", cli.stderr)

    def test_registering_a_surface_over_a_live_constant_still_needs_a_bump(self) -> None:
        fixture, base = self.registration_fixture(LIB_V1)
        fixture.write_config(CONFIG + UNRELATED_SURFACE_ENTRY)
        fixture.write_file("src/wire.rs", WIRE_CHANGED)
        head = fixture.commit("re-register a surface whose constant already existed")

        result = self.check_cli(fixture, base, head)

        self.assertEqual(result.returncode, 1)
        self.assertIn("Bump WIRE_VERSION strictly past 1", result.stderr)

    def renamed_check(
        self, fixture: FixtureRepository, base: str, head: str, baselines: dict[str, str]
    ):
        surfaces = MODULE.load_config(fixture.root / "surface.toml")
        with unittest.mock.patch.dict(
            MODULE.IDENTIFIER_RENAME_BASELINES, baselines, clear=True
        ):
            return MODULE.check_surfaces(fixture.root, base, head, surfaces)

    def identifier_rename_fixture(self) -> tuple[FixtureRepository, str, str]:
        """A variant renamed with no bump: guarded text moves, format does not."""
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(LIB_V1, WIRE_BASE.replace("Existing", "Present"))
        head = fixture.commit("rename a wire variant without bumping")
        return fixture, base, head

    def test_an_identifier_rename_without_a_burned_baseline_fails(self) -> None:
        fixture, base, head = self.identifier_rename_fixture()

        result = self.renamed_check(fixture, base, head, {})

        self.assertEqual(result.identifier_renames, ())
        self.assertEqual(len(result.failures), 1)

        cli = self.check_cli(fixture, base, head)
        self.assertEqual(cli.returncode, 1)
        self.assertIn("Bump WIRE_VERSION strictly past 1", cli.stderr)
        self.assertIn("'src/lib.rs:WIRE_VERSION': 'sha256:", cli.stderr)

    def test_an_identifier_rename_with_its_burned_baseline_needs_no_bump(self) -> None:
        fixture, base, head = self.identifier_rename_fixture()

        unburned = self.renamed_check(fixture, base, head, {})

        result = self.renamed_check(
            fixture,
            base,
            head,
            {"src/lib.rs:WIRE_VERSION": unburned.failures[0].fingerprint},
        )

        self.assertEqual(result.failures, ())
        self.assertEqual(result.errors, ())
        self.assertEqual(
            tuple(surface.key for surface in result.identifier_renames),
            ("src/lib.rs:WIRE_VERSION",),
        )

    def test_an_atomic_stack_can_reserve_its_bump_on_the_lower_branch(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("lower stack branch already bumped the version")
        fixture.write(LIB_V1, WIRE_CHANGED)
        head = fixture.commit("upper stack branch lands the reserved shape")
        surfaces = MODULE.load_config(fixture.root / "surface.toml")

        with unittest.mock.patch.dict(MODULE.STACKED_VERSION_BASELINES, {}, clear=True):
            unburned = MODULE.check_surfaces(fixture.root, base, head, surfaces)
        fingerprint = unburned.failures[0].fingerprint
        with unittest.mock.patch.dict(
            MODULE.STACKED_VERSION_BASELINES,
            {"src/lib.rs:WIRE_VERSION": fingerprint},
            clear=True,
        ):
            result = MODULE.check_surfaces(fixture.root, base, head, surfaces)

        self.assertEqual(result.failures, ())
        self.assertEqual(result.identifier_renames, ())
        self.assertEqual(
            tuple(surface.key for surface in result.stacked_versions),
            ("src/lib.rs:WIRE_VERSION",),
        )

    def test_a_burned_rename_baseline_does_not_cover_a_later_change(self) -> None:
        """The single-use property: the baseline pins one head shape, not a surface.

        A second change to the same guarded shape — even another rename — computes
        a different fingerprint, so the burned entry stops answering for it.
        """
        fixture, base, head = self.identifier_rename_fixture()
        burned = self.renamed_check(fixture, base, head, {}).failures[0].fingerprint

        fixture.write(LIB_V1, WIRE_BASE.replace("Existing", "Current"))
        later = fixture.commit("rename the same variant again")

        result = self.renamed_check(
            fixture, base, later, {"src/lib.rs:WIRE_VERSION": burned}
        )

        self.assertEqual(result.identifier_renames, ())
        self.assertEqual(len(result.failures), 1)
        self.assertNotEqual(result.failures[0].fingerprint, burned)

    def test_a_rename_baseline_for_another_surface_excuses_nothing(self) -> None:
        fixture, base, head = self.identifier_rename_fixture()

        result = self.renamed_check(
            fixture,
            base,
            head,
            {"src/other_version.rs:OTHER_VERSION": "sha256:" + "0" * 64},
        )

        self.assertEqual(result.identifier_renames, ())
        self.assertEqual(len(result.failures), 1)

    def test_an_unreadable_base_inventory_checks_every_surface(self) -> None:
        fixture = self.fixture()
        fixture.write(LIB_V1, WIRE_BASE)
        base = fixture.commit("base")
        fixture.write(LIB_V1, WIRE_CHANGED)
        head = fixture.commit("change the shape without a bump")

        self.assertIsNone(
            MODULE.base_inventory_keys(fixture.root, base, Path("/nowhere/surface.toml"))
        )

        result = self.check_cli(fixture, base, head)

        self.assertEqual(result.returncode, 1)

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
