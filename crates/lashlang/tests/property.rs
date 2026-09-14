//! Property tests over the IR, the VM and the snapshot encoding.
//!
//! ADR 0096 makes TypeScript the sole authored RLM dialect, so every generated
//! program below is TypeScript lowered through `lash_typescript`, and the
//! generators emit TypeScript rather than the retired surface. The type-literal
//! generator emits `TypeExpr` directly: `Type { .. }` had no TypeScript
//! spelling, and the JSON-schema law it pins is a property of the IR.
//!
//! The code→graph→code laws moved out with the workflow-graph lens: rendering
//! and re-reading a program's canonical source is the lens's own contract, and
//! the lens is a TypeScript-facing surface under FIG-3033. Its round-trip law
//! is pinned there against the TypeScript printer rather than here against a
//! retired one.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, ImageValue,
    ProjectedFuture, ProjectedHostDescriptor, ProjectedReadRequest, ProjectedReadResponse,
    ProjectedValue, Record, ResourceHandle, Snapshot, State, TypeExpr, TypeField, Value,
};
use proptest::prelude::*;

#[path = "support/execute.rs"]
mod execute_support;

use execute_support::{ExecuteError, execute};

#[derive(Default)]
struct DeterministicHost;

impl ExecutionHost for DeterministicHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => match operation.operation.as_str() {
                "echo" => Ok(AbilityResult::Value(
                    operation
                        .args
                        .first()
                        .and_then(Value::as_record)
                        .and_then(|record| record.get("value"))
                        .cloned()
                        .unwrap_or(Value::Null),
                )),
                "fail" => Err(ExecutionHostError::new("fail")),
                _ => Err(ExecutionHostError::new(format!(
                    "unknown module operation: {}",
                    operation.operation
                ))),
            },
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "proptest driver builds the fixed-configuration tokio runtime, per the message"
)]
fn run_execute(
    source: &str,
    state: &mut State,
    host: &DeterministicHost,
) -> Result<ExecutionOutcome, ExecuteError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(execute(source, state, host, property_host_environment()))
}

fn finished(outcome: ExecutionOutcome) -> Value {
    match outcome {
        ExecutionOutcome::Finished(value) => value,
        ExecutionOutcome::Continued => panic!("expected `finish`"),
        ExecutionOutcome::Failed(value) => panic!("unexpected process failure: {value}"),
    }
}

#[expect(
    clippy::expect_used,
    reason = "fixture catalog registers each host operation once into a fresh catalog, per each message"
)]
fn property_host_environment() -> lashlang::LashlangHostEnvironment {
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "echo",
            "echo",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "fail",
            "fail",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    lashlang::LashlangHostEnvironment::new(resources, lashlang::LashlangAbilities::all())
}

#[derive(Clone, Debug)]
enum GenValue {
    Null,
    Bool(bool),
    Number(i32),
    String(String),
    List(Vec<GenValue>),
    Record(Vec<(String, GenValue)>),
}

fn generated_string_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            proptest::char::range(' ', '~'),
            Just('\n'),
            Just('\r'),
            Just('\t'),
        ],
        0..20,
    )
    .prop_map(|chars: Vec<char>| chars.into_iter().collect())
}

