from __future__ import annotations

import importlib.util
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest
import unittest.mock


SCRIPT = Path(__file__).with_name("check_version_bumps.py")
REAL_CONFIG = SCRIPT.with_name("versioned-surfaces.toml")
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

CONTINUATION_TOOL_FAILURE_CONFIG = """
[[surface]]
constant = "VM_CONTINUATION_FORMAT_VERSION"
constant_path = "src/continuation.rs"
description = "fixture continuation tool-failure leaves"

[[surface.guard]]
kind = "rust_items"
paths = ["src/tool_output.rs"]
symbols = ["ToolFailureClass", "ToolFailureSource", "ToolRetryStatus"]
"""

CONTINUATION_TOOL_FAILURE_LEAVES = """
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureClass {
    InvalidRequest,
    PermissionDenied,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureSource {
    Runtime,
    Policy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolRetryStatus {
    Never,
    Exhausted { attempts: u32 },
}
"""

# The two real surfaces the same `ToolUsageDelta` is guarded under: the
# settlement carries it and the attempt capture's `usage` list is made of it,
# and a mutation must trip *both* constants or one carrier decodes a fact the
# other never versioned.
TOOL_SETTLEMENT_SURFACES_CONFIG = """
[[surface]]
constant = "TOOL_SETTLEMENT_VERSION"
constant_path = "src/settlement.rs"
description = "fixture tool-child settlement"

[[surface.guard]]
kind = "rust_items"
paths = ["src/tool_facts.rs"]
symbols = ["ToolSettlement", "ToolUsageDelta"]

[[surface]]
constant = "TOOL_ATTEMPT_CAPTURE_VERSION"
constant_path = "src/settlement.rs"
description = "fixture tool-attempt capture"

[[surface.guard]]
kind = "rust_items"
paths = ["src/tool_facts.rs"]
symbols = ["ToolAttemptCapture", "ToolUsageDelta"]
"""

TOOL_SETTLEMENT_SHAPES = """
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSettlement {
    pub version: u16,
    pub usage: Vec<ToolUsageDelta>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolAttemptCapture {
    pub version: u16,
    pub usage: Vec<ToolUsageDelta>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolUsageDelta {
    pub attempt: u32,
    pub provider_attempt: u32,
    pub usage: TokenUsage,
}
"""

ABILITY_CONFIG = """
[[surface]]
constant = "LASHLANG_VM_ABI_VERSION"
constant_path = "src/artifact.rs"
version_regex = '\\bLASHLANG_VM_ABI_VERSION\\b\\s*:\\s*&str\\s*=\\s*"lashlang-vm-abi-v([0-9]+)"'
description = "fixture VM-to-host ability contract"

[[surface.guard]]
kind = "rust_items"
paths = ["src/host.rs"]
symbols = [
  "AbilityOp",
  "AbilityResult",
  "ResourceOperationBatch",
  "ResourceOperationBatchResult",
]
"""

ABILITY_SHAPES = """
#[derive(Clone, Debug)]
pub enum AbilityOp {
    ResourceOperation(Box<ResourceOperation>),
    ResourceOperationBatch(ResourceOperationBatch),
    Await(Value),
}

#[derive(Clone, Debug)]
pub enum AbilityResult {
    Value(Value),
    ResourceOperationBatch(ResourceOperationBatchResult),
    Unit,
}

#[derive(Clone, Debug)]
pub struct ResourceOperationBatch {
    pub operations: Vec<ResourceOperation>,
}

#[derive(Clone, Debug)]
pub struct ResourceOperationBatchResult {
    pub results: Vec<ResourceOperationResult>,
    pub settlement_order: Vec<usize>,
}
"""

# One `pub(crate) const XS: &[T] = &[T { .. }, ..];` table, which is the shape
# the builtin registry has and the shape an item walk that stops at the first
# balanced brace truncates after its first entry.
REGISTRY_CONFIG = """
[[surface]]
constant = "LASHLANG_SEMANTIC_HASH_VERSION"
constant_path = "src/identity.rs"
version_regex = '\\bLASHLANG_SEMANTIC_HASH_VERSION\\b\\s*:\\s*&str\\s*=\\s*"lashlang-semantic-v([0-9]+)"'
description = "fixture builtin vocabulary behind a generic builtin-call encoding"

[[surface.guard]]
kind = "rust_items"
paths = ["src/builtins.rs"]
symbols = ["SOURCE_BUILTINS", "TYPESCRIPT_BUILTINS"]
"""

REGISTRY_SOURCE = """
pub(crate) const SOURCE_BUILTINS: &[Builtin] = &[
    Builtin {
        name: "len",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "join",
        arity: Arity::Exact(2),
    },
];

pub(crate) const TYPESCRIPT_BUILTINS: &[Builtin] = &[
    Builtin {
        name: "__typescript_split",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__typescript_stdlib",
        arity: Arity::AtLeast(1),
    },
];
"""

# A lowering change re-keys what unchanged source means without touching any
# item a symbol-named guard projects, so the real surface whole-file-guards
# the TypeScript lowerer and the structural roles it marks. The fixture keeps
# the same two legs: a directory glob and one file beside it.
LOWERING_CONFIG = """
[[surface]]
constant = "LASHLANG_SEMANTIC_HASH_VERSION"
constant_path = "src/identity.rs"
version_regex = '\\bLASHLANG_SEMANTIC_HASH_VERSION\\b\\s*:\\s*&str\\s*=\\s*"lashlang-semantic-v([0-9]+)"'
description = "fixture lowerer and structural-role surface"

[[surface.guard]]
kind = "file"
paths = ["src/lower/**", "src/ast_roles.rs"]
must_cover = ["impl Lowerer", "CollectionTransformParts"]
"""

LOWERING_SOURCE = """
impl Lowerer {
    fn reference(&mut self, member: &Expr) -> LashExpr {
        LashExpr::Read(member)
    }
}
"""

AST_ROLES_SOURCE = """
pub struct CollectionTransformParts;
"""

