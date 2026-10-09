use super::*;
use crate::ast::{CoercingBinaryOp, CoercingUnaryOp};
use crate::testing::projection::{TestView, test_view, with_test_views};

/// The lash_vm spelling of the language-operation parity fixture, kept as the
/// label the divergence assertions quote.
const LANGUAGE_OPERATION_PARITY_SOURCE: &str = r#"
out = {
  exact_smoke: slice(input.context, 2, 7),
  field: input.record.a,
  index: input.items[input.start],
  len_context: len(input.context),
  empty_items: empty(input.items),
  keys_record: keys(input.record),
  values_record: values(input.record),
  contains_text: contains(input.context, "beta"),
  contains_list: contains(input.items, "green"),
  contains_record: contains(input.record, "a"),
  find_text: find(input.context, "beta"),
  grep_text: grep_text(input.context, "beta"),
  starts: starts_with(trim(input.context), "alpha"),
  ends: ends_with(trim(input.context), "gamma"),
  split: split(trim(input.context), ","),
  joined: join(input.items, "|"),
  trimmed: trim(input.context),
  list_slice: slice(input.items, 0, 2),
  pushed: push(input.items, "yellow"),
  as_int: to_int(input.n),
  as_float: to_float(input.n),
  parsed: json_parse(input.json),
  plus: input.record.a + 1,
  neg: -input.record.a,
  cmp: input.record.a < input.record.b,
  truthy: input.record.a ? "yes" : "no",
  formatted: format("ctx={}", input.context),
  text: to_string(input.record)
}
finish out
"#;

/// The lash_vm spelling of the range/validation/iteration parity fixture.
const RANGE_PARITY_SOURCE: &str = r#"
total = 0
for i in range(input.start, input.end) {
  total = total + i
}
finish {
  range_values: range(input.start, input.end),
  total: total,
  validated: validate(input.item, Type { name: str, version: str })
}
"#;

fn test_image() -> Value {
    Value::Image(Box::new(ImageValue::new(
        "img-1",
        crate::MediaType::parse("image/png").unwrap(),
        "chart.png",
        1234,
        Some(640),
        Some(480),
    )))
}

async fn exec_with_global(
    name: &str,
    value: Value,
    program: Program,
) -> Result<Value, RuntimeError> {
    let mut state = State::new();
    state.insert_global(name, value)?;
    match execute_program(&program, &mut state, &Host).await? {
        ExecutionOutcome::Finished(value) => Ok(value),
        ExecutionOutcome::Continued => panic!("expected `finish` in test program"),
        ExecutionOutcome::Failed(value) => panic!("unexpected process failure: {value}"),
    }
}

struct TestProjectedValue {
    values: Vec<Value>,
    get_count: AtomicUsize,
    materialize_count: AtomicUsize,
    render_count: AtomicUsize,
}

impl TestProjectedValue {
    fn new(values: Vec<Value>) -> Arc<Self> {
        Arc::new(Self {
            values,
            get_count: AtomicUsize::new(0),
            materialize_count: AtomicUsize::new(0),
            render_count: AtomicUsize::new(0),
        })
    }
}

struct SearchProjectedText {
    text: Arc<str>,
    slice_count: AtomicUsize,
    materialize_count: AtomicUsize,
    render_count: AtomicUsize,
    slices: Mutex<Vec<(Option<isize>, Option<isize>)>>,
}

impl SearchProjectedText {
    fn new(text: impl Into<Arc<str>>) -> Arc<Self> {
        Arc::new(Self {
            text: text.into(),
            slice_count: AtomicUsize::new(0),
            materialize_count: AtomicUsize::new(0),
            render_count: AtomicUsize::new(0),
            slices: Mutex::new(Vec::new()),
        })
    }

    fn slices(&self) -> Vec<(Option<isize>, Option<isize>)> {
        self.slices.lock_recover().clone()
    }
}

impl TestView for SearchProjectedText {
    fn type_name(&self) -> &str {
        "string"
    }