impl GenValue {
    fn to_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(*value),
            Self::Number(value) => Value::Number(*value as f64),
            Self::String(value) => Value::String(value.clone().into()),
            Self::List(values) => {
                Value::List(values.iter().map(Self::to_value).collect::<Vec<_>>().into())
            }
            Self::Record(entries) => Value::Record(
                entries
                    .iter()
                    .map(|(key, value)| (key.clone(), value.to_value()))
                    .collect::<lashlang::Record>()
                    .into(),
            ),
        }
    }

    fn to_source(&self) -> String {
        match self {
            Self::Null => "null".to_string(),
            Self::Bool(value) => value.to_string(),
            Self::Number(value) => value.to_string(),
            Self::String(value) => encode_string(value),
            Self::List(values) => format!(
                "[{}]",
                values
                    .iter()
                    .map(Self::to_source)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Record(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(key, value)| format!("{key}: {}", value.to_source()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

fn ident_strategy() -> impl Strategy<Value = String> {
    "[a-z_][a-z0-9_]{0,10}".prop_filter("reserved TypeScript name", |ident| {
        !matches!(
            ident.as_str(),
            "if" | "else"
                | "for"
                | "of"
                | "in"
                | "do"
                | "while"
                | "break"
                | "continue"
                | "return"
                | "function"
                | "class"
                | "new"
                | "this"
                | "typeof"
                | "instanceof"
                | "void"
                | "delete"
                | "try"
                | "catch"
                | "finally"
                | "throw"
                | "switch"
                | "case"
                | "default"
                | "const"
                | "let"
                | "var"
                | "await"
                | "async"
                | "yield"
                | "true"
                | "false"
                | "null"
                | "undefined"
                | "finish"
                | "print"
                | "start"
                | "sleep"
                | "wake"
        )
    })
}

fn gen_value_strategy() -> impl Strategy<Value = GenValue> {
    let leaf = prop_oneof![
        Just(GenValue::Null),
        any::<bool>().prop_map(GenValue::Bool),
        (-10_000i32..=10_000i32).prop_map(GenValue::Number),
        generated_string_strategy().prop_map(GenValue::String),
    ];

    leaf.prop_recursive(4, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(GenValue::List),
            prop::collection::vec((ident_strategy(), inner), 0..4).prop_map(GenValue::Record),
        ]
    })
}

fn encode_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn globals_strategy() -> impl Strategy<Value = HashMap<String, GenValue>> {
    prop::collection::hash_map(ident_strategy(), gen_value_strategy(), 0..6)
}

#[derive(Debug)]
struct SnapshotProjectedDescriptor;

impl ProjectedHostDescriptor for SnapshotProjectedDescriptor {
    fn type_name(&self) -> &str {
        "snapshot_property"
    }

    /// Identity only: this descriptor answers no read (FIG-2863).
    fn read_one(
        &self,
        _request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, Option<ProjectedReadResponse>> {
        Box::pin(async { None })
    }
}

fn snapshot_string_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            any::<char>(),
            Just('\0'),
            Just('\u{7f}'),
            Just('\u{80}'),
            Just('\u{fffd}'),
            Just('\u{10ffff}'),
        ],
        0..24,
    )
    .prop_map(|characters| characters.into_iter().collect())
}

#[expect(
    clippy::expect_used,
    reason = "the corpus strategy seeds a literal image/png attachment, which parses, per the message"
)]
fn canonical_snapshot_variant_corpus_strategy() -> impl Strategy<Value = Vec<Value>> {
    (
        any::<u64>(),
        any::<bool>(),
        snapshot_string_strategy(),
        0_u64..1_000_000,
        any::<Option<u32>>(),
        any::<Option<u32>>(),
        snapshot_string_strategy(),
    )
        .prop_map(
            |(number_bits, boolean, text, image_size, width, height, projection_text)| {
                let projection_ref = serde_json::json!({
                    "z-last": [projection_text, 7, true, null],
                    "a-first": {"nul": "\0", "edge": "\u{fffd}"},
                });
                let projected = Value::Projected(ProjectedValue::custom_with_projection_ref(
                    "session.items[3]",
                    Arc::new(SnapshotProjectedDescriptor),
                    projection_ref,
                ));
                let tuple =
                    Value::Tuple(vec![Value::String(text.clone().into()), Value::Null].into());
                let list = Value::List(vec![Value::Bool(boolean), tuple.clone()].into());
                let record = Value::Record(Arc::new(
                    [
                        ("z-last".to_string(), list.clone()),
                        ("a-first".to_string(), projected.clone()),
                    ]
                    .into_iter()
                    .collect(),
                ));

                vec![
                    Value::Null,
                    Value::Undefined,
                    Value::Bool(boolean),
                    Value::Number(f64::from_bits(number_bits)),
                    Value::Number(f64::INFINITY),
                    Value::Number(f64::NEG_INFINITY),
                    Value::String(text.into()),
                    Value::Image(Box::new(ImageValue::new(
                        "image-id",
                        lashlang::MediaType::parse("image/png").expect("valid media type"),
                        "label\0\u{fffd}",
                        image_size,
                        width,
                        height,
                    ))),
                    Value::Resource(ResourceHandle::new("files", "workspace\0\u{fffd}")),
                    tuple,
                    list,
                    record,
                    projected,
                ]
            },
        )
}