BYTECODE_CONFIG = """
[[surface]]
constant = "BYTECODE_FORMAT_VERSION"
constant_path = "src/lib.rs"
description = "fixture instruction stream and the enums it encodes"

[[surface.guard]]
kind = "rust_items"
paths = ["src/instruction.rs"]
symbols = ["Instruction", "IntrinsicOp"]

[[surface.guard]]
kind = "rust_items"
paths = ["src/ast.rs"]
symbols = ["BinaryOp"]
"""

# The instruction stream carries its operand enums by value, so the vocabulary
# that decides what a compiled program means is spread across three enums in two
# files while only the outer one names the stream.
BYTECODE_INSTRUCTIONS = """
#[derive(Clone, Copy)]
pub(crate) enum Instruction {
    PushConst(usize),
    Binary(BinaryOp),
    Intrinsic(IntrinsicOp),
    Return,
}

#[derive(Clone, Copy)]
pub(crate) enum IntrinsicOp {
    Len,
    Slice,
    Sort,
    Reverse,
}
"""

BYTECODE_OPERATORS = """
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinaryOp {
    Add,
    Subtract,
}
"""

# A fixed-size array type puts a semicolon inside brackets, at the top level of
# the item's own declaration.
ARRAY_LENGTH_ITEM = """
pub(crate) const LANES: [Lane; 2] = [Lane::First, Lane::Second];
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
    TurnStarted,
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

    def test_adding_json_schema_to_guarded_derive_lists_needs_no_bump(self) -> None:
        cases = (
            (
                "direct enum derive",
                WIRE_BASE,
                WIRE_BASE.replace(
                    "Serialize, Deserialize", "Serialize, Deserialize, schemars::JsonSchema"
                ),
            ),
            (
                "cfg_attr struct derive",
                """
                #[derive(Serialize, Deserialize)]
                #[cfg_attr(feature = "schema", derive(Clone))]
                pub struct WireMessage { value: String }
                """,
                """
                #[derive(Serialize, Deserialize)]
                #[cfg_attr(feature = "schema", derive(Clone, schemars::JsonSchema))]
                pub struct WireMessage { value: String }
                """,
            ),
        )
        for name, base_wire, head_wire in cases:
            with self.subTest(name=name):
                fixture = self.fixture()
                fixture.write(LIB_V1, base_wire)
                base = fixture.commit("base")
                fixture.write(LIB_V1, head_wire)
                head = fixture.commit("add schema derive without bump")

                self.assertEqual(
                    self.check(fixture, base, head), MODULE.CheckResult((), ())
                )

    def test_derive_spelling_inside_attribute_string_is_not_exempt(self) -> None:
        fixture = self.fixture()
        base_wire = '#[doc = "derive(Clone)"]\n' + WIRE_BASE
        head_wire = '#[doc = "derive(Clone, JsonSchema)"]\n' + WIRE_BASE
        fixture.write(LIB_V1, base_wire)
        base = fixture.commit("base")
        fixture.write(LIB_V1, head_wire)
        head = fixture.commit("change attribute string without bump")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)

    def test_only_allowlisted_derives_are_exempt(self) -> None:
        cases = (
            (
                "wire derive",
                "#[derive(Clone, Serialize)]\npub struct WireMessage { value: String }\n",
                "#[derive(Clone, serde_repr::Serialize_repr)]\npub struct WireMessage { value: String }\n",
            ),
            (
                "unknown derive",
                "#[derive(Clone, Serialize, WireV1)]\npub struct WireMessage { value: String }\n",
                "#[derive(Clone, Serialize, WireV2)]\npub struct WireMessage { value: String }\n",
            ),
            (
                "namespaced attribute",
                "#[x::derive(WireV1)]\n" + WIRE_BASE,
                "#[x::derive(WireV2)]\n" + WIRE_BASE,
            ),
            (
                "macro argument",
                "#[wire_macro(derive(WireV1))]\n" + WIRE_BASE,
                "#[wire_macro(derive(WireV2))]\n" + WIRE_BASE,
            ),
            (
                "cfg_attr sibling after commented parenthesis",
                '#[cfg_attr(all(), derive(Clone /* ( */), serde(rename = "old"))]\n'
                + WIRE_BASE,
                '#[cfg_attr(all(), derive(Clone /* ( */), serde(rename = "new"))]\n'
                + WIRE_BASE,
            ),
        )
        for name, base_wire, head_wire in cases:
            with self.subTest(name=name):
                fixture = self.fixture()
                fixture.write(LIB_V1, base_wire)
                base = fixture.commit("base")
                fixture.write(LIB_V1, head_wire)
                head = fixture.commit("change guarded tokens without bump")

                result = self.check(fixture, base, head)

                self.assertEqual(result.errors, ())
                self.assertEqual(len(result.failures), 1)

    def test_ast_string_wire_edit_trips_graph_and_facet_guards(self) -> None:
        config = """
        [[surface]]
        constant = "GRAPH_VERSION"
        constant_path = "src/lib.rs"
        description = "graph"

        [[surface.guard]]
        kind = "rust_items"
        paths = ["src/ast_string.rs"]
        symbols = ["AstString"]

        [[surface]]
        constant = "FACET_VERSION"
        constant_path = "src/lib.rs"
        description = "facets"

        [[surface.guard]]
        kind = "rust_items"
        paths = ["src/ast_string.rs"]
        symbols = ["AstString"]
        """
        fixture = FixtureRepository(config)
        self.addCleanup(fixture.close)
        fixture.write_file(
            "src/lib.rs",
            "pub const GRAPH_VERSION: u32 = 1;\npub const FACET_VERSION: u32 = 1;\n",
        )
        fixture.write_file(
            "src/ast_string.rs",
            "#[derive(Serialize, Deserialize)]\n#[serde(transparent)]\npub struct AstString(String);\n",
        )
        base = fixture.commit("base")
        fixture.write_file(
            "src/ast_string.rs",
            "#[derive(Serialize, Deserialize)]\npub struct AstString { value: String }\n",
        )
        head = fixture.commit("change wrapper wire without bumps")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(
            {failure.surface.constant for failure in result.failures},
            {"GRAPH_VERSION", "FACET_VERSION"},
        )

    def test_serde_attribute_changes_still_require_a_bump(self) -> None:
        cases = (
            (
                "rename",
                '#[serde(rename = "old")]\n',
                '#[serde(rename = "new")]\n',
            ),
            ("tag", '#[serde(tag = "type")]\n', '#[serde(tag = "kind")]\n'),
            ("deny_unknown_fields", "", "#[serde(deny_unknown_fields)]\n"),
            (
                "skip_serializing_if",
                '#[serde(skip_serializing_if = "Option::is_none")]\n',
                '#[serde(skip_serializing_if = "Vec::is_empty")]\n',
            ),
            ("default", "#[serde(default)]\n", '#[serde(default = "default_value")]\n'),
        )
        for name, base_attribute, head_attribute in cases:
            with self.subTest(name=name):
                fixture = self.fixture()
                base_wire = (
                    "#[derive(Serialize, Deserialize)]\n"
                    + base_attribute
                    + "pub struct WireMessage { value: Option<String> }\n"
                )
                head_wire = (
                    "#[derive(Serialize, Deserialize)]\n"
                    + head_attribute
                    + "pub struct WireMessage { value: Option<String> }\n"
                )
                fixture.write(LIB_V1, base_wire)
                base = fixture.commit("base")
                fixture.write(LIB_V1, head_wire)
                head = fixture.commit("change serde attribute without bump")

                result = self.check(fixture, base, head)

                self.assertEqual(result.errors, ())
                self.assertEqual(len(result.failures), 1)
                self.assertEqual(result.failures[0].surface.constant, "WIRE_VERSION")

    def test_continuation_registry_guards_all_tool_failure_leaves(self) -> None:
        continuation = next(
            surface
            for surface in MODULE.load_config(REAL_CONFIG)
            if surface.constant == "VM_CONTINUATION_FORMAT_VERSION"
        )
        leaf_guard = next(
            guard
            for guard in continuation.guards
            if guard.paths == ("crates/lash-sansio/src/tool_output.rs",)
        )

        self.assertEqual(leaf_guard.kind, "rust_items")
        self.assertEqual(
            leaf_guard.symbols,
            ("ToolFailureClass", "ToolFailureSource", "ToolRetryStatus"),
        )

    def test_each_continuation_tool_failure_leaf_change_demands_a_bump(self) -> None:
        mutations = {
            "class vocabulary": ("InvalidRequest", "InvalidRequestV2"),
            "source vocabulary": ("Policy", "PolicyV2"),
            "retry shape": ("attempts: u32", "attempts: u64"),
        }
        for name, (before, after) in mutations.items():
            with self.subTest(name=name):
                fixture = FixtureRepository(CONTINUATION_TOOL_FAILURE_CONFIG)
                self.addCleanup(fixture.close)
                fixture.write_file(
                    "src/continuation.rs",
                    "pub const VM_CONTINUATION_FORMAT_VERSION: u32 = 10;\n",
                )
                fixture.write_file(
                    "src/tool_output.rs", CONTINUATION_TOOL_FAILURE_LEAVES
                )
                base = fixture.commit("base")
                fixture.write_file(
                    "src/tool_output.rs",
                    CONTINUATION_TOOL_FAILURE_LEAVES.replace(before, after),
                )
                head = fixture.commit(f"change {name} without bump")

                result = self.check(fixture, base, head)

                self.assertEqual(result.errors, ())
                self.assertEqual(len(result.failures), 1)
                self.assertEqual(
                    result.failures[0].surface.constant,
                    "VM_CONTINUATION_FORMAT_VERSION",
                )
                self.assertEqual(result.failures[0].base_version, 10)
                self.assertEqual(result.failures[0].head_version, 10)

    def test_the_attempt_capture_surface_guards_tool_usage_delta(self) -> None:
        """`ToolUsageDelta` is the capture's `usage` element, so the capture
        guard must name it as well as the settlement guard does — a mutation
        there that tripped only one constant would let one carrier decode a
        fact the other never versioned (FIG-2266 C1 review)."""
        surfaces = MODULE.load_config(REAL_CONFIG)
        for constant in ("TOOL_SETTLEMENT_VERSION", "TOOL_ATTEMPT_CAPTURE_VERSION"):
            surface = next(
                surface for surface in surfaces if surface.constant == constant
            )
            symbols = {symbol for guard in surface.guards for symbol in guard.symbols}
            self.assertIn("ToolUsageDelta", symbols, constant)

    def test_a_tool_usage_delta_mutation_demands_both_bumps(self) -> None:
        """The same type is journaled inside two carriers; mutating it without
        bumping must fail both surfaces, not just the settlement's."""
        fixture = FixtureRepository(TOOL_SETTLEMENT_SURFACES_CONFIG)
        self.addCleanup(fixture.close)
        fixture.write_file(
            "src/settlement.rs",
            "pub const TOOL_SETTLEMENT_VERSION: u16 = 2;\n"
            "pub const TOOL_ATTEMPT_CAPTURE_VERSION: u16 = 2;\n",
        )
        fixture.write_file("src/tool_facts.rs", TOOL_SETTLEMENT_SHAPES)
        base = fixture.commit("base")
        fixture.write_file(
            "src/tool_facts.rs",
            TOOL_SETTLEMENT_SHAPES.replace("provider_attempt: u32", "provider_attempt: u64"),
        )
        head = fixture.commit("retype the usage delta without bumping")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(
            {failure.surface.constant for failure in result.failures},
            {"TOOL_SETTLEMENT_VERSION", "TOOL_ATTEMPT_CAPTURE_VERSION"},
        )

    def test_nested_workflow_payload_changes_demand_each_carrier_bump(self) -> None:
        """A shared nested type payload must trip both documents that embed it."""
        surfaces = {
            surface.constant: surface
            for surface in MODULE.load_config(REAL_CONFIG)
            if surface.constant
            in {"WORKFLOW_GRAPH_SCHEMA_VERSION", "WORKFLOW_TYPE_FACET_SCHEMA_VERSION"}
        }
        ast_path = "crates/lashlang/src/ast.rs"

        def ast_symbols(constant: str) -> tuple[str, ...]:
            return next(
                guard.symbols
                for guard in surfaces[constant].guards
                if guard.kind == "rust_items" and guard.paths == (ast_path,)
            )

        graph_symbols = ast_symbols("WORKFLOW_GRAPH_SCHEMA_VERSION")
        facet_symbols = ast_symbols("WORKFLOW_TYPE_FACET_SCHEMA_VERSION")
        all_symbols = set(graph_symbols) | set(facet_symbols)

        def toml_strings(symbols: tuple[str, ...]) -> str:
            return ", ".join(f'"{symbol}"' for symbol in symbols)

        config = f"""
        [[surface]]
        constant = "WORKFLOW_GRAPH_SCHEMA_VERSION"
        constant_path = "src/graph.rs"
        description = "fixture graph carrier"

        [[surface.guard]]
        kind = "rust_items"
        paths = ["src/ast.rs"]
        symbols = [{toml_strings(graph_symbols)}]

        [[surface]]
        constant = "WORKFLOW_TYPE_FACET_SCHEMA_VERSION"
        constant_path = "src/facets.rs"
        description = "fixture facet carrier"

        [[surface.guard]]
        kind = "rust_items"
        paths = ["src/ast.rs"]
        symbols = [{toml_strings(facet_symbols)}]
        """
        fixture = FixtureRepository(config)
        self.addCleanup(fixture.close)
        fixture.write_file(
            "src/graph.rs", "pub const WORKFLOW_GRAPH_SCHEMA_VERSION: u32 = 11;\n"
        )
        fixture.write_file(
            "src/facets.rs",
            "pub const WORKFLOW_TYPE_FACET_SCHEMA_VERSION: u32 = 3;\n",
        )
        declarations = []
        for symbol in sorted(all_symbols):
            if symbol == "TypeField":
                declarations.append("pub struct TypeField { pub optional: bool }")
            else:
                declarations.append(f"pub struct {symbol};")
        base_source = "\n".join(declarations) + "\n"
        fixture.write_file("src/ast.rs", base_source)
        base = fixture.commit("workflow carrier base")
        fixture.write_file(
            "src/ast.rs",
            base_source.replace(
                "pub struct TypeField { pub optional: bool }",
                "pub struct TypeField { pub optional: bool, pub future: bool }",
            ),
        )
        head = fixture.commit("change nested TypeField without carrier bumps")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(
            {failure.surface.constant for failure in result.failures},
            {"WORKFLOW_GRAPH_SCHEMA_VERSION", "WORKFLOW_TYPE_FACET_SCHEMA_VERSION"},
        )

    def test_workflow_identity_preimage_selection_changes_demand_a_graph_bump(
        self,
    ) -> None:
        graph_surface = next(
            surface
            for surface in MODULE.load_config(REAL_CONFIG)
            if surface.constant == "WORKFLOW_GRAPH_SCHEMA_VERSION"
        )
        identity_guard = next(
            guard
            for guard in graph_surface.guards
            if "crates/lash-typescript/src/workflow_graph/mod.rs" in guard.paths
        )
        selectors = (
            "workflow_graph_from_source_with_facets",
            "workflow_graph_from_program",
        )
        for selector in selectors:
            self.assertIn(selector, identity_guard.symbols)

        symbols = ", ".join(f'"{symbol}"' for symbol in identity_guard.symbols)
        config = f"""
        [[surface]]
        constant = "WORKFLOW_GRAPH_SCHEMA_VERSION"
        constant_path = "src/graph.rs"
        description = "fixture workflow identity preimage selection"

        [[surface.guard]]
        kind = "rust_items"
        paths = ["src/projector.rs"]
        symbols = [{symbols}]
        """
        source = """
        fn workflow_graph_from_source_with_facets() { select("admitted artifact"); }
        fn workflow_graph_from_program() { select("program"); }
        fn workflow_graph_from_artifact() {}
        fn project_process() {}
        fn project_literal_process() {}
        fn project_node() {}
        fn source_identity() {}
        fn edge() {}
        """
        for selector in selectors:
            with self.subTest(selector=selector):
                fixture = FixtureRepository(config)
                self.addCleanup(fixture.close)
                fixture.write_file(
                    "src/graph.rs",
                    "pub const WORKFLOW_GRAPH_SCHEMA_VERSION: u32 = 13;\n",
                )
                fixture.write_file("src/projector.rs", source)
                base = fixture.commit("workflow identity selector base")
                fixture.write_file(
                    "src/projector.rs",
                    source.replace(
                        f"fn {selector}() {{",
                        f"fn {selector}() {{ changed();",
                    ),
                )
                head = fixture.commit("change preimage selection without graph bump")

                result = self.check(fixture, base, head)

                self.assertEqual(result.errors, ())
                self.assertEqual(len(result.failures), 1)
                self.assertEqual(
                    result.failures[0].surface.constant,
                    "WORKFLOW_GRAPH_SCHEMA_VERSION",
                )

    def test_each_runtime_node_identity_helper_demands_graph_and_segment_bumps(
        self,
    ) -> None:
        constants = {"WORKFLOW_GRAPH_SCHEMA_VERSION", "LASHLANG_SEGMENT_STATE_VERSION"}
        surfaces = {
            surface.constant: surface
            for surface in MODULE.load_config(REAL_CONFIG)
            if surface.constant in constants
        }
        helper_symbols = (
            "workflow_node_id",
            "from_indices",
            "indices",
            "path_for_ast",
            "for_main",
            "for_process",
            "ownership_map",
            "into_ownership_map",
            "workflow_projection",
            "statement_list",
            "push_statement_list",
            "is_statement_list",
            "collect_body",
            "collect_statement",
            "statement_value",
            "map_node_subtree",
            "check_shape",
            "process_wrapper_run_path",
            "execution_sites",
            "collect_execution_sites",
            "push_execution_site_descriptor",
            "collect_child_execution_sites",
            "workflow_owner",
            "node_site",
            "branch_site",
            "branch_edge_id",
        )
        for constant in constants:
            guarded = {
                symbol
                for guard in surfaces[constant].guards
                if guard.kind == "rust_items"
                for symbol in guard.symbols
            }
            self.assertLessEqual(set(helper_symbols), guarded)

        symbols = ", ".join(f'"{symbol}"' for symbol in helper_symbols)
        config = f"""
        [[surface]]
        constant = "WORKFLOW_GRAPH_SCHEMA_VERSION"
        constant_path = "src/versions.rs"
        description = "fixture graph node identity"

        [[surface.guard]]
        kind = "rust_items"
        paths = ["src/identity.rs"]
        symbols = [{symbols}]

        [[surface]]
        constant = "LASHLANG_SEGMENT_STATE_VERSION"
        constant_path = "src/versions.rs"
        description = "fixture persisted runtime node identity"

        [[surface.guard]]
        kind = "rust_items"
        paths = ["src/identity.rs"]
        symbols = [{symbols}]
        """
        source = "\n".join(f"fn {symbol}() {{ stable(); }}" for symbol in helper_symbols) + "\n"
        for symbol in helper_symbols:
            with self.subTest(symbol=symbol):
                fixture = FixtureRepository(config)
                self.addCleanup(fixture.close)
                fixture.write_file(
                    "src/versions.rs",
                    "pub const WORKFLOW_GRAPH_SCHEMA_VERSION: u32 = 14;\n"
                    "pub const LASHLANG_SEGMENT_STATE_VERSION: u32 = 13;\n",
                )
                fixture.write_file("src/identity.rs", source)
                base = fixture.commit("node identity helper base")
                fixture.write_file(
                    "src/identity.rs",
                    source.replace(
                        f"fn {symbol}() {{ stable(); }}",
                        f"fn {symbol}() {{ changed(); }}",
                    ),
                )
                head = fixture.commit(f"change {symbol} without bumps")

                result = self.check(fixture, base, head)

                self.assertEqual(result.errors, ())
                self.assertEqual(
                    {failure.surface.constant for failure in result.failures},
                    constants,
                )

    def test_workflow_execution_site_shape_demands_graph_and_trace_bumps(self) -> None:
        constants = {"WORKFLOW_GRAPH_SCHEMA_VERSION", "TRACE_SCHEMA_VERSION"}
        surfaces = {
            surface.constant: surface
            for surface in MODULE.load_config(REAL_CONFIG)
            if surface.constant in constants
        }
        workflow_path = "crates/lash-sansio/src/workflow.rs"
        for constant in constants:
            guards = [
                guard
                for guard in surfaces[constant].guards
                if guard.kind == "rust_items" and guard.paths == (workflow_path,)
            ]
            self.assertEqual(len(guards), 1)
            self.assertIn("WorkflowExecutionSite", guards[0].symbols)

        config = """
        [[surface]]
        constant = "WORKFLOW_GRAPH_SCHEMA_VERSION"
        constant_path = "src/versions.rs"
        description = "fixture graph site carrier"

        [[surface.guard]]
        kind = "rust_items"
        paths = ["src/workflow.rs"]
        symbols = ["WorkflowExecutionSite"]

        [[surface]]
        constant = "TRACE_SCHEMA_VERSION"
        constant_path = "src/versions.rs"
        description = "fixture trace site carrier"

        [[surface.guard]]
        kind = "rust_items"
        paths = ["src/workflow.rs"]
        symbols = ["WorkflowExecutionSite"]
        """
        fixture = FixtureRepository(config)
        self.addCleanup(fixture.close)
        fixture.write_file(
            "src/versions.rs",
            "pub const WORKFLOW_GRAPH_SCHEMA_VERSION: u32 = 14;\n"
            "pub const TRACE_SCHEMA_VERSION: u32 = 25;\n",
        )
        source = "pub struct WorkflowExecutionSite { pub path: Vec<u32> }\n"
        fixture.write_file("src/workflow.rs", source)
        base = fixture.commit("workflow site base")
        fixture.write_file(
            "src/workflow.rs",
            source.replace("pub path: Vec<u32>", "pub path: Vec<u64>"),
        )
        head = fixture.commit("change workflow site without bumps")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(
            {failure.surface.constant for failure in result.failures}, constants
        )

    def test_rust_impl_guard_detects_custom_serializer_changes(self) -> None:
        config = """
        [[surface]]
        constant = "WIRE_VERSION"
        constant_path = "src/lib.rs"
        description = "fixture custom serializer"

        [[surface.guard]]
        kind = "rust_impls"
        paths = ["src/wire.rs"]
        symbols = ["Serialize for WireValue"]
        """
        fixture = FixtureRepository(config)
        self.addCleanup(fixture.close)
        fixture.write_file("src/lib.rs", "pub const WIRE_VERSION: u32 = 1;\n")
        fixture.write_file(
            "src/wire.rs",
            "impl Serialize for WireValue { fn serialize(&self) { write(1); } }\n",
        )
        base = fixture.commit("custom serializer base")
        fixture.write_file(
            "src/wire.rs",
            "impl Serialize for WireValue { fn serialize(&self) { write(2); } }\n",
        )
        head = fixture.commit("change custom serializer without version bump")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(
            {failure.surface.constant for failure in result.failures},
            {"WIRE_VERSION"},
        )

    def test_the_vm_abi_surface_covers_every_ability_leaf_host_rs_declares(
        self,
    ) -> None:
        """No type an ability op reaches may be guarded by nothing.

        The abilities are an in-process contract, so the reachable leaves are
        found by walking the guarded item text rather than by trusting a list
        someone kept up to date. `ExecutionHostError` is deliberately absent
        from the ABI guard -- it is durable and already fenced by the
        continuation surface -- so the contract is that the union covers the
        closure, not that one guard does.
        """
        surfaces = MODULE.load_config(REAL_CONFIG)
        host_path = "crates/lashlang/src/runtime/host.rs"
        abi = next(
            surface
            for surface in surfaces
            if surface.constant == "LASHLANG_VM_ABI_VERSION"
        )
        abi_guard = next(guard for guard in abi.guards if guard.paths == (host_path,))
        continuation = next(
            surface
            for surface in surfaces
            if surface.constant == "VM_CONTINUATION_FORMAT_VERSION"
        )
        continuation_guard = next(
            guard for guard in continuation.guards if guard.paths == (host_path,)
        )
        self.assertEqual(abi_guard.kind, "rust_items")

        source = (Path(MODULE.ROOT) / host_path).read_text(encoding="utf-8")
        declared = {
            match.group(1) for match in MODULE.RUST_SERDE_SHAPE.finditer(source)
        }
        items = MODULE.named_rust_items(source, declared)
        reachable = {"AbilityOp", "AbilityResult"}
        pending = list(reachable)
        while pending:
            body = items[pending.pop()]
            for name in declared - reachable:
                if re.search(rf"\b{re.escape(name)}\b", body):
                    reachable.add(name)
                    pending.append(name)

        self.assertIn("ExecutionHostError", reachable)
        self.assertIn("ExecutionHostError", continuation_guard.symbols)
        self.assertLessEqual(
            reachable, set(abi_guard.symbols) | set(continuation_guard.symbols)
        )
        self.assertLessEqual(set(abi_guard.symbols), declared)

    def test_each_ability_change_demands_a_vm_abi_bump(self) -> None:
        mutations = {
            "new ability": ("    Await(Value),", "    Await(Value),\n    Print(Value),"),
            "boxed receiver": (
                "ResourceOperation(Box<ResourceOperation>)",
                "ResourceOperation(ResourceOperation)",
            ),
            "retired result arm": ("    Unit,\n", ""),
            "batch result field": (
                "    pub settlement_order: Vec<usize>,\n",
                "",
            ),
        }
        for name, (before, after) in mutations.items():
            with self.subTest(name=name):
                fixture = FixtureRepository(ABILITY_CONFIG)
                self.addCleanup(fixture.close)
                fixture.write_file(
                    "src/artifact.rs",
                    'pub const LASHLANG_VM_ABI_VERSION: &str = "lashlang-vm-abi-v8";\n',
                )
                fixture.write_file("src/host.rs", ABILITY_SHAPES)
                base = fixture.commit("base")
                fixture.write_file("src/host.rs", ABILITY_SHAPES.replace(before, after))
                head = fixture.commit(f"change {name} without bump")

                result = self.check(fixture, base, head)

                self.assertEqual(result.errors, ())
                self.assertEqual(len(result.failures), 1)
                self.assertEqual(
                    result.failures[0].surface.constant, "LASHLANG_VM_ABI_VERSION"
                )
                self.assertEqual(result.failures[0].base_version, 8)
                self.assertEqual(result.failures[0].head_version, 8)

    def test_the_semantic_hash_surface_covers_the_whole_builtin_registry(self) -> None:
        """Every registered intrinsic is inside the guarded text, not just the first.

        A table is exactly the shape an item walk can truncate after one entry,
        and a guard that covers a table's head while appearing to cover the
        table is worse than no guard: appending to it changes nothing the check
        can see.
        """
        semantic = next(
            surface
            for surface in MODULE.load_config(REAL_CONFIG)
            if surface.constant == "LASHLANG_SEMANTIC_HASH_VERSION"
        )
        registry_path = "crates/lashlang/src/builtins.rs"
        guard = next(
            guard for guard in semantic.guards if guard.paths == (registry_path,)
        )
        self.assertEqual(guard.kind, "rust_items")
        self.assertIn("TYPESCRIPT_BUILTINS", guard.symbols)

        source = (Path(MODULE.ROOT) / registry_path).read_text(encoding="utf-8")
        guarded = MODULE.named_rust_items(source, guard.symbols)
        intrinsics = re.findall(r'name: "(__typescript_\w+)"', source)

        self.assertGreater(len(intrinsics), 1)
        for name in intrinsics:
            self.assertIn(f'"{name}"', guarded["TYPESCRIPT_BUILTINS"])

    def test_a_registry_entry_change_demands_a_semantic_hash_bump(self) -> None:
        mutations = {
            "appended dialect intrinsic": (
                '    Builtin {\n        name: "__typescript_stdlib",\n'
                "        arity: Arity::AtLeast(1),\n    },\n",
                '    Builtin {\n        name: "__typescript_stdlib",\n'
                "        arity: Arity::AtLeast(1),\n    },\n"
                '    Builtin {\n        name: "__typescript_btoa",\n'
                "        arity: Arity::Exact(1),\n    },\n",
            ),
            "retired source builtin": (
                '    Builtin {\n        name: "join",\n'
                "        arity: Arity::Exact(2),\n    },\n",
                "",
            ),
            "widened arity": ("Arity::Exact(2),\n    },\n];", "Arity::AtLeast(2),\n    },\n];"),
        }
        for name, (before, after) in mutations.items():
            with self.subTest(name=name):
                fixture = FixtureRepository(REGISTRY_CONFIG)
                self.addCleanup(fixture.close)
                fixture.write_file(
                    "src/identity.rs",
                    "pub const LASHLANG_SEMANTIC_HASH_VERSION: &str = "
                    '"lashlang-semantic-v15";\n',
                )
                fixture.write_file("src/builtins.rs", REGISTRY_SOURCE)
                base = fixture.commit("base")
                mutated = REGISTRY_SOURCE.replace(before, after)
                self.assertNotEqual(mutated, REGISTRY_SOURCE)
                fixture.write_file("src/builtins.rs", mutated)
                head = fixture.commit(f"change {name} without bump")

                result = self.check(fixture, base, head)

                self.assertEqual(result.errors, ())
                self.assertEqual(len(result.failures), 1)
                self.assertEqual(
                    result.failures[0].surface.constant,
                    "LASHLANG_SEMANTIC_HASH_VERSION",
                )
                self.assertEqual(result.failures[0].base_version, 15)
                self.assertEqual(result.failures[0].head_version, 15)

    def test_the_semantic_hash_surface_guards_the_typescript_lowering(self) -> None:
        """The lowerer and the role table sit inside the surface's whole-file guard.

        A lowering change moves every module-ref preimage while touching no
        named item, so only a `file` guard over `crates/lash-typescript/src/
        lower/**` and `crates/lashlang/src/ast_roles.rs` sees it. The guard's
        markers are also checked against the real matched files, so a marker
        that names nothing the guard covers fails here.
        """
        semantic = next(
            surface
            for surface in MODULE.load_config(REAL_CONFIG)
            if surface.constant == "LASHLANG_SEMANTIC_HASH_VERSION"
        )
        guard = next(
            guard for guard in semantic.guards if guard.kind == "file"
        )
        self.assertIn("crates/lash-typescript/src/lower/**", guard.paths)
        self.assertIn("crates/lashlang/src/ast_roles.rs", guard.paths)

        view = MODULE.RepositoryView(MODULE.ROOT)
        matched = view.matching_paths("HEAD", guard.paths)
        self.assertIn("crates/lash-typescript/src/lower/mod.rs", matched)
        self.assertIn("crates/lashlang/src/ast_roles.rs", matched)
        contents = [
            view.content("HEAD", path) or "" for path in matched
        ]
        for marker in guard.must_cover:
            self.assertTrue(
                any(marker in content for content in contents),
                f"no guarded file carries {marker!r}",
            )

    def test_a_lowering_or_role_change_demands_a_semantic_hash_bump(self) -> None:
        mutations = {
            "lowered shape": (
                "src/lower/calls.rs",
                "LashExpr::Read(member)",
                "LashExpr::Convert(member)",
            ),
            "a new lowering file": (
                "src/lower/nested/added.rs",
                None,
                "impl Lowerer {\n    fn added(&self) {}\n}\n",
            ),
            "role recognition": (
                "src/ast_roles.rs",
                "CollectionTransformParts;",
                "CollectionTransformParts { guarded: bool }",
            ),
        }
        for name, (path, before, after) in mutations.items():
            with self.subTest(name=name):
                fixture = FixtureRepository(LOWERING_CONFIG)
                self.addCleanup(fixture.close)
                fixture.write_file(
                    "src/identity.rs",
                    "pub const LASHLANG_SEMANTIC_HASH_VERSION: &str = "
                    '"lashlang-semantic-v15";\n',
                )
                fixture.write_file("src/lower/calls.rs", LOWERING_SOURCE)
                fixture.write_file("src/ast_roles.rs", AST_ROLES_SOURCE)
                base = fixture.commit("base")
                if before is None:
                    fixture.write_file(path, after)
                else:
                    fixture.write_file(
                        path,
                        (LOWERING_SOURCE if "lower" in path else AST_ROLES_SOURCE)
                        .replace(before, after),
                    )
                head = fixture.commit(f"change {name} without bump")

                result = self.check(fixture, base, head)

                self.assertEqual(result.errors, ())
                self.assertEqual(len(result.failures), 1)
                self.assertEqual(
                    result.failures[0].surface.constant,
                    "LASHLANG_SEMANTIC_HASH_VERSION",
                )
                self.assertEqual(result.failures[0].base_version, 15)
                self.assertEqual(result.failures[0].head_version, 15)

    def test_the_bytecode_surface_covers_every_enum_the_stream_encodes(self) -> None:
        """No enum an instruction carries by value may be guarded by nothing.

        `Instruction::Intrinsic(IntrinsicOp)` carries no discriminant of its
        own, so the intrinsic vocabulary is an arm set `Instruction`'s own text
        never mentions; the operator enums are the same shape. The closure is
        walked out of the guarded item text rather than compared against a list
        someone kept up to date, because the list going stale silently is the
        defect this registration closes.
        """
        surfaces = MODULE.load_config(REAL_CONFIG)
        bytecode = next(
            surface
            for surface in surfaces
            if surface.constant == "BYTECODE_FORMAT_VERSION"
        )
        item_guards = {
            guard.paths[0]: guard
            for guard in bytecode.guards
            if guard.kind == "rust_items"
        }
        instruction_path = "crates/lashlang/src/runtime/instruction.rs"
        self.assertIn(instruction_path, item_guards)
        self.assertIn("Instruction", item_guards[instruction_path].symbols)

        declarations: dict[str, str] = {}
        for path, guard in item_guards.items():
            source = (Path(MODULE.ROOT) / path).read_text(encoding="utf-8")
            declared = {
                match.group(1) for match in MODULE.RUST_SERDE_SHAPE.finditer(source)
            }
            declarations.update(MODULE.named_rust_items(source, declared))
            self.assertLessEqual(set(guard.symbols), declared)

        # A type appears in a payload position: after `(`, `,`, `:` or `<`,
        # possibly behind a wrapper. A variant name appears after `{` or `}`,
        # so it is not mistaken for one.
        payload_type = re.compile(
            r"[(,:<]\s*(?:Box<|Option<|Vec<|&|\[)*([A-Z][A-Za-z0-9_]*)"
        )
        reachable = {"Instruction"}
        pending = ["Instruction"]
        while pending:
            body = declarations[pending.pop()]
            for match in payload_type.finditer(body):
                name = match.group(1)
                if name in declarations and name not in reachable:
                    reachable.add(name)
                    pending.append(name)

        self.assertIn("IntrinsicOp", reachable)
        self.assertIn("BinaryOp", reachable)
        guarded = {
            symbol for guard in item_guards.values() for symbol in guard.symbols
        }
        self.assertLessEqual(reachable, guarded)

    def test_each_instruction_vocabulary_change_demands_a_bytecode_bump(self) -> None:
        mutations = {
            "appended intrinsic": ("    Reverse,\n", "    Reverse,\n    Unique,\n"),
            "reordered intrinsic arms": ("    Sort,\n    Reverse,\n", "    Reverse,\n    Sort,\n"),
            "re-payloaded intrinsic": ("    Slice,\n", "    Slice(usize),\n"),
            "retired intrinsic": ("    Sort,\n", ""),
        }
        for name, (before, after) in mutations.items():
            with self.subTest(name=name):
                fixture = FixtureRepository(BYTECODE_CONFIG)
                self.addCleanup(fixture.close)
                fixture.write_file(
                    "src/lib.rs", "pub const BYTECODE_FORMAT_VERSION: u32 = 17;\n"
                )
                fixture.write_file("src/instruction.rs", BYTECODE_INSTRUCTIONS)
                fixture.write_file("src/ast.rs", BYTECODE_OPERATORS)
                base = fixture.commit("base")
                mutated = BYTECODE_INSTRUCTIONS.replace(before, after)
                self.assertNotEqual(mutated, BYTECODE_INSTRUCTIONS)
                fixture.write_file("src/instruction.rs", mutated)
                head = fixture.commit(f"change {name} without bump")

                result = self.check(fixture, base, head)

                self.assertEqual(result.errors, ())
                self.assertEqual(len(result.failures), 1)
                self.assertEqual(
                    result.failures[0].surface.constant, "BYTECODE_FORMAT_VERSION"
                )
                self.assertEqual(result.failures[0].base_version, 17)
                self.assertEqual(result.failures[0].head_version, 17)

    def test_an_operator_vocabulary_change_demands_a_bytecode_bump(self) -> None:
        fixture = FixtureRepository(BYTECODE_CONFIG)
        self.addCleanup(fixture.close)
        fixture.write_file(
            "src/lib.rs", "pub const BYTECODE_FORMAT_VERSION: u32 = 17;\n"
        )
        fixture.write_file("src/instruction.rs", BYTECODE_INSTRUCTIONS)
        fixture.write_file("src/ast.rs", BYTECODE_OPERATORS)
        base = fixture.commit("base")
        fixture.write_file(
            "src/ast.rs", BYTECODE_OPERATORS.replace("    Add,\n", "    Add,\n    Power,\n")
        )
        head = fixture.commit("add an operator without bump")

        result = self.check(fixture, base, head)

        self.assertEqual(result.errors, ())
        self.assertEqual(len(result.failures), 1)
        self.assertEqual(
            result.failures[0].surface.constant, "BYTECODE_FORMAT_VERSION"
        )

    def test_a_guarded_item_keeps_its_delimiters_apart(self) -> None:
        """A `;` inside brackets is an array length, not the item's terminator."""
        source = ARRAY_LENGTH_ITEM
        extracted = MODULE.named_rust_items(source, ["LANES"])["LANES"]

        self.assertEqual(
            extracted,
            "pub(crate)constLANES:[Lane;2]=[Lane::First,Lane::Second];",
        )

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