    fn read_one(&self, request: ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        match request {
            ProjectedReadRequest::Len => {
                Some(ProjectedReadResponse::Len(self.text.chars().count()))
            }
            ProjectedReadRequest::Slice { start, end } => {
                self.slice_count.fetch_add(1, Ordering::SeqCst);
                self.slices.lock_recover().push((start, end));
                Some(ProjectedReadResponse::Value(Value::String(
                    slice_string(&self.text, start, end).into(),
                )))
            }
            ProjectedReadRequest::Render => {
                self.render_count.fetch_add(1, Ordering::SeqCst);
                Some(ProjectedReadResponse::Text(self.text.to_string()))
            }
            ProjectedReadRequest::Materialize => {
                self.materialize_count.fetch_add(1, Ordering::SeqCst);
                Some(ProjectedReadResponse::Value(Value::String(
                    self.text.as_ref().into(),
                )))
            }
            _ => None,
        }
    }
}

impl TestView for TestProjectedValue {
    fn type_name(&self) -> &str {
        "list"
    }

    fn read_one(&self, request: ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        let ProjectedReadRequest::Index(index) = request else {
            return match request {
                ProjectedReadRequest::Len => Some(ProjectedReadResponse::Len(self.values.len())),
                ProjectedReadRequest::Render => {
                    self.render_count.fetch_add(1, Ordering::SeqCst);
                    Some(ProjectedReadResponse::Text("<projected list>".to_string()))
                }
                ProjectedReadRequest::Materialize => {
                    self.materialize_count.fetch_add(1, Ordering::SeqCst);
                    Some(ProjectedReadResponse::Value(Value::List(
                        self.values.clone().into(),
                    )))
                }
                _ => None,
            };
        };
        let Value::Number(index) = index else {
            return None;
        };
        if !index.is_finite() || index.fract() != 0.0 {
            return None;
        }
        let len = self.values.len() as isize;
        let index = index as isize;
        let index = if index < 0 { len + index } else { index };
        if index < 0 || index >= len {
            return None;
        }
        self.get_count.fetch_add(1, Ordering::SeqCst);
        self.values
            .get(index as usize)
            .cloned()
            .map(ProjectedReadResponse::Value)
    }
}

fn projected_list_bindings(name: &str, list: Arc<TestProjectedValue>) -> ProjectedBindings {
    let mut projected = ProjectedBindings::new();
    projected.insert(name, test_view(name, list));
    projected
}

struct ProjectedFixture {
    value: Value,
    materialize_count: AtomicUsize,
}

impl ProjectedFixture {
    fn new(value: Value) -> Arc<Self> {
        Arc::new(Self {
            value,
            materialize_count: AtomicUsize::new(0),
        })
    }
}

