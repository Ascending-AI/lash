use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, ImageValue,
    ProjectedHostDescriptor, ProjectedValue, Record, ResourceHandle, Snapshot, State, Value,
    canonical_program_ir, canonical_program_source, parse,
};
use proptest::prelude::*;

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

fn run_execute(
    source: &str,
    state: &mut State,
    host: &DeterministicHost,
) -> Result<ExecutionOutcome, ExecuteError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(execute(source, state, host))
}

fn finished(outcome: ExecutionOutcome) -> Value {
    match outcome {
        ExecutionOutcome::Finished(value) => value,
        ExecutionOutcome::Continued => panic!("expected `finish`"),
        ExecutionOutcome::Failed(value) => panic!("unexpected process failure: {value}"),
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
enum ExecuteError {
    #[error(transparent)]
    Parse(#[from] lashlang::ParseError),
    #[error(transparent)]
    Link(#[from] lashlang::LinkError),
    #[error(transparent)]
    Runtime(#[from] lashlang::RuntimeError),
}

async fn execute<H: ExecutionHost>(
    source: &str,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, ExecuteError> {
    let compiled = if source.contains("tools.") {
        let program = parse(source)?;
        let linked = lashlang::LinkedModule::link(program, property_host_environment())?;
        lashlang::compile_linked(&linked)
    } else {
        lashlang::compile(source)?
    };
    lashlang::execute(&compiled, state, host)
        .await
        .map_err(ExecuteError::Runtime)
}

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

fn lashlang_string_strategy() -> impl Strategy<Value = String> {
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
    "[a-z_][a-z0-9_]{0,10}".prop_filter("reserved lashlang keyword", |ident| {
        !matches!(
            ident.as_str(),
            "if" | "else"
                | "for"
                | "in"
                | "finish"
                | "submit"
                | "print"
                | "call"
                | "true"
                | "false"
                | "null"
                | "and"
                | "or"
                | "not"
        )
    })
}

fn gen_value_strategy() -> impl Strategy<Value = GenValue> {
    let leaf = prop_oneof![
        Just(GenValue::Null),
        any::<bool>().prop_map(GenValue::Bool),
        (-10_000i32..=10_000i32).prop_map(GenValue::Number),
        lashlang_string_strategy().prop_map(GenValue::String),
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

fn generated_workflow_corpus(ident: &str, value: &str) -> Vec<(&'static str, String)> {
    let ident = format!("generated_{ident}");
    vec![
        ("data", format!("{ident} = {value}\nfinish {ident}\n")),
        (
            "call",
            format!(
                "@label(title: \"Generated call\", description: \"Metadata survives\")\n{ident} = await tools.echo({{ value: {value} }})?\nfinish {ident}\n"
            ),
        ),
        (
            "effect",
            format!(
                "@label(title: \"Generated effect\", description: \"Metadata survives\")\nprint {value}\nfinish {value}\n"
            ),
        ),
        (
            "terminal",
            format!("process {ident}_terminal() {{\n  fail {value}\n}}\n"),
        ),
        (
            "if_forms",
            format!(
                "{ident} = (true ? {value} : null)\nif true {{\n  print {ident}\n}} else if false {{\n  print null\n}} else {{\n  print {value}\n}}\nfinish {ident}\n"
            ),
        ),
        (
            "for",
            format!("for item in [1, 2] {{\n  print item\n}}\nfinish {value}\n"),
        ),
        (
            "comprehension",
            format!("{ident} = [item for item in [1, 2, 3] if item > 1]\nfinish {ident}\n"),
        ),
        (
            "process",
            format!(
                "@label(title: \"Generated process\", description: \"Metadata survives\")\nprocess {ident}() {{\n  wake {value}\n  finish {value}\n}}\n"
            ),
        ),
        (
            "while",
            "count = 0\nwhile count < 2 { count = count + 1 }\nfinish count\n".to_string(),
        ),
        (
            "stateful_for",
            "total = 0\nfor item in [1, 2] { total = total + item\nintroduced = item }\nfinish [total, introduced]\n".to_string(),
        ),
        (
            "state_update_path",
            "state = { count: 0 }\nstate.count = 1\nfinish state\n".to_string(),
        ),
        (
            "state_update_simple",
            format!("{ident} = null\n{ident} = {value}\nfinish {ident}\n"),
        ),
        (
            "type_ref_data",
            "type Generated = { value: str }\nschema = Type { nested: Generated }\nfinish schema\n"
                .to_string(),
        ),
        (
            "computation",
            "process generated_worker() { finish 1 }\nruns = [start generated_worker(), start generated_worker()]\ntupled = (await runs[0], await runs[1])\nlisted = [await runs[0], await runs[1]]\nrecorded = { value: await runs[0] }\nbuilt = len(await runs)\nbinary = (await runs[0] + 1)\nunary = not await runs[0]\nfield = (await runs[0]).value\nindexed = (await runs)[0]\nunusual = (await runs[0])??\nfinish unusual\n".to_string(),
        ),
        (
            "scoped_for",
            "item = 99\nitems = [1, 2]\nfor item in items { print item }\nselected = [item for item in items if item > 0]\nfinish item\n"
                .to_string(),
        ),
    ]
}

fn graph_has_required_variant(graph: &lashlang::WorkflowGraph, variant: &str) -> bool {
    match variant {
        "data" => graph
            .nodes()
            .any(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Data { .. })),
        "call" => graph
            .nodes()
            .any(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Call { .. })),
        "effect" => graph
            .nodes()
            .any(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Effect { .. })),
        "terminal" => graph
            .nodes()
            .any(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Terminal { .. })),
        "if_forms" => {
            let mut has_expression_if = false;
            let mut has_direct_else_if = false;
            for node in graph.nodes() {
                if let lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::If {
                    then_is_block,
                    else_is_block,
                    ..
                }) = node.kind
                {
                    has_expression_if |= !then_is_block && !else_is_block;
                    has_direct_else_if |= then_is_block && !else_is_block;
                }
            }
            has_expression_if && has_direct_else_if
        }
        "for" | "stateful_for" | "scoped_for" => graph.nodes().any(|node| {
            matches!(
                node.kind,
                lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::For { .. })
            )
        }),
        "while" => graph.nodes().any(|node| {
            matches!(
                node.kind,
                lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::While { .. })
            )
        }),
        "comprehension" => graph.nodes().any(|node| {
            matches!(
                node.kind,
                lashlang::WorkflowNodeKind::Container(
                    lashlang::WorkflowContainer::ListComprehension { .. }
                )
            )
        }),
        "process" => graph
            .declarations
            .iter()
            .any(|declaration| matches!(declaration, lashlang::WorkflowDeclaration::Process(_))),
        "state_update_path" | "state_update_simple" => graph
            .nodes()
            .any(|node| matches!(node.kind, lashlang::WorkflowNodeKind::StateUpdate { .. })),
        "type_ref_data" => graph.nodes().any(|node| {
            matches!(
                &node.kind,
                lashlang::WorkflowNodeKind::Data {
                    expression,
                    ..
                } if expression.starts_with("Type {")
            )
        }),
        "computation" => graph
            .nodes()
            .any(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Computation { .. })),
        _ => false,
    }
}

