//! Property tests over the IR, the VM and the snapshot encoding.
//!
//! ADR 0096 makes TypeScript the sole authored RLM dialect, so every generated
//! program below is TypeScript lowered through `lash_typescript`, and the
//! generators emit TypeScript rather than the retired surface.
//!
//! The code→graph→code laws moved out with the workflow-graph lens: rendering
//! and re-reading a program's canonical source is the lens's own contract, and
//! the lens is a TypeScript-facing surface under FIG-3033. Its round-trip law
//! is pinned there against the TypeScript printer rather than here against a
//! retired one.

use std::sync::Arc;

use lash_vm::{
    AbilityOp, AbilityOutcome, ExecutionHost, ExecutionHostError, ExecutionOutcome, ImageValue,
    ProjectedValue, ProjectionType, Record, ResourceHandle, ResourceRef, Snapshot, State, Value,
};
use proptest::prelude::*;

#[path = "property/edits.rs"]
mod edits;
#[path = "support/execute.rs"]
mod execute_support;
#[path = "property/ir_gen.rs"]
mod ir_gen;

use execute_support::{ExecuteError, execute};
use lash_vm::testing::differential;
use proptest::test_runner::TestCaseError;

#[derive(Default)]
struct DeterministicHost;

impl ExecutionHost for DeterministicHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => match operation.operation.as_str() {
                "echo" => Ok(AbilityOutcome::Value(
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
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityOutcome::Value(value)),
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
fn property_host_environment() -> lash_vm::LashVmHostEnvironment {
    let mut resources = lash_vm::LashVmHostCatalog::new();
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "echo",
            "echo",
            lash_vm::TypeExpr::Any,
            lash_vm::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "fail",
            "fail",
            lash_vm::TypeExpr::Any,
            lash_vm::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    lash_vm::LashVmHostEnvironment::new(resources)
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
                    .collect::<lash_vm::Record>()
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
                let projected = Value::Projected(ProjectedValue::resource(
                    "session.items[3]",
                    "snapshot_property",
                    ResourceRef {
                        projection: ProjectionType::new("snapshot_property"),
                        id: projection_text.clone(),
                        revision: (!projection_text.is_empty()).then_some(projection_text),
                    },
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
                        lash_vm::MediaType::parse("image/png").expect("valid media type"),
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
            assert_eq!(actual.resource_ref(), expected.resource_ref());
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
        let decoded = lash_vm::VmInstance::pristine().open_snapshot(&encoded).expect("heap-backed snapshot decode");
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
        let decoded = lash_vm::VmInstance::pristine().open_snapshot(&encoded)
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

}

/// A tape of choices from a seed: the fixed programs the coverage laws read.
fn seeded_tape(seed: u64) -> Vec<u16> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x5578;
    (0..96)
        .map(|_| {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut word = state;
            word = (word ^ (word >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            word = (word ^ (word >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (word ^ (word >> 31)) as u16
        })
        .collect()
}

/// How many seeded programs the coverage laws read.
const SEEDED_PROGRAMS: u64 = 256;

/// The generator's own contract: every program it builds is valid IR that
/// the linker admits against the host environment, and together the
/// programs and the artifacts they admit to hold every variant of the IR.
/// The variant names come from the exhaustive match beside the slot walk, so
/// a variant added to the IR fails this law until the generator builds it.
#[test]
fn generated_programs_admit_and_hold_every_ir_variant() {
    let environment = ir_gen::environment();
    let mut seen = std::collections::BTreeSet::new();
    let mut refused = std::collections::BTreeMap::<String, (usize, u64)>::new();
    for seed in 0..SEEDED_PROGRAMS {
        let program = ir_gen::program(&seeded_tape(seed));
        lash_vm::validate_ast(&program)
            .unwrap_or_else(|error| panic!("seed {seed} is valid IR: {error}\n{program:#?}"));
        seen.extend(lash_vm::testing::ir_variants::variants_in(&program));
        match lash_vm::LinkedModule::link(program, &environment) {
            Ok(linked) => {
                seen.extend(lash_vm::testing::ir_variants::variants_in(
                    linked.artifact.ir(),
                ));
            }
            Err(error) => {
                let entry = refused.entry(error.to_string()).or_insert((0, seed));
                entry.0 += 1;
            }
        }
    }
    assert!(
        refused.is_empty(),
        "generated programs the linker refuses (refusal: count, first seed): {refused:#?}"
    );
    let all = lash_vm::testing::ir_variants::EXPR_VARIANT_NAMES
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        seen,
        all,
        "the generated programs miss {:?}",
        all.difference(&seen).collect::<Vec<_>>()
    );
}

#[expect(
    clippy::expect_used,
    reason = "law driver builds the fixed-configuration tokio runtime, per the message"
)]
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(future)
}

fn through_wire(graph: &lash_vm::WorkflowGraph) -> Result<lash_vm::WorkflowGraph, TestCaseError> {
    differential::through_wire(graph).map_err(TestCaseError::fail)
}

fn fail(error: &dyn std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(error.to_string())
}

fn link(program: lash_vm::Program) -> Result<lash_vm::LinkedModule, TestCaseError> {
    lash_vm::LinkedModule::link(program, ir_gen::environment()).map_err(|error| fail(&error))
}

/// The round-trip law: a program projected to its document and
/// reconstructed is the same program, and admits to the same definition,
/// with no dialect involved. It holds for the draft a host authors (process
/// literals still inline) and for the artifact the linker admits (processes
/// lifted, calls resolved).
fn document_round_trips(program: lash_vm::Program) -> Result<(), TestCaseError> {
    let document = lash_vm::workflow_graph_from_program(&program);
    let held = through_wire(&document)?;
    prop_assert_eq!(&held, &document, "the draft document survives its encoding");
    let rebuilt = lash_vm::workflow_program_from_graph(&held).map_err(|error| fail(&error))?;
    prop_assert_eq!(&rebuilt, &program, "the draft document spells its program");
    let linked = link(program)?;
    let relinked = link(rebuilt)?;
    prop_assert_eq!(
        relinked.artifact.source_identity(),
        linked.artifact.source_identity()
    );
    prop_assert_eq!(relinked.artifact.module_ref(), linked.artifact.module_ref());

    let admitted = lash_vm::workflow_graph_from_artifact(&linked.artifact);
    prop_assert_eq!(
        admitted.source_identity.clone(),
        Some(linked.artifact.source_identity()),
        "an admitted document names its definition"
    );
    let rebuilt = lash_vm::workflow_program_from_graph(&through_wire(&admitted)?)
        .map_err(|error| fail(&error))?;
    prop_assert_eq!(
        &rebuilt,
        linked.artifact.ir(),
        "the admitted document spells the admitted program"
    );
    let complete = lash_vm::ModuleArtifact::from_program(rebuilt).map_err(|error| fail(&error))?;
    prop_assert_eq!(
        complete.source_identity(),
        linked.artifact.source_identity(),
        "the admitted document admits to the same definition"
    );
    prop_assert_eq!(complete.exports(), linked.artifact.exports());
    let readmitted =
        differential::readmitted_from_document(&linked.artifact, &ir_gen::environment())
            .map_err(TestCaseError::fail)?;
    prop_assert_eq!(
        &readmitted.linked.artifact,
        &linked.artifact,
        "admitting an admitted document again changes nothing"
    );
    prop_assert_eq!(&readmitted.graph, &admitted);
    Ok(())
}

/// The differential law over one generated program
/// ([`differential::document_runs_like_its_source`]), and the site law over
/// its run.
fn document_runs_like_its_source(
    program: &lash_vm::Program,
) -> Result<differential::SourceRun, TestCaseError> {
    let run = block_on(differential::document_runs_like_its_source(
        program,
        &ir_gen::environment(),
    ))
    .map_err(TestCaseError::fail)?;
    differential::observed_sites_are_in_the_document(&run).map_err(TestCaseError::fail)?;
    Ok(run)
}

/// The seeded programs do run: most reach their finish, some stop on a
/// failure nothing handles, and they perform effects, start processes and
/// take branches. Without this the differential law could hold over
/// programs that all stop at their first statement.
#[test]
fn generated_programs_exercise_the_vm() {
    let mut finished = 0usize;
    let mut failed = 0usize;
    let mut stopped = 0usize;
    let mut effects = 0usize;
    let mut observations = 0usize;
    let mut processes = 0usize;
    let mut lifted = 0usize;
    for seed in 0..SEEDED_PROGRAMS {
        let differential::SourceRun { artifact, run } =
            document_runs_like_its_source(&ir_gen::program(&seeded_tape(seed)))
                .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
        lifted += usize::from(artifact.ir().declarations.iter().any(|declaration| {
            matches!(declaration, lash_vm::Declaration::Process(process) if process.origin.is_lifted())
        }));
        processes += run.entries.len() - 1;
        for entry in &run.entries {
            match &entry.outcome {
                Ok(ExecutionOutcome::Finished(_)) => finished += 1,
                Ok(ExecutionOutcome::Failed(_)) => failed += 1,
                Ok(ExecutionOutcome::Continued) | Err(_) => stopped += 1,
            }
            effects += entry.effects.len();
            observations += entry.observations.len();
        }
    }
    assert!(
        finished > 2 * (failed + stopped) && failed > 0 && stopped > 0,
        "finished {finished}, failed {failed}, stopped {stopped}"
    );
    assert!(effects > 1000 && observations > 2000 && processes > 100);
    assert!(
        lifted > 50 && lifted < SEEDED_PROGRAMS as usize,
        "programs with and without a lifted process are both generated: {lifted}"
    );
}

/// What admission said of the documents a script of edits left.
#[derive(Default)]
struct Published {
    admitted: usize,
    /// The kind of each diagnostic of each document it refused.
    refused: Vec<lash_vm::WorkflowAdmissionDiagnosticKind>,
}

/// The edit law ([`edits::fuzz`]) and what follows it: every document an
/// applied transaction leaves goes to admission, which admits it or refuses
/// it with located, typed diagnostics and never anything else, and the last
/// program it admits round-trips through its document and runs like its
/// source.
fn edited_documents_stay_programs(
    program_words: &[u16],
    edit_words: &[u16],
) -> Result<(edits::Fuzzed, Published), TestCaseError> {
    let fuzzed = edits::fuzz(&ir_gen::program(program_words), edit_words)?;
    let environment = ir_gen::environment();
    let mut published = Published::default();
    let mut last = None;
    for program in &fuzzed.programs {
        let document = lash_vm::workflow_graph_from_program(program);
        match lash_vm::admit_workflow_graph(&document, &environment) {
            Ok(_) => {
                published.admitted += 1;
                last = Some(program);
            }
            Err(refusal) => {
                prop_assert!(!refusal.diagnostics.is_empty(), "a refusal says why");
                published
                    .refused
                    .extend(refusal.diagnostics.iter().map(|diagnostic| diagnostic.kind));
            }
        }
    }
    if let Some(program) = last {
        document_round_trips(program.clone())?;
        document_runs_like_its_source(program)?;
    }
    Ok((fuzzed, published))
}

/// The seeded edit scripts reach every edit: each kind applies at least
/// once, refusals of every class occur, and admission both admits and
/// refuses what edits leave. The kind names come from an exhaustive match,
/// so an edit added to the draft fails this law until a script applies it.
#[test]
fn seeded_edit_scripts_apply_every_edit_kind() {
    let mut applied = std::collections::BTreeMap::<&str, usize>::new();
    let mut refused = std::collections::BTreeMap::<(&str, &str), usize>::new();
    let mut admitted = 0usize;
    let mut admission_refused = std::collections::BTreeMap::<String, usize>::new();
    for seed in 0..SEEDED_PROGRAMS {
        let (fuzzed, published) =
            edited_documents_stay_programs(&seeded_tape(seed), &seeded_tape(seed + 10_000))
                .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
        for kind in fuzzed.applied {
            *applied.entry(kind).or_default() += 1;
        }
        for refusal in fuzzed.refused {
            *refused.entry(refusal).or_default() += 1;
        }
        admitted += published.admitted;
        for kind in published.refused {
            *admission_refused.entry(format!("{kind:?}")).or_default() += 1;
        }
    }
    let kinds = lash_vm::testing::workflow_edits::WORKFLOW_EDIT_KINDS;
    assert_eq!(
        applied.keys().copied().collect::<Vec<_>>(),
        {
            let mut kinds = kinds.to_vec();
            kinds.sort_unstable();
            kinds
        },
        "every edit kind applies in some script: {applied:#?}"
    );
    let codes = refused
        .keys()
        .map(|(_, code)| *code)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        codes.into_iter().collect::<Vec<_>>(),
        DRAFT_REFUSAL_CODES,
        "every refusal the draft has is reached: {refused:#?}"
    );
    assert!(
        admitted > 500 && !admission_refused.is_empty(),
        "admission takes most of what edits leave and refuses some: {admitted} admitted, {admission_refused:#?}"
    );
}

/// The code of every refusal a draft has, in order.
const DRAFT_REFUSAL_CODES: [&str; 17] = [
    "anchor_outside_body",
    "binding_captured",
    "binding_name_taken",
    "derived_process",
    "edit_does_not_apply",
    "invalid_document",
    "invalid_program",
    "move_into_own_subtree",
    "slot_in_child_body",
    "stale_revision",
    "unknown_binding",
    "unknown_body",
    "unknown_function",
    "unknown_handle",
    "unknown_process",
    "unknown_slot",
    "unresolved_binding",
];

/// A program whose loops run a number of times its shape decides: `outer`
/// passes of a loop that runs an inner loop of `inner` elements, left early
/// at element `stop`, then `turns` passes of a counted loop that skips its
/// second. Each body calls the host with its own tag, and the inner one
/// makes the same call twice in one statement.
fn loop_nest(outer: usize, inner: usize, stop: Option<usize>, turns: usize) -> lash_vm::Program {
    use lash_vm::CoercingBinaryOp as Op;
    use lash_vm::testing::ast_builders as b;
    let numbers = |count: usize| b::list((0..count).map(|index| b::num(index as f64)).collect());
    let call = |tag: &str| {
        b::module_call(
            &["tools"],
            "echo",
            vec![b::record(vec![("value", b::string(tag))])],
        )
    };
    let when =
        |condition, control| b::if_else(condition, b::block(vec![control]), b::block(Vec::new()));
    let mut inner_body = Vec::new();
    if let Some(stop) = stop {
        inner_body.push(when(
            b::binary(b::var("b"), Op::StrictEqual, b::num(stop as f64)),
            lash_vm::Expr::Break,
        ));
    }
    inner_body.push(b::list(vec![call("inner"), call("inner")]));
    b::program(vec![
        b::for_in(
            "a",
            numbers(outer),
            b::block(vec![
                b::for_in("b", numbers(inner), b::block(inner_body)),
                call("outer"),
            ]),
        ),
        b::assign("k", b::num(0.0)),
        b::while_loop(
            b::binary(b::var("k"), Op::Less, b::num(turns as f64)),
            b::block(vec![
                b::assign("k", b::binary(b::var("k"), Op::Add, b::num(1.0))),
                when(
                    b::binary(b::var("k"), Op::StrictEqual, b::num(2.0)),
                    lash_vm::Expr::Continue,
                ),
                call("turn"),
            ]),
        ),
        b::finish(b::null()),
    ])
}

/// What [`loop_nest`] asks its host for, by a reference count of its loops:
/// each call's tag and the body iteration of every loop around it,
/// outermost first.
fn loop_nest_reference(
    outer: usize,
    inner: usize,
    stop: Option<usize>,
    turns: usize,
) -> Vec<(&'static str, Vec<u64>)> {
    let mut calls = Vec::new();
    for a in 1..=outer as u64 {
        for b in 1..=stop.map_or(inner, |stop| stop.min(inner)) as u64 {
            calls.push(("inner", vec![a, b]));
            calls.push(("inner", vec![a, b]));
        }
        calls.push(("outer", vec![a]));
    }
    for k in (1..=turns as u64).filter(|k| *k != 2) {
        calls.push(("turn", vec![k]));
    }
    calls
}

/// The loop law: the calls a program makes in its loops are attributed as a
/// reference count of those loops says.
///
/// * Each call names the body iteration of every loop around it, across
///   loop re-entry, `break` and `continue`.
/// * Two identical calls in one statement are two sites of one node, and no
///   two calls of the program share a site.
/// * A loop is one site. Entering it again is a new activation, and an
///   activation is never shared by two loops.
fn loop_occurrences_match_the_program(
    outer: usize,
    inner: usize,
    stop: Option<usize>,
    turns: usize,
) -> Result<(), TestCaseError> {
    use lash_vm::WorkflowLoopPosition as Position;
    let source = document_runs_like_its_source(&loop_nest(outer, inner, stop, turns))?;
    let main = &source.run.entries[0];
    prop_assert!(matches!(main.outcome, Ok(ExecutionOutcome::Finished(_))));
    let calls = main
        .calls
        .iter()
        .map(|call| {
            let value = call
                .argument
                .as_ref()
                .and_then(Value::as_record)
                .and_then(|record| record.get("value"))?;
            let tag = ["inner", "outer", "turn"]
                .into_iter()
                .find(|tag| *value == Value::String((*tag).into()))?;
            Some((tag, call.site.as_ref()?))
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| TestCaseError::fail("a call has no tag or no site"))?;
    let observed = calls
        .iter()
        .map(|(tag, site)| {
            let iterations = site
                .loops
                .iter()
                .map(|frame| match frame.position {
                    Position::Body(iteration) => Some(iteration),
                    Position::Check(_) => None,
                })
                .collect::<Option<Vec<_>>>();
            (*tag, iterations)
        })
        .collect::<Vec<_>>();
    let reference = loop_nest_reference(outer, inner, stop, turns)
        .into_iter()
        .map(|(tag, iterations)| (tag, Some(iterations)))
        .collect::<Vec<_>>();
    prop_assert_eq!(observed, reference);

    // Sites: one per call expression, two for the twin calls of one node.
    let mut sites = std::collections::BTreeMap::<_, std::collections::BTreeSet<_>>::new();
    for (tag, site) in &calls {
        sites.entry(*tag).or_default().insert(site.site.site_ref());
    }
    for (tag, count) in [("inner", 2), ("outer", 1), ("turn", 1)] {
        let Some(of_tag) = sites.get(tag) else {
            continue;
        };
        prop_assert_eq!(of_tag.len(), count, "`{}` has {} site(s)", tag, count);
        let nodes = of_tag
            .iter()
            .map(|site| &site.node_id)
            .collect::<std::collections::BTreeSet<_>>();
        prop_assert_eq!(nodes.len(), 1, "the calls of one statement are one node");
    }
    let distinct = sites
        .values()
        .flatten()
        .collect::<std::collections::BTreeSet<_>>();
    prop_assert_eq!(
        distinct.len(),
        sites.values().map(|of_tag| of_tag.len()).sum::<usize>()
    );

    // Loops: the frame at each depth is one loop, and each entry of a loop
    // is its own activation.
    let mut activations = std::collections::BTreeMap::new();
    let mut loops = std::collections::BTreeMap::<_, std::collections::BTreeSet<_>>::new();
    for (tag, site) in &calls {
        for (depth, frame) in site.loops.iter().enumerate() {
            loops
                .entry((*tag != "turn", depth))
                .or_default()
                .insert(frame.site.clone());
            let outer_iteration = match site.loops[0].position {
                Position::Body(iteration) if depth == 1 => iteration,
                _ => 0,
            };
            let entered = activations
                .entry(frame.activation)
                .or_insert((frame.site.clone(), outer_iteration));
            prop_assert_eq!(
                &*entered,
                &(frame.site.clone(), outer_iteration),
                "activation {} is one entry of one loop",
                frame.activation
            );
        }
    }
    prop_assert!(loops.values().all(|at_depth| at_depth.len() == 1));
    let distinct = loops
        .values()
        .flatten()
        .collect::<std::collections::BTreeSet<_>>();
    prop_assert_eq!(distinct.len(), loops.len(), "each loop is its own site");
    let entries = activations
        .values()
        .collect::<std::collections::BTreeSet<_>>();
    prop_assert_eq!(
        entries.len(),
        activations.len(),
        "one entry of a loop has one activation"
    );
    Ok(())
}

fn tape() -> impl Strategy<Value = Vec<u16>> {
    prop::collection::vec(any::<u16>(), 0..120)
}

proptest! {
    // Fixed seeds: the cases are the same on every run, and a failure is
    // shrunk to the smallest tape that still fails. A shrunk failure's seed
    // line goes in `proptest-regressions/property.txt`, whose cases run
    // before the seeded ones.
    #![proptest_config(ProptestConfig {
        cases: 256,
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x5578),
        .. ProptestConfig::default()
    })]

    #[test]
    fn a_generated_program_round_trips_through_its_document(words in tape()) {
        document_round_trips(ir_gen::program(&words))?;
    }

    #[test]
    fn a_document_published_program_runs_like_its_source(words in tape()) {
        document_runs_like_its_source(&ir_gen::program(&words))?;
    }

    #[test]
    fn loop_occurrences_count_what_the_program_runs(
        outer in 0usize..4,
        inner in 0usize..4,
        stop in prop::option::of(0usize..4),
        turns in 0usize..5,
    ) {
        loop_occurrences_match_the_program(outer, inner, stop, turns)?;
    }

    #[test]
    fn an_edit_script_leaves_a_program_or_refuses_whole(
        program in tape(),
        script in tape(),
    ) {
        edited_documents_stay_programs(&program, &script)?;
    }
}