fn projected_response_from_value(
    value: &Value,
    request: ProjectedReadRequest,
) -> Option<ProjectedReadResponse> {
    match request {
        ProjectedReadRequest::Len => value_len(value).map(ProjectedReadResponse::Len),
        ProjectedReadRequest::Empty => {
            value_len(value).map(|len| ProjectedReadResponse::Bool(len == 0))
        }
        ProjectedReadRequest::Truthy => match is_truthy(value) {
            Ok(truthy) => Some(ProjectedReadResponse::Bool(truthy)),
            Err(_) => None,
        },
        ProjectedReadRequest::Field(field) => {
            let field = Name::new(field.as_ref());
            read_field_ref_direct(value, &field)
                .ok()
                .map(ProjectedReadResponse::Value)
        }
        ProjectedReadRequest::Index(index) => read_index_ref_direct(value, &index)
            .ok()
            .map(ProjectedReadResponse::Value),
        ProjectedReadRequest::Contains(needle) => execute_contains_direct(value, &needle)
            .ok()
            .map(ProjectedReadResponse::Bool),
        ProjectedReadRequest::Find { needle, start } => execute_find_direct(value, &needle, start)
            .ok()
            .map(ProjectedReadResponse::Value),
        ProjectedReadRequest::GrepText(needle) => execute_grep_text_direct(value, &needle)
            .ok()
            .map(ProjectedReadResponse::Value),
        ProjectedReadRequest::Keys => match value {
            Value::Record(record) => Some(ProjectedReadResponse::Keys(
                record.keys().map(ToString::to_string).collect(),
            )),
            _ => None,
        },
        ProjectedReadRequest::Values => match value {
            Value::Record(record) => Some(ProjectedReadResponse::Value(Value::List(
                record.values().cloned().collect::<Vec<_>>().into(),
            ))),
            Value::Null => Some(ProjectedReadResponse::Value(Value::List(Vec::new().into()))),
            _ => None,
        },
        ProjectedReadRequest::StartsWith(prefix) => {
            let Ok(value) = coerce_string(value) else {
                return None;
            };
            let Ok(prefix) = coerce_string(&prefix) else {
                return None;
            };
            Some(ProjectedReadResponse::Bool(
                value.starts_with(prefix.as_ref()),
            ))
        }
        ProjectedReadRequest::EndsWith(suffix) => {
            let Ok(value) = coerce_string(value) else {
                return None;
            };
            let Ok(suffix) = coerce_string(&suffix) else {
                return None;
            };
            Some(ProjectedReadResponse::Bool(
                value.ends_with(suffix.as_ref()),
            ))
        }
        ProjectedReadRequest::Split(needle) => {
            let Ok(value) = coerce_string(value) else {
                return None;
            };
            let Ok(needle) = coerce_string(&needle) else {
                return None;
            };
            Some(ProjectedReadResponse::Value(Value::List(
                value
                    .split(needle.as_ref())
                    .map(|part| Value::String(part.into()))
                    .collect::<Vec<_>>()
                    .into(),
            )))
        }
        ProjectedReadRequest::Join(sep) => execute_join_builtin(value, &sep)
            .ok()
            .map(ProjectedReadResponse::Value),
        ProjectedReadRequest::Trim => {
            let Ok(value) = coerce_string(value) else {
                return None;
            };
            Some(ProjectedReadResponse::Value(Value::String(
                value.trim().into(),
            )))
        }
        ProjectedReadRequest::Slice { start, end } => match value {
            Value::String(value) => Some(ProjectedReadResponse::Value(Value::String(
                slice_string(value, start, end).into(),
            ))),
            Value::List(items) => {
                let Some((start, end)) = clamp_slice_bounds(start, end, items.len()) else {
                    return Some(ProjectedReadResponse::Value(Value::List(Vec::new().into())));
                };
                Some(ProjectedReadResponse::Value(Value::List(
                    items[start..end].to_vec().into(),
                )))
            }
            _ => None,
        },
        ProjectedReadRequest::Push(item) => execute_push_builtin(value.clone(), item)
            .ok()
            .map(ProjectedReadResponse::Value),
        ProjectedReadRequest::ToNumber => as_number(value)
            .ok()
            .map(Value::Number)
            .map(ProjectedReadResponse::Value),
        ProjectedReadRequest::JsonParse => {
            let Ok(text) = coerce_string(value) else {
                return None;
            };
            serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .map(from_json)
                .map(ProjectedReadResponse::Value)
        }
        ProjectedReadRequest::SliceBound => as_slice_bound(value).ok().map(|bound| {
            ProjectedReadResponse::Value(match bound {
                Some(value) => Value::Number(value as f64),
                None => Value::Null,
            })
        }),
        ProjectedReadRequest::RangeBound => as_range_bound(value)
            .ok()
            .map(|value| ProjectedReadResponse::Value(Value::Number(value as f64))),
        ProjectedReadRequest::Render => Some(ProjectedReadResponse::Text(
            stringify_value(value).expect("projected fixture should stringify"),
        )),
        ProjectedReadRequest::Materialize => Some(ProjectedReadResponse::Value(value.clone())),
    }
}

impl TestView for ProjectedFixture {
    fn type_name(&self) -> &str {
        value_type_name(&self.value)
    }

    fn read_one(&self, request: ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        if matches!(request, ProjectedReadRequest::Materialize) {
            self.materialize_count.fetch_add(1, Ordering::SeqCst);
        }
        projected_response_from_value(&self.value, request)
    }
}