BINARY_CONFIG = """
[[surface]]
constant = "FIXTURE_VERSION"
constant_path = "src/lib.rs"
description = "checked-in binary fixtures"

[[surface.guard]]
kind = "file"
paths = ["fixtures/*"]
must_cover = ["SQLite format 3"]
"""


class BinaryFileGuardTest(unittest.TestCase):
    """A `file` guard over binary payloads must compare every byte.

    These pin the two ways a lenient decode silently un-guards binary content:
    `errors="replace"`, which maps every invalid byte to one U+FFFD, and
    subprocess text mode's universal-newline translation, which folds CR and
    CRLF into LF. Both make a real change to a durable fixture read as no
    change, which is worse than the strict-UTF-8 crash they replace.
    """

    def fixture(self, payload: bytes) -> tuple[FixtureRepository, str]:
        fixture = FixtureRepository(BINARY_CONFIG)
        self.addCleanup(fixture.close)
        fixture.write_file("src/lib.rs", "const FIXTURE_VERSION: u32 = 1;\n")
        self.write_bytes(fixture, "fixtures/store.db", payload)
        return fixture, fixture.commit("base fixture")

    @staticmethod
    def write_bytes(fixture: FixtureRepository, path: str, payload: bytes) -> None:
        destination = fixture.root / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(payload)

    # A plausible SQLite prologue: the format marker, an invalid UTF-8 byte,
    # and a lone CR, which universal-newline translation maps onto the very
    # LF that replaces it below -- so the two payloads decode identically
    # unless the bytes are compared as bytes.
    BASE_PAYLOAD = b"SQLite format 3\x00\x8a\x0dpage\x00"

    def test_an_invalid_byte_change_in_a_binary_fixture_needs_a_bump(self) -> None:
        fixture, base = self.fixture(self.BASE_PAYLOAD)
        self.write_bytes(
            fixture,
            "fixtures/store.db",
            self.BASE_PAYLOAD.replace(b"\x8a", b"\x81"),
        )
        head = fixture.commit("flip one invalid byte")

        result = self.check_result(fixture, base, head)

        self.assertEqual(len(result.failures), 1)
        self.assertEqual(result.failures[0].surface.constant, "FIXTURE_VERSION")

    def test_a_cr_to_lf_change_in_a_binary_fixture_needs_a_bump(self) -> None:
        fixture, base = self.fixture(self.BASE_PAYLOAD)
        self.write_bytes(
            fixture,
            "fixtures/store.db",
            self.BASE_PAYLOAD.replace(b"\x0d", b"\x0a"),
        )
        head = fixture.commit("turn a lone CR into an LF")

        result = self.check_result(fixture, base, head)

        self.assertEqual(len(result.failures), 1)
        self.assertEqual(result.failures[0].surface.constant, "FIXTURE_VERSION")

    def test_a_bumped_binary_fixture_change_passes(self) -> None:
        fixture, base = self.fixture(self.BASE_PAYLOAD)
        self.write_bytes(
            fixture,
            "fixtures/store.db",
            self.BASE_PAYLOAD.replace(b"\x8a", b"\x81"),
        )
        fixture.write_file("src/lib.rs", "const FIXTURE_VERSION: u32 = 2;\n")
        head = fixture.commit("flip one invalid byte and bump")

        result = self.check_result(fixture, base, head)

        self.assertEqual(result.failures, ())
        self.assertEqual(result.errors, ())

    def test_an_unchanged_binary_fixture_needs_no_bump(self) -> None:
        fixture, base = self.fixture(self.BASE_PAYLOAD)
        fixture.write_file("src/other.rs", "// unrelated\n")
        head = fixture.commit("unrelated change")

        result = self.check_result(fixture, base, head)

        self.assertEqual(result.failures, ())
        self.assertEqual(result.errors, ())

    def test_git_output_round_trips_invalid_bytes_and_carriage_returns(self) -> None:
        fixture, base = self.fixture(self.BASE_PAYLOAD)

        shown = MODULE.git(fixture.root, "show", f"{base}:fixtures/store.db").stdout

        self.assertEqual(
            shown.encode("utf-8", errors="surrogateescape"), self.BASE_PAYLOAD
        )

    def check_result(self, fixture: FixtureRepository, base: str, head: str):
        surfaces = MODULE.load_config(fixture.root / "surface.toml")
        return MODULE.check_surfaces(fixture.root, base, head, surfaces)



if __name__ == "__main__":
    unittest.main()