#[expect(
    clippy::expect_used,
    reason = "the round-trip property holds per the same-name rule, so the field is present, per the message"
)]
fn assert_canonical_value_round_trip(expected: &Value, actual: &Value) {
    match (expected, actual) {
        (Value::Null, Value::Null) => {}
        (Value::Undefined, Value::Undefined) => {}
        (Value::Bool(expected), Value::Bool(actual)) => assert_eq!(actual, expected),
        (Value::Number(expected), Value::Number(actual)) if expected.is_nan() => {
            assert!(actual.is_nan());
            assert_eq!(actual.to_bits(), 0x7ff8_0000_0000_0000);
        }
        (Value::Number(expected), Value::Number(actual)) => {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
        (Value::String(expected), Value::String(actual)) => assert_eq!(actual, expected),
        (Value::Image(expected), Value::Image(actual)) => assert_eq!(actual, expected),
        (Value::Resource(expected), Value::Resource(actual)) => assert_eq!(actual, expected),
        (Value::Tuple(expected), Value::Tuple(actual))
        | (Value::List(expected), Value::List(actual)) => {
            assert_eq!(actual.len(), expected.len());
            for (expected, actual) in expected.iter().zip(actual.iter()) {
                assert_canonical_value_round_trip(expected, actual);
            }
        }
        (Value::Record(expected), Value::Record(actual)) => {
            assert_eq!(actual.len(), expected.len());
            for (name, expected) in expected.iter() {
                assert_canonical_value_round_trip(
                    expected,
                    actual.get(name).expect("round-tripped record field"),
                );
            }
        }
        (Value::Projected(expected), Value::Projected(actual)) => {
            assert_eq!(actual.name(), expected.name());
            assert_eq!(actual.type_name(), expected.type_name());
            assert_eq!(actual.projection_ref(), expected.projection_ref());
        }
        (expected, actual) => panic!("snapshot value changed variant: {expected:?} -> {actual:?}"),
    }
}

/// One statement in a generated heap-shaping program.
///
/// The interesting states for the persistence oracle are the ones a real
/// session reaches: containers built, aliased, appended to, mutated through a
/// path, iterated, and discarded so the heap ends up with vacant storage slots.
#[derive(Clone, Debug)]
enum HeapStep {
    Seed(u8),
    Alias,
    PushRow(u8),
    ConcatRow(u8),
    NestInRecord,
    MutateFirst(u8),
    Discard,
    IterateCopy,
}

impl HeapStep {
    fn to_source(&self, index: usize) -> String {
        match self {
            Self::Seed(n) => format!("base = [[{n}], [{}]];\n", n.wrapping_add(1)),
            Self::Alias => "alias = base;\n".to_string(),
            Self::PushRow(n) => format!("base.push([{n}]);\n"),
            Self::ConcatRow(n) => format!("base = base.concat([[{n}]]);\n"),
            Self::NestInRecord => "holder = { rows: base, tag: \"held\" };\n".to_string(),
            Self::MutateFirst(n) => format!("base[0] = [{n}];\n"),
            Self::Discard => {
                format!("let scratch{index} = [[9], [8], [7]];\nscratch{index} = 0;\n")
            }
            Self::IterateCopy => format!(
                "let copies{index} = [];\nfor (const row of base) {{ copies{index} = copies{index}.concat([row]); }}\n"
            ),
        }
    }
}

/// The prologue every generated heap program needs.
///
/// TypeScript binds names before they are assigned, so the containers the
/// steps reshape are declared once up front; the steps themselves only assign.
const HEAP_PROLOGUE: &str = "let base = [];\nlet alias = null;\nlet holder = null;\n";

fn heap_step_strategy() -> impl Strategy<Value = HeapStep> {
    prop_oneof![
        any::<u8>().prop_map(HeapStep::Seed),
        Just(HeapStep::Alias),
        any::<u8>().prop_map(HeapStep::PushRow),
        any::<u8>().prop_map(HeapStep::ConcatRow),
        Just(HeapStep::NestInRecord),
        any::<u8>().prop_map(HeapStep::MutateFirst),
        Just(HeapStep::Discard),
        Just(HeapStep::IterateCopy),
    ]
}

fn heap_program_strategy() -> impl Strategy<Value = Vec<HeapStep>> {
    (
        any::<u8>().prop_map(HeapStep::Seed),
        proptest::collection::vec(heap_step_strategy(), 0..8),
    )
        .prop_map(|(seed, rest)| std::iter::once(seed).chain(rest).collect())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        max_local_rejects: 10_000,
        .. ProptestConfig::default()
    })]

    #[test]
    fn parse_never_panics_on_arbitrary_input(source in ".*") {
        let result = catch_unwind(AssertUnwindSafe(|| lash_typescript::parse(&source)));
        prop_assert!(result.is_ok(), "parse panicked for input: {source:?}");
    }

    #[test]
    fn execute_never_panics_on_arbitrary_input(source in ".*") {
        let host = DeterministicHost;
        let mut state = State::new();
        let result = catch_unwind(AssertUnwindSafe(|| run_execute(&source, &mut state, &host)));
        prop_assert!(result.is_ok(), "execute panicked for input: {source:?}");
    }

    #[test]
    fn generated_value_programs_round_trip_through_parser_and_runtime(
        ident in ident_strategy(),
        value in gen_value_strategy()
    ) {
        let expected = value.to_value();
        let source = format!("const {ident} = {};\nfinish({ident});\n", value.to_source());
        let host = DeterministicHost;
        let mut state = State::new();

        let actual = finished(
            run_execute(&source, &mut state, &host)
                .expect("generated value program should execute")
        );

        prop_assert_eq!(actual, expected.clone());
        let globals = state.globals();
        prop_assert_eq!(globals.get(&ident), Some(&expected));
    }


    /// `decode(encode(state)) == state` for states a program actually produces.
    ///
    /// Snapshot equality is the oracle every persistence test leans on, so this
    /// exercises it over heap-backed states rather than plain trees: it fails if
    /// equality is too strict (comparing private storage layout a round trip
    /// compacts) and it fails if a round trip loses roots, objects or meters.
    /// The encoded bytes must also be a fixed point, and the restored state must
    /// answer the same as the original when execution continues on it.
    #[test]
    fn heap_backed_state_round_trips_as_an_equal_snapshot(
        steps in heap_program_strategy()
    ) {
        let body = steps
            .iter()
            .enumerate()
            .map(|(index, step)| step.to_source(index))
            .collect::<String>();
        let source = format!("{HEAP_PROLOGUE}{body}finish(0);\n");
        let host = DeterministicHost;
        let mut state = State::new();
        prop_assume!(run_execute(&source, &mut state, &host).is_ok());

        let snapshot = state.snapshot();
        let encoded = snapshot.to_canonical_bytes().expect("heap-backed snapshot encode");
        let decoded = Snapshot::from_canonical_bytes(&encoded).expect("heap-backed snapshot decode");
        prop_assert_eq!(&decoded, &snapshot);
        prop_assert_eq!(
            decoded.to_canonical_bytes().expect("re-encode"),
            encoded
        );

        let mut restored = State::from_snapshot(decoded);
        let read = "finish(base);\n";
        let original = run_execute(read, &mut state, &host);
        let continued = run_execute(read, &mut restored, &host);
        prop_assert_eq!(original.is_ok(), continued.is_ok());
        if let (Ok(original), Ok(continued)) = (original, continued) {
            prop_assert_eq!(finished(original), finished(continued));
        }
        prop_assert_eq!(restored.snapshot(), state.snapshot());
    }

    #[test]
    fn snapshot_round_trip_preserves_state(
        globals in globals_strategy()
    ) {
        let state = State::from_snapshot(Snapshot::new(
            globals
                .iter()
                .map(|(key, value)| (key.clone(), value.to_value()))
                .collect(),
        ));

        let encoded = state.snapshot().to_canonical_bytes().expect("snapshot encode");
        let decoded = Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");
        let restored = State::from_snapshot(decoded);

        prop_assert_eq!(restored.globals(), state.globals());
    }

    #[test]
    fn canonical_snapshot_round_trip_covers_every_value_variant(
        values in canonical_snapshot_variant_corpus_strategy()
    ) {
        let globals: Record = values
            .iter()
            .enumerate()
            .map(|(index, value)| (format!("variant_{index}"), value.clone()))
            .collect();
        let snapshot = Snapshot::new(globals);

        let encoded = snapshot.to_canonical_bytes().expect("canonical snapshot encode");
        let decoded = Snapshot::from_canonical_bytes(&encoded)
            .expect("canonical snapshot decode");
        let reencoded = decoded
            .to_canonical_bytes()
            .expect("canonical snapshot re-encode");

        prop_assert_eq!(&reencoded, &encoded);
        for (index, expected) in values.iter().enumerate() {
            let actual = decoded
                .globals()
                .get(&format!("variant_{index}"))
                .expect("round-tripped global");
            assert_canonical_value_round_trip(expected, actual);
        }
    }

    #[test]
    fn execution_from_restored_snapshot_matches_fresh_state(
        globals in globals_strategy(),
        value in gen_value_strategy()
    ) {
        let base_globals: Record = globals
            .iter()
            .map(|(key, value)| (key.clone(), value.to_value()))
            .collect();
        let source = format!(
            "const roundtrip_result = {};\nfinish(roundtrip_result);\n",
            value.to_source()
        );
        let host = DeterministicHost;

        let mut fresh = State::from_snapshot(Snapshot::new(base_globals.clone()));
        let mut restored = State::from_snapshot(Snapshot::new(base_globals));
        let blob = restored
            .snapshot()
            .to_canonical_bytes()
            .expect("snapshot encode");
        let snapshot = Snapshot::from_canonical_bytes(&blob).expect("snapshot decode");
        restored = State::from_snapshot(snapshot);

        let fresh_value = finished(run_execute(&source, &mut fresh, &host).expect("fresh execution"));
        let restored_value = finished(
            run_execute(&source, &mut restored, &host).expect("restored execution")
        );

        prop_assert_eq!(fresh_value, restored_value);
        prop_assert_eq!(fresh.globals(), restored.globals());
    }

    #[test]
    fn tool_result_contract_is_stable_for_generated_values(
        value in gen_value_strategy()
    ) {
        // A TypeScript tool call yields the host's value directly and throws on
        // failure, so the contract pinned here is that the value survives the
        // round trip through the host boundary unchanged.
        let source = format!(
            "const tool_result = await tools.echo({{ value: {} }});\nfinish(tool_result);\n",
            value.to_source()
        );
        let host = DeterministicHost;
        let mut state = State::new();

        let result = finished(run_execute(&source, &mut state, &host).expect("tool call should succeed"));

        prop_assert_eq!(result, value.to_value());
    }

    #[test]
    fn ternary_selects_generated_branch_without_evaluating_the_other_side(
        condition in any::<bool>(),
        yes in gen_value_strategy(),
        no in gen_value_strategy()
    ) {
        let expected = if condition { yes.to_value() } else { no.to_value() };
        let source = format!(
            "const ternary_result = {} ? {} : {};\nfinish(ternary_result);\n",
            if condition { "true" } else { "false" },
            yes.to_source(),
            no.to_source()
        );
        let host = DeterministicHost;
        let mut state = State::new();

        let actual = finished(run_execute(&source, &mut state, &host).expect("ternary execution"));

        prop_assert_eq!(actual, expected);
    }

    #[test]
    fn generated_type_literal_always_produces_valid_json_schema(
        ty in gen_type_strategy(6)
    ) {
        // Type literals have no TypeScript spelling (ADR 0096 keeps the type
        // language in the IR), so the program is built from the public AST.
        let program = lashlang::Program::block(vec![lashlang::Expr::Finish(Box::new(
            lashlang::Expr::TypeLiteral(Box::new(ty.to_type_expr())),
        ))]);
        let host = DeterministicHost;
        let mut state = State::new();
        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(lashlang::execute(&program, &mut state, &host));
        let value = finished(outcome.expect("Type literal should execute"));
        let inner = lashlang::unwrap_type_value(&value).expect("wrapped type");
        let schema = inner.as_record().expect("schema record");
        // Every generated type is an object at the top level.
        prop_assert_eq!(&schema["type"], &Value::String("object".into()));
        // `required` always exists as a list (possibly empty if everything is optional).
        prop_assert!(matches!(&schema["required"], Value::List(_)));
        prop_assert_eq!(&schema["additionalProperties"], &Value::Bool(false));
    }
}