fn projected_value_binding(name: &str, value: Value) -> ProjectedBindings {
    let mut projected = ProjectedBindings::new();
    projected.insert(name, ProjectedValue::scalar(name.to_string(), value));
    projected
}

fn projected_custom_binding(name: &str, value: Arc<dyn TestView>) -> ProjectedBindings {
    let mut projected = ProjectedBindings::new();
    projected.insert(name, test_view(name, value));
    projected
}

async fn exec_with_global_state(
    name: &str,
    value: Value,
    program: Program,
) -> Result<(Value, State), RuntimeError> {
    let mut state = State::new();
    state.insert_global(name, value)?;
    let outcome = execute_compiled(&compile_program(&program), &mut state, &Host).await?;
    match outcome {
        ExecutionOutcome::Finished(value) => Ok((value, state)),
        ExecutionOutcome::Continued => panic!("expected `finish` in test program"),
        ExecutionOutcome::Failed(value) => panic!("unexpected process failure: {value}"),
    }
}

async fn assert_projected_parity(name: &str, value: Value, source: &str, program: Program) {
    let (normal, _) = exec_with_global_state(name, value.clone(), program.clone())
        .await
        .expect("normal global should run");
    let projected = projected_value_binding(name, value.clone());
    let (projected_scalar, _) = exec_with_projected(program.clone(), &projected)
        .await
        .expect("scalar projected binding should run");
    assert_eq!(
        to_json(&projected_scalar),
        to_json(&normal),
        "scalar projected binding diverged for `{source}`"
    );

    let custom_value = ProjectedFixture::new(value);
    let projected = projected_custom_binding(name, custom_value);
    let (projected_custom, _) = exec_with_projected(program, &projected)
        .await
        .expect("custom projected binding should run");
    assert_eq!(
        to_json(&projected_custom),
        to_json(&normal),
        "custom projected binding diverged for `{source}`"
    );
}

#[test]
fn projected_bindings_reject_duplicate_checked_insertions() {
    let mut projected = ProjectedBindings::new();
    projected
        .try_insert("history", ProjectedValue::scalar("history", Value::Null))
        .expect("first binding should succeed");
    let err = projected
        .try_insert("history", ProjectedValue::scalar("history", Value::Null))
        .expect_err("duplicate binding should fail");
    assert_eq!(err.name(), "history");
}

pub(super) async fn exec_with_projected(
    program: Program,
    projected: &ProjectedBindings,
) -> Result<(Value, State), RuntimeError> {
    let mut state = State::new();
    let outcome = execute_compiled_with_projected_bindings(
        &compile_program(&program),
        &mut state,
        &Host,
        projected,
    )
    .await?;
    match outcome {
        ExecutionOutcome::Finished(value) => Ok((value, state)),
        ExecutionOutcome::Continued => panic!("expected `finish` in test program"),
        ExecutionOutcome::Failed(value) => panic!("unexpected process failure: {value}"),
    }
}

/// A descriptor that answers nothing at all: every read is a decision it has
/// not made.
struct SilentDescriptor;

impl TestView for SilentDescriptor {
    fn type_name(&self) -> &str {
        "widget"
    }

    fn read_one(&self, _request: ProjectedReadRequest) -> Option<ProjectedReadResponse> {
        None
    }
}