fn promoted_invalid_graph_is_typed(graph: &lashlang::WorkflowGraph, variant: &str) -> bool {
    let mut invalid = graph.clone();
    #[allow(
        clippy::needless_late_init,
        reason = "the mutation-heavy match arms stay substantially clearer without expression nesting"
    )]
    let expected;
    match variant {
        "data" => {
            let Some(node) = invalid
                .main
                .nodes
                .iter_mut()
                .find(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Data { .. }))
            else {
                return false;
            };
            let lashlang::WorkflowNodeKind::Data { expression, .. } = &mut node.kind else {
                return false;
            };
            *expression = "{".to_string();
            expected = "invalid_expression";
        }
        "call" => {
            let Some(node) = invalid
                .main
                .nodes
                .iter_mut()
                .find(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Call { .. }))
            else {
                return false;
            };
            let lashlang::WorkflowNodeKind::Call { binding, .. } = &mut node.kind else {
                return false;
            };
            *binding = Some("1 + 2".to_string());
            expected = "invalid_assignment_target";
        }
        "effect" => {
            let Some(node) = invalid
                .main
                .nodes
                .iter_mut()
                .find(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Effect { .. }))
            else {
                return false;
            };
            let lashlang::WorkflowNodeKind::Effect { expression, .. } = &mut node.kind else {
                return false;
            };
            *expression = "{".to_string();
            expected = "invalid_expression";
        }
        "terminal" => {
            let Some(process) =
                invalid
                    .declarations
                    .iter_mut()
                    .find_map(|declaration| match declaration {
                        lashlang::WorkflowDeclaration::Process(process) => Some(process),
                        lashlang::WorkflowDeclaration::Type(_)
                        | lashlang::WorkflowDeclaration::Function(_) => None,
                    })
            else {
                return false;
            };
            let Some(node) = process
                .body
                .nodes
                .iter_mut()
                .find(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Terminal { .. }))
            else {
                return false;
            };
            let lashlang::WorkflowNodeKind::Terminal { expression, .. } = &mut node.kind else {
                return false;
            };
            *expression = "{".to_string();
            expected = "invalid_expression";
        }
        "if_forms" => {
            let Some(node) = invalid.main.nodes.iter_mut().find(|node| {
                matches!(
                    node.kind,
                    lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::If {
                        then_is_block: false,
                        ..
                    })
                )
            }) else {
                return false;
            };
            let lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::If {
                else_is_block,
                ..
            }) = &mut node.kind
            else {
                return false;
            };
            *else_is_block = true;
            expected = "invalid_payload";
        }
        "for" | "stateful_for" | "scoped_for" => {
            let Some(node) = invalid.nodes().find(|node| {
                matches!(
                    node.kind,
                    lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::For { .. })
                )
            }) else {
                return false;
            };
            let id = node.id.clone();
            let Some(node) = invalid.main.nodes.iter_mut().find(|node| node.id == id) else {
                return false;
            };
            let lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::For {
                body,
                ..
            }) = &mut node.kind
            else {
                return false;
            };
            *body = None;
            expected = "missing_child";
        }
        "while" => {
            let Some(node) = invalid.main.nodes.iter_mut().find(|node| {
                matches!(
                    node.kind,
                    lashlang::WorkflowNodeKind::Container(
                        lashlang::WorkflowContainer::While { .. }
                    )
                )
            }) else {
                return false;
            };
            let lashlang::WorkflowNodeKind::Container(lashlang::WorkflowContainer::While {
                body,
                ..
            }) = &mut node.kind
            else {
                return false;
            };
            *body = None;
            expected = "missing_child";
        }
        "comprehension" => {
            let Some(node) = invalid.main.nodes.iter_mut().find(|node| {
                matches!(
                    node.kind,
                    lashlang::WorkflowNodeKind::Container(
                        lashlang::WorkflowContainer::ListComprehension { .. }
                    )
                )
            }) else {
                return false;
            };
            let lashlang::WorkflowNodeKind::Container(
                lashlang::WorkflowContainer::ListComprehension { element, .. },
            ) = &mut node.kind
            else {
                return false;
            };
            *element = None;
            expected = "missing_child";
        }
        "state_update_path" | "state_update_simple" => {
            let Some(node) =
                invalid.main.nodes.iter_mut().find(|node| {
                    matches!(node.kind, lashlang::WorkflowNodeKind::StateUpdate { .. })
                })
            else {
                return false;
            };
            let lashlang::WorkflowNodeKind::StateUpdate { expression, .. } = &mut node.kind else {
                return false;
            };
            *expression = "{".to_string();
            expected = "invalid_expression";
        }
        "type_ref_data" => {
            let Some(node) = invalid.main.nodes.iter_mut().find(|node| {
                matches!(
                    &node.kind,
                    lashlang::WorkflowNodeKind::Data {
                        expression,
                        ..
                    } if expression.starts_with("Type {")
                )
            }) else {
                return false;
            };
            let lashlang::WorkflowNodeKind::Data { expression, .. } = &mut node.kind else {
                return false;
            };
            *expression = "print null".to_string();
            expected = "invalid_payload";
        }
        "computation" => {
            let Some(node) =
                invalid.main.nodes.iter_mut().find(|node| {
                    matches!(node.kind, lashlang::WorkflowNodeKind::Computation { .. })
                })
            else {
                return false;
            };
            let lashlang::WorkflowNodeKind::Computation { expression, .. } = &mut node.kind else {
                return false;
            };
            *expression = "{".to_string();
            expected = "invalid_expression";
        }
        "process" => {
            let Some(process) =
                invalid
                    .declarations
                    .iter_mut()
                    .find_map(|declaration| match declaration {
                        lashlang::WorkflowDeclaration::Process(process) => Some(process),
                        lashlang::WorkflowDeclaration::Type(_)
                        | lashlang::WorkflowDeclaration::Function(_) => None,
                    })
            else {
                return false;
            };
            let Some(node) = process
                .body
                .nodes
                .iter_mut()
                .find(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Effect { .. }))
            else {
                return false;
            };
            let lashlang::WorkflowNodeKind::Effect { expression, .. } = &mut node.kind else {
                return false;
            };
            *expression = "{".to_string();
            expected = "invalid_expression";
        }
        _ => return false,
    }

    matches!(
        (expected, lashlang::workflow_graph_to_source(&invalid)),
        (
            "invalid_payload",
            Err(lashlang::GraphRenderError::InvalidNodePayload { .. })
        ) | (
            "missing_child",
            Err(lashlang::GraphRenderError::MissingRequiredChild { .. })
        ) | (
            "invalid_expression",
            Err(lashlang::GraphRenderError::InvalidExpression { .. })
        ) | (
            "invalid_assignment_target",
            Err(lashlang::GraphRenderError::InvalidAssignmentTarget { .. })
        )
    )
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
    fn to_source(&self) -> String {
        match self {
            Self::Seed(n) => format!("base = [[{n}], [{}]]\n", n.wrapping_add(1)),
            Self::Alias => "alias = base\n".to_string(),
            Self::PushRow(n) => format!("base = push(base, [{n}])\n"),
            Self::ConcatRow(n) => format!("base = base + [[{n}]]\n"),
            Self::NestInRecord => "holder = { rows: base, tag: \"held\" }\n".to_string(),
            Self::MutateFirst(n) => format!("base[0] = [{n}]\n"),
            Self::Discard => "scratch = [[9], [8], [7]]\nscratch = 0\n".to_string(),
            Self::IterateCopy => {
                "copies = []\nfor row in base { copies = push(copies, row) }\n".to_string()
            }
        }
    }
}

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
        let result = catch_unwind(AssertUnwindSafe(|| parse(&source)));
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
        let source = format!("{ident} = {}\nfinish {ident}\n", value.to_source());
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

    #[test]
    fn generated_value_programs_round_trip_through_canonical_source(
        ident in ident_strategy(),
        value in gen_value_strategy()
    ) {
        let source = format!("{ident} = {}\nfinish {ident}\n", value.to_source());
        let program = parse(&source).expect("generated source should parse");
        let rendered = canonical_program_source(&program).expect("canonical source should render");
        let reparsed = parse(&rendered).expect("canonical source should parse");

        prop_assert_eq!(canonical_program_ir(reparsed), canonical_program_ir(program));
    }

    #[test]
    fn generated_workflows_obey_code_graph_code_laws(
        ident in ident_strategy(),
        value in gen_value_strategy(),
    ) {
        let value = value.to_source();
        for (variant, source) in generated_workflow_corpus(&ident, &value) {
            let input = parse(&source).expect("generated workflow should parse");
            let canonical = canonical_program_source(&input).expect("canonical workflow source");
            let graph = lashlang::workflow_graph_from_source(&canonical)
                .expect("canonical source should project");
            prop_assert!(
                graph_has_required_variant(&graph, variant),
                "{variant} did not project to its required workflow graph kind"
            );

            let rendered = lashlang::workflow_graph_to_source(&graph)
                .expect("projected graph should render");

            // GetPut on canonical source.
            prop_assert_eq!(&rendered, &canonical, "GetPut failed for {}", variant);
            // Canonical structural fixpoint (spans are ignored by Program::PartialEq).
            prop_assert_eq!(
                parse(&rendered).unwrap(),
                parse(&canonical).unwrap(),
                "canonical AST fixpoint failed for {}",
                variant
            );
            // PutGet for graphs produced by source projection.
            prop_assert_eq!(
                lashlang::workflow_graph_from_source(&rendered).unwrap(),
                graph.clone(),
                "PutGet failed for {}",
                variant
            );

            // Every invalid document is rejected through the typed render surface.
            let mut unsupported = graph.clone();
            unsupported.schema_version = lashlang::WORKFLOW_GRAPH_SCHEMA_VERSION + 1;
            let unsupported_is_typed = matches!(
                lashlang::workflow_graph_to_source(&unsupported),
                Err(lashlang::GraphRenderError::UnsupportedSchemaVersion { .. })
            );
            prop_assert!(unsupported_is_typed);

            prop_assert!(
                !graph
                    .nodes()
                    .any(|node| matches!(node.kind, lashlang::WorkflowNodeKind::Opaque { .. })),
                "promoted construct unexpectedly projected as opaque for {variant}"
            );
            prop_assert!(
                promoted_invalid_graph_is_typed(&graph, variant),
                "invalid promoted graph did not return its typed error for {variant}"
            );

            if source.contains("@label") {
                let has_label = graph.nodes().any(|node| {
                    node.name_source == lashlang::WorkflowNodeNameSource::Label
                        && node.description.as_deref() == Some("Metadata survives")
                }) || graph.declarations.iter().any(|declaration| {
                    matches!(
                        declaration,
                        lashlang::WorkflowDeclaration::Process(process)
                            if process.name_source == lashlang::WorkflowNodeNameSource::Label
                                && process.description.as_deref() == Some("Metadata survives")
                    )
                });
                prop_assert!(has_label, "@label metadata was lost for {variant}");
            }
        }
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
        let source = steps.iter().map(HeapStep::to_source).collect::<String>() + "finish 0\n";
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
        let read = "finish base\n";
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
        let source = format!("result = {}\nfinish result\n", value.to_source());
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
        let source = format!(
            "r = await tools.echo({{ value: {} }})\nfinish r\n",
            value.to_source()
        );
        let host = DeterministicHost;
        let mut state = State::new();

        let result = finished(run_execute(&source, &mut state, &host).expect("tool call should succeed"));
        let record = result.as_record().expect("tool result should be a record");

        prop_assert_eq!(record.get("ok"), Some(&Value::Bool(true)));
        prop_assert_eq!(record.get("value"), Some(&value.to_value()));
    }

    #[test]
    fn ternary_selects_generated_branch_without_evaluating_the_other_side(
        condition in any::<bool>(),
        yes in gen_value_strategy(),
        no in gen_value_strategy()
    ) {
        let expected = if condition { yes.to_value() } else { no.to_value() };
        let source = format!(
            "result = {} ? {} : {}\nfinish result\n",
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
        let source = format!("x = {}\nfinish x\n", ty.to_source());
        let host = DeterministicHost;
        let mut state = State::new();
        let outcome = run_execute(&source, &mut state, &host);
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
    fn to_source(&self) -> String {
        // Only Object is a valid top-level Type literal; others appear as
        // field types. Caller must wrap scalars/lists/enums in a field.
        match self {
            Self::Scalar(name) => (*name).to_string(),
            Self::Enum(values) => {
                let rendered: Vec<String> = values.iter().map(|v| encode_string(v)).collect();
                format!("enum[{}]", rendered.join(", "))
            }
            Self::List(inner) => format!("list[{}]", inner.to_source()),
            Self::Object(fields) => {
                let rendered: Vec<String> = fields
                    .iter()
                    .map(|(name, ty, optional)| {
                        let opt = if *optional { "?" } else { "" };
                        format!("{name}: {}{opt}", ty.to_source())
                    })
                    .collect();
                format!("Type {{ {} }}", rendered.join(", "))
            }
        }
    }
}

fn gen_field_name() -> impl Strategy<Value = String> {
    // Lowercase ASCII identifiers to avoid collision with keywords.
    "[a-z][a-z0-9_]{0,6}".prop_map(|s| {
        // Avoid reserved identifiers that could shadow keywords.
        const RESERVED: &[&str] = &[
            "if", "else", "for", "in", "start", "await", "cancel", "finish", "submit", "print",
            "call", "and", "or", "not", "true", "false", "null",
        ];
        if RESERVED.iter().any(|r| *r == s) {
            format!("{s}_")
        } else {
            s
        }
    })
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