// ------------------------------------------------------------------
//  Generator for arbitrary Type literals. Used only by property tests.
// ------------------------------------------------------------------

#[derive(Clone, Debug)]
enum GenType {
    Scalar(&'static str),
    Enum(Vec<String>),
    List(Box<GenType>),
    Object(Vec<(String, GenType, bool)>),
}

impl GenType {
    /// Lowers the generated shape into the IR's own type language.
    ///
    /// Only `Object` is a valid top-level type literal; the other variants
    /// appear as field types, so the caller wraps them in a field.
    fn to_type_expr(&self) -> TypeExpr {
        match self {
            Self::Scalar("str") => TypeExpr::Str,
            Self::Scalar("int") => TypeExpr::Int,
            Self::Scalar("float") => TypeExpr::Float,
            Self::Scalar("bool") => TypeExpr::Bool,
            Self::Scalar("dict") => TypeExpr::Dict,
            Self::Scalar("any") => TypeExpr::Any,
            Self::Scalar(other) => panic!("unexpected generated scalar: {other}"),
            Self::Enum(values) => {
                TypeExpr::Enum(values.iter().map(|value| value.as_str().into()).collect())
            }
            Self::List(inner) => TypeExpr::List(Box::new(inner.to_type_expr())),
            Self::Object(fields) => TypeExpr::Object(
                fields
                    .iter()
                    .map(|(name, ty, optional)| TypeField {
                        name: name.as_str().into(),
                        ty: ty.to_type_expr(),
                        optional: *optional,
                    })
                    .collect(),
            ),
        }
    }
}

fn gen_field_name() -> impl Strategy<Value = String> {
    // Field names live in the IR's type language, not in any dialect's
    // namespace, so no keyword filtering is needed here.
    "[a-z][a-z0-9_]{0,6}"
}

fn gen_enum_value() -> impl Strategy<Value = String> {
    "[a-z]{1,5}".prop_map(|s| s)
}

fn gen_scalar_name() -> impl Strategy<Value = GenType> {
    prop_oneof![
        Just(GenType::Scalar("str")),
        Just(GenType::Scalar("int")),
        Just(GenType::Scalar("float")),
        Just(GenType::Scalar("bool")),
        Just(GenType::Scalar("dict")),
        Just(GenType::Scalar("any")),
    ]
}

fn gen_type_strategy(max_depth: u32) -> impl Strategy<Value = GenType> {
    gen_type_expr(max_depth).prop_flat_map(|inner| {
        // Wrap in an Object if the inner isn't already one — the top-level
        // Type literal must always be an Object in our grammar.
        match inner {
            GenType::Object(fields) => Just(GenType::Object(fields)).boxed(),
            other => (gen_field_name(), Just(other))
                .prop_map(|(name, ty)| GenType::Object(vec![(name, ty, false)]))
                .boxed(),
        }
    })
}

fn gen_type_expr(_max_depth: u32) -> BoxedStrategy<GenType> {
    let leaf = prop_oneof![
        gen_scalar_name(),
        prop::collection::vec(gen_enum_value(), 1..4).prop_map(|values| {
            // Deduplicate to keep JSON-Schema enums valid.
            let mut seen = std::collections::HashSet::new();
            let unique: Vec<String> = values
                .into_iter()
                .filter(|v| seen.insert(v.clone()))
                .collect();
            GenType::Enum(unique)
        }),
    ];
    leaf.prop_recursive(3, 32, 4, |inner| {
        prop_oneof![
            inner.clone().prop_map(|ty| GenType::List(Box::new(ty))),
            prop::collection::vec((gen_field_name(), inner, any::<bool>()), 1..4).prop_map(
                |fields| {
                    let mut seen = std::collections::HashSet::new();
                    let unique: Vec<(String, GenType, bool)> = fields
                        .into_iter()
                        .filter(|(name, _, _)| seen.insert(name.clone()))
                        .collect();
                    GenType::Object(unique)
                }
            ),
        ]
    })
    .boxed()
}