/// A read a descriptor does not answer, and for which no absent value stands
/// in, is a typed refusal naming the binding, its type and the request --
/// mirroring `EmptyUnsupported`/`KeysUnsupported` rather than widening into
/// `false`, `null` or an empty key set (FIG-2863).
#[tokio::test(flavor = "current_thread")]
async fn an_unanswered_read_refuses_with_the_binding_and_request_named() {
    let projected = test_view("widget", Arc::new(SilentDescriptor));

    let errors = with_test_views(|| {
        [
            ("len", projected.len().err()),
            ("empty", projected.empty().err()),
            ("truthy", projected.truthy().err()),
            ("keys", projected.keys().err()),
            ("values", projected.values().err()),
            ("contains", projected.contains(&Value::Number(1.0)).err()),
        ]
    });
    for (label, error) in errors {
        let error = error.unwrap_or_else(|| panic!("`{label}` must refuse"));
        assert!(
            matches!(
                error,
                RuntimeError::ProjectedReadUnsupported {
                    ref name,
                    ref type_name,
                    ref request,
                } if name == "widget" && type_name == "widget" && request == label
            ),
            "unexpected error for `{label}`: {error:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_list_len_and_index_are_lazy() {
    let list = TestProjectedValue::new(vec![Value::String("first".into()), Value::Number(2.0)]);
    let projected = projected_list_bindings("history", Arc::clone(&list));

    let (value, _) = exec_with_projected(
        builders::program(vec![builders::finish(builders::record(vec![
            (
                "n",
                builders::builtin("len", vec![builders::var("history")]),
            ),
            (
                "first",
                builders::index(builders::var("history"), builders::num(0.0)),
            ),
            (
                "missing",
                builders::index(builders::var("history"), builders::num(9.0)),
            ),
        ]))]),
        &projected,
    )
    .await
    .expect("projected read");

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(record["n"], Value::Number(2.0));
    assert_eq!(record["first"], Value::String("first".into()));
    // An index past the end reads `undefined`, the one ECMA answer now that
    // TypeScript is the only RLM language (ADR 0096), and a member read that
    // yields a scalar is that plain value (FIG-5197).
    assert_eq!(record["missing"], Value::Undefined);
    assert_eq!(list.get_count.load(Ordering::SeqCst), 1);
    assert_eq!(list.materialize_count.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn projected_bindings_are_read_only_and_not_snapshotted() {
    let list = TestProjectedValue::new(vec![Value::String("entry".into())]);
    let projected = projected_list_bindings("history", Arc::clone(&list));

    // history = []
    // finish history
    let err = exec_with_projected(
        builders::program(vec![
            builders::assign("history", builders::list(Vec::new())),
            builders::finish(builders::var("history")),
        ]),
        &projected,
    )
    .await
    .expect_err("projected root assignment should fail");
    assert!(err.to_string().contains("read-only projected binding"));

    // alias = history
    // finish alias[0]
    let (_, state) = exec_with_projected(
        builders::program(vec![
            builders::assign("alias", builders::var("history")),
            builders::finish(builders::index(builders::var("alias"), builders::num(0.0))),
        ]),
        &projected,
    )
    .await
    .expect("alias should materialize");
    assert!(state.snapshot().globals().get("history").is_none());
    assert!(matches!(
        state.snapshot().globals().get("alias"),
        Some(Value::List(_))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn print_projected_leaves_projection_to_host_and_finish_materializes() {
    let list = TestProjectedValue::new(vec![Value::String("entry".into())]);
    let projected = projected_list_bindings("history", Arc::clone(&list));

    let (value, _) = exec_with_projected(
        builders::program(vec![
            builders::print(builders::var("history")),
            builders::finish(builders::var("history")),
        ]),
        &projected,
    )
    .await
    .expect("projected print and finish");
    // The host reads a finished projection through its providers.
    let _ = with_test_views(|| to_json(&value));

    assert_eq!(list.render_count.load(Ordering::SeqCst), 0);
    assert_eq!(list.materialize_count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn flat_search_match_projected_text_separates_slice_snapshot_and_stringify_metrics() {
    let text = SearchProjectedText::new("0123456789abcdefghijklmnopqrstuvwxyz");
    let mut match_record = Record::default();
    match_record.insert("title".to_string(), Value::String("first".into()));
    match_record.insert(
        "text".to_string(),
        Value::Projected(test_view("search.matches[0].text", text.clone())),
    );
    let mut result_record = Record::default();
    result_record.insert(
        "matches".to_string(),
        Value::List(vec![Value::Record(Arc::new(match_record))].into()),
    );

    // m = r.matches[0]
    // head = slice(m.text, 10, 30)
    // finish { title: m.title, head: head }
    let (value, state) = exec_with_global_state(
        "r",
        Value::Record(Arc::new(result_record)),
        builders::program(vec![
            builders::assign(
                "m",
                builders::index(
                    builders::field(builders::var("r"), "matches"),
                    builders::num(0.0),
                ),
            ),
            builders::assign(
                "head",
                builders::builtin(
                    "slice",
                    vec![
                        builders::field(builders::var("m"), "text"),
                        builders::num(10.0),
                        builders::num(30.0),
                    ],
                ),
            ),
            builders::finish(builders::record(vec![
                ("title", builders::field(builders::var("m"), "title")),
                ("head", builders::var("head")),
            ])),
        ]),
    )
    .await
    .expect("projected search result should run");
    let record = value.as_record().expect("final record");
    assert_eq!(record["title"], Value::String("first".into()));
    assert_eq!(record["head"], Value::String("abcdefghijklmnopqrst".into()));
    assert_eq!(text.slice_count.load(Ordering::SeqCst), 1);
    assert_eq!(text.slices(), vec![(Some(10), Some(30))]);
    assert_eq!(text.render_count.load(Ordering::SeqCst), 0);
    assert_eq!(text.materialize_count.load(Ordering::SeqCst), 0);

    let snapshot = state.snapshot();
    let Some(Value::Record(stored_match)) = snapshot.globals().get("m") else {
        panic!("stored match should stay flat record");
    };
    assert!(matches!(
        stored_match.get("text"),
        Some(Value::Projected(_))
    ));
    let encoded = snapshot.to_canonical_bytes().expect("snapshot encode");
    let encoded_snapshot = Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");
    let Some(Value::Record(encoded_match)) = encoded_snapshot.globals().get("m") else {
        panic!("encoded match should stay a flat record");
    };
    let Some(Value::Projected(encoded_text)) = encoded_match.get("text") else {
        panic!("encoded text should stay projected");
    };
    assert_eq!(encoded_text.name(), "search.matches[0].text");
    assert_eq!(text.render_count.load(Ordering::SeqCst), 0);
    assert_eq!(text.materialize_count.load(Ordering::SeqCst), 0);

    let program = builders::program(vec![builders::finish(builders::builtin(
        "to_string",
        vec![builders::field(builders::var("m"), "text")],
    ))]);
    let mut state = State::from_snapshot(snapshot);
    let outcome = execute_program(&program, &mut state, &Host)
        .await
        .expect("explicit stringify should run");
    let ExecutionOutcome::Finished(Value::String(full_text)) = outcome else {
        panic!("expected full text");
    };
    assert_eq!(full_text.as_str(), "0123456789abcdefghijklmnopqrstuvwxyz");
    assert_eq!(text.render_count.load(Ordering::SeqCst), 1);
    assert_eq!(text.materialize_count.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn projected_values_match_normal_values_for_language_operations() {
    assert_projected_parity(
        "input",
        from_json(serde_json::json!({
            "context": "  alpha,beta,gamma  ",
            "items": ["red", "green", "blue"],
            "record": { "a": 1, "b": 2 },
            "n": "42",
            "json": "{\"ok\":true}",
            "start": 1,
            "end": 4
        })),
        LANGUAGE_OPERATION_PARITY_SOURCE,
        {
            let input = |field: &str| builders::field(builders::var("input"), field);
            let record_a = || builders::field(input("record"), "a");
            let trimmed_context = || builders::builtin("trim", vec![input("context")]);
            builders::program(vec![
                builders::assign(
                    "out",
                    builders::record(vec![
                        (
                            "exact_smoke",
                            builders::builtin(
                                "slice",
                                vec![input("context"), builders::num(2.0), builders::num(7.0)],
                            ),
                        ),
                        ("field", record_a()),
                        ("index", builders::index(input("items"), input("start"))),
                        (
                            "len_context",
                            builders::builtin("len", vec![input("context")]),
                        ),
                        (
                            "empty_items",
                            builders::builtin("empty", vec![input("items")]),
                        ),
                        (
                            "keys_record",
                            builders::builtin("keys", vec![input("record")]),
                        ),
                        (
                            "values_record",
                            builders::builtin("values", vec![input("record")]),
                        ),
                        (
                            "contains_text",
                            builders::builtin(
                                "contains",
                                vec![input("context"), builders::string("beta")],
                            ),
                        ),
                        (
                            "contains_list",
                            builders::builtin(
                                "contains",
                                vec![input("items"), builders::string("green")],
                            ),
                        ),
                        (
                            "contains_record",
                            builders::builtin(
                                "contains",
                                vec![input("record"), builders::string("a")],
                            ),
                        ),
                        (
                            "find_text",
                            builders::builtin(
                                "find",
                                vec![input("context"), builders::string("beta")],
                            ),
                        ),
                        (
                            "grep_text",
                            builders::builtin(
                                "grep_text",
                                vec![input("context"), builders::string("beta")],
                            ),
                        ),
                        (
                            "starts",
                            builders::builtin(
                                "starts_with",
                                vec![trimmed_context(), builders::string("alpha")],
                            ),
                        ),
                        (
                            "ends",
                            builders::builtin(
                                "ends_with",
                                vec![trimmed_context(), builders::string("gamma")],
                            ),
                        ),
                        (
                            "split",
                            builders::builtin(
                                "split",
                                vec![trimmed_context(), builders::string(",")],
                            ),
                        ),
                        (
                            "joined",
                            builders::builtin("join", vec![input("items"), builders::string("|")]),
                        ),
                        ("trimmed", trimmed_context()),
                        (
                            "list_slice",
                            builders::builtin(
                                "slice",
                                vec![input("items"), builders::num(0.0), builders::num(2.0)],
                            ),
                        ),
                        (
                            "pushed",
                            builders::builtin(
                                "push",
                                vec![input("items"), builders::string("yellow")],
                            ),
                        ),
                        ("as_int", builders::builtin("to_int", vec![input("n")])),
                        ("as_float", builders::builtin("to_float", vec![input("n")])),
                        (
                            "parsed",
                            builders::builtin("json_parse", vec![input("json")]),
                        ),
                        (
                            "plus",
                            builders::binary(record_a(), CoercingBinaryOp::Add, builders::num(1.0)),
                        ),
                        ("neg", builders::unary(CoercingUnaryOp::Negate, record_a())),
                        (
                            "cmp",
                            builders::binary(
                                record_a(),
                                CoercingBinaryOp::Less,
                                builders::field(input("record"), "b"),
                            ),
                        ),
                        (
                            "truthy",
                            builders::if_else(
                                record_a(),
                                builders::string("yes"),
                                builders::string("no"),
                            ),
                        ),
                        (
                            "formatted",
                            builders::builtin(
                                "format",
                                vec![builders::string("ctx={}"), input("context")],
                            ),
                        ),
                        (
                            "text",
                            builders::builtin("to_string", vec![input("record")]),
                        ),
                    ]),
                ),
                builders::finish(builders::var("out")),
            ])
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn projected_values_match_normal_values_for_ranges_validation_and_iteration() {
    assert_projected_parity(
        "input",
        from_json(serde_json::json!({
            "start": 2,
            "end": 5,
            "item": { "name": "pkg", "version": "1.0" }
        })),
        RANGE_PARITY_SOURCE,
        {
            let input = |field: &str| builders::field(builders::var("input"), field);
            let range = || builders::builtin("range", vec![input("start"), input("end")]);
            builders::program(vec![
                builders::assign("total", builders::num(0.0)),
                builders::for_in(
                    "i",
                    range(),
                    builders::block(vec![builders::assign(
                        "total",
                        builders::binary(
                            builders::var("total"),
                            CoercingBinaryOp::Add,
                            builders::var("i"),
                        ),
                    )]),
                ),
                builders::finish(builders::record(vec![
                    ("range_values", range()),
                    ("total", builders::var("total")),
                    (
                        "validated",
                        builders::builtin(
                            "validate",
                            vec![
                                input("item"),
                                builders::type_literal(TypeExpr::Object(vec![
                                    builders::type_field("name", TypeExpr::Str, false),
                                    builders::type_field("version", TypeExpr::Str, false),
                                ])),
                            ],
                        ),
                    ),
                ])),
            ])
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn projected_empty_rejects_scalar_like_normal_empty() {
    let empty_n = || {
        builders::program(vec![builders::finish(builders::builtin(
            "empty",
            vec![builders::var("n")],
        ))])
    };
    let normal = exec_with_global_state("n", Value::Number(1.0), empty_n())
        .await
        .expect_err("normal scalar empty should fail");
    let projected = projected_value_binding("n", Value::Number(1.0));
    let projected_err = exec_with_projected(empty_n(), &projected)
        .await
        .expect_err("projected scalar empty should fail");
    assert_eq!(projected_err, normal);
}

#[tokio::test(flavor = "current_thread")]
async fn image_values_expose_read_only_metadata_fields() {
    let value = exec_with_global(
        "img",
        test_image(),
        builders::program(vec![builders::finish(builders::list(
            ["id", "label", "size", "width", "height", "missing"]
                .into_iter()
                .map(|field| builders::field(builders::var("img"), field))
                .collect(),
        ))]),
    )
    .await
    .expect("image fields should read");

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("img-1".into()),
                Value::String("chart.png".into()),
                Value::Number(1234.0),
                Value::Number(640.0),
                Value::Number(480.0),
                Value::Null,
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn image_values_serialize_as_descriptors() {
    let image = test_image();
    assert_eq!(
        to_json(&image),
        serde_json::json!({
            "type": "image",
            "id": "img-1",
            "mime": "image/png",
            "label": "chart.png",
            "size": 1234,
            "width": 640,
            "height": 480
        })
    );
    assert_eq!(
        stringify_value(&image).expect("stringify image"),
        r#"{"height":480,"id":"img-1","mime":"image/png","label":"chart.png","size":1234,"type":"image","width":640}"#
    );
    assert_eq!(
        exec_with_global(
            "img",
            image.clone(),
            builders::program(vec![builders::finish(builders::var("img"))]),
        )
        .await
        .expect("finish image"),
        image
    );
}

#[tokio::test(flavor = "current_thread")]
async fn image_values_are_immutable_and_len_is_unsupported() {
    // img.label = "other"
    // finish img
    let err = exec_with_global(
        "img",
        test_image(),
        builders::program(vec![
            builders::assign_path(
                "img",
                vec![builders::field_step("label")],
                builders::string("other"),
            ),
            builders::finish(builders::var("img")),
        ]),
    )
    .await
    .expect_err("image field assignment should fail");
    assert_eq!(err, RuntimeError::ImmutableImageFields);

    let err = exec_with_global(
        "img",
        test_image(),
        builders::program(vec![builders::finish(builders::builtin(
            "len",
            vec![builders::var("img")],
        ))]),
    )
    .await
    .expect_err("len image should fail");
    assert_eq!(err, RuntimeError::LenUnsupported);
}

/// FIG-2865: a projection nested inside a container survives the snapshot wire
/// with its canonical fields intact. A projection is plain data (ADR 0132 §9),
/// so what decodes is the same projection of the same resource.
#[test]
fn nested_projection_survives_the_snapshot_wire() {
    let report = ResourceRef {
        projection: ProjectionType::new("report"),
        id: "7".into(),
        revision: Some("r1".into()),
    };
    let snapshot = Snapshot::new(
        [(
            "rows".to_string(),
            Value::List(
                vec![Value::Projected(ProjectedValue::resource(
                    "report",
                    "string",
                    report.clone(),
                ))]
                .into(),
            ),
        )]
        .into_iter()
        .collect(),
    );
    let encoded = snapshot.to_canonical_bytes().expect("snapshot encode");
    let snapshot = Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");

    let Some(Value::List(rows)) = snapshot.globals().get("rows") else {
        panic!("expected the nested container to survive the snapshot wire");
    };
    let Some(Value::Projected(nested)) = rows.first() else {
        panic!("expected a nested projection");
    };
    assert_eq!(nested.name(), "report");
    assert_eq!(nested.value_type_name(), "string");
    assert_eq!(
        nested.resource_ref(),
        Some(&report),
        "the resource must cross the wire unchanged"
    );
}
