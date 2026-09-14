use super::*;
use crate::ast::{BinaryOp, UnaryOp};

/// The lashlang spelling of the language-operation parity fixture, kept as the
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

/// The lashlang spelling of the range/validation/iteration parity fixture.
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
    state.globals.insert(name.to_string(), value);
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

#[derive(Default)]
struct SnapshotGuardProjectedValue {
    materialize_count: AtomicUsize,
    render_count: AtomicUsize,
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

impl ProjectedHostDescriptor for SnapshotGuardProjectedValue {
    fn type_name(&self) -> &str {
        "string"
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, Option<ProjectedReadResponse>> {
        Box::pin(async move {
            match request {
                ProjectedReadRequest::Render => {
                    self.render_count.fetch_add(1, Ordering::SeqCst);
                    Some(ProjectedReadResponse::Text(
                        "rendered full text".to_string(),
                    ))
                }
                ProjectedReadRequest::Materialize => {
                    self.materialize_count.fetch_add(1, Ordering::SeqCst);
                    Some(ProjectedReadResponse::Value(Value::String(
                        "materialized full text".into(),
                    )))
                }
                _ => None,
            }
        })
    }
}

impl ProjectedHostDescriptor for SearchProjectedText {
    fn type_name(&self) -> &str {
        "string"
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, Option<ProjectedReadResponse>> {
        Box::pin(async move {
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
        })
    }
}

impl ProjectedHostDescriptor for TestProjectedValue {
    fn type_name(&self) -> &str {
        "list"
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, Option<ProjectedReadResponse>> {
        Box::pin(async move {
            let ProjectedReadRequest::Index(index) = request else {
                return match request {
                    ProjectedReadRequest::Len => {
                        Some(ProjectedReadResponse::Len(self.values.len()))
                    }
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
        })
    }
}

fn projected_list_bindings(name: &str, list: Arc<TestProjectedValue>) -> ProjectedBindings {
    let mut projected = ProjectedBindings::new();
    projected.insert(name, ProjectedValue::custom(name.to_string(), list));
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
            let field = Name {
                symbol: intern_symbol(field.as_ref()),
                text: field,
            };
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
        ProjectedReadRequest::Push(item) => execute_push_builtin(value, item)
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

impl ProjectedHostDescriptor for ProjectedFixture {
    fn type_name(&self) -> &str {
        value_type_name(&self.value)
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, Option<ProjectedReadResponse>> {
        Box::pin(async move {
            if matches!(request, ProjectedReadRequest::Materialize) {
                self.materialize_count.fetch_add(1, Ordering::SeqCst);
            }
            projected_response_from_value(&self.value, request)
        })
    }
}

fn projected_value_binding(name: &str, value: Value) -> ProjectedBindings {
    let mut projected = ProjectedBindings::new();
    projected.insert(name, ProjectedValue::scalar(name.to_string(), value));
    projected
}

fn projected_custom_binding(
    name: &str,
    value: Arc<dyn ProjectedHostDescriptor>,
) -> ProjectedBindings {
    let mut projected = ProjectedBindings::new();
    projected.insert(name, ProjectedValue::custom(name.to_string(), value));
    projected
}

async fn exec_with_global_state(
    name: &str,
    value: Value,
    program: Program,
) -> Result<(Value, State), RuntimeError> {
    let mut state = State::new();
    state.globals.insert(name.to_string(), value);
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

impl ProjectedHostDescriptor for SilentDescriptor {
    fn type_name(&self) -> &str {
        "widget"
    }

    fn read_one(
        &self,
        _request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, Option<ProjectedReadResponse>> {
        Box::pin(async move { None })
    }
}

/// A read a descriptor does not answer, and for which no absent value stands
/// in, is a typed refusal naming the binding, its type and the request --
/// mirroring `EmptyUnsupported`/`KeysUnsupported` rather than widening into
/// `false`, `null` or an empty key set (FIG-2863).
#[tokio::test(flavor = "current_thread")]
async fn an_unanswered_read_refuses_with_the_binding_and_request_named() {
    let projected = ProjectedValue::custom("widget", Arc::new(SilentDescriptor));

    for (label, error) in [
        ("len", projected.len().await.err()),
        ("empty", projected.empty().await.err()),
        ("truthy", projected.truthy().await.err()),
        ("keys", projected.keys().await.err()),
        ("values", projected.values().await.err()),
        (
            "contains",
            projected.contains(&Value::Number(1.0)).await.err(),
        ),
    ] {
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

    // finish { n: len(history), first: history[0], missing: history[9] }
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
    // TypeScript is the only RLM language (ADR 0096); the projected wrapper is
    // kept so the path still says where the read came from.
    assert_eq!(
        record["missing"],
        Value::Projected(ProjectedValue::scalar("history[9]", Value::Undefined))
    );
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
async fn projected_children_can_be_lazy_inside_ordinary_records() {
    let body = TestProjectedValue::new(vec![Value::String("lazy markdown".into())]);
    let mut record = Record::default();
    record.insert("title".to_string(), Value::String("Rules".into()));
    record.insert(
        "body".to_string(),
        Value::Projected(ProjectedValue::custom("body", body.clone())),
    );
    let mut projected = ProjectedBindings::new();
    projected.insert(
        "rules",
        ProjectedValue::scalar("rules", Value::Record(Arc::new(record))),
    );

    // finish { title: rules.title, first_body_item: rules.body[0] }
    let (value, _) = exec_with_projected(
        builders::program(vec![builders::finish(builders::record(vec![
            ("title", builders::field(builders::var("rules"), "title")),
            (
                "first_body_item",
                builders::index(
                    builders::field(builders::var("rules"), "body"),
                    builders::num(0.0),
                ),
            ),
        ]))]),
        &projected,
    )
    .await
    .expect("projected child read");

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(record["title"], Value::String("Rules".into()));
    assert_eq!(
        record["first_body_item"],
        Value::String("lazy markdown".into())
    );
    assert_eq!(body.get_count.load(Ordering::SeqCst), 1);
    assert_eq!(body.materialize_count.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn print_projected_leaves_projection_to_host_and_finish_materializes() {
    let list = TestProjectedValue::new(vec![Value::String("entry".into())]);
    let projected = projected_list_bindings("history", Arc::clone(&list));

    // print history
    // finish history
    let (value, _) = exec_with_projected(
        builders::program(vec![
            builders::print(builders::var("history")),
            builders::finish(builders::var("history")),
        ]),
        &projected,
    )
    .await
    .expect("projected print and finish");
    let _ = to_json(&value);

    assert_eq!(list.render_count.load(Ordering::SeqCst), 0);
    assert_eq!(list.materialize_count.load(Ordering::SeqCst), 1);
}

#[test]
fn canonical_snapshot_encodes_projected_values_without_materializing() {
    let projected = Arc::new(SnapshotGuardProjectedValue::default());
    let mut state = State::new();
    state.globals.insert(
        "match_text".to_string(),
        Value::Projected(ProjectedValue::custom("matches[0].text", projected.clone())),
    );

    let encoded = state
        .snapshot()
        .to_canonical_bytes()
        .expect("snapshot encode");
    let restored = Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");

    assert_eq!(projected.render_count.load(Ordering::SeqCst), 0);
    assert_eq!(projected.materialize_count.load(Ordering::SeqCst), 0);
    let Some(Value::Projected(projected)) = restored.globals().get("match_text") else {
        panic!("expected projected placeholder");
    };
    assert_eq!(projected.name(), "matches[0].text");
    assert_eq!(projected.value_type_name(), "string");
    let encoded_text = String::from_utf8_lossy(&encoded);
    assert!(!encoded_text.contains("rendered full text"));
    assert!(!encoded_text.contains("materialized full text"));
}

#[tokio::test(flavor = "current_thread")]
async fn canonical_snapshot_restore_makes_projected_value_unavailable() {
    let snapshot = Snapshot::new(
        [(
            "match_text".to_string(),
            Value::Projected(ProjectedValue::custom(
                "matches[0].text",
                Arc::new(SnapshotGuardProjectedValue::default()),
            )),
        )]
        .into_iter()
        .collect(),
    );
    let encoded = snapshot.to_canonical_bytes().expect("snapshot encode");
    let snapshot = Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");

    let Some(Value::Projected(projected)) = snapshot.globals().get("match_text") else {
        panic!("expected projected placeholder");
    };

    assert_eq!(projected.name(), "matches[0].text");
    assert_eq!(projected.value_type_name(), "string");
    // Before FIG-2865 both of these produced the diagnostic *as data*: `render`
    // returned the sentence and `materialize` returned it as a `Value::String`,
    // so a restored placeholder read back as an English message where the host's
    // view used to be. Both now refuse, typed.
    assert!(matches!(
        projected.render().await,
        Err(RuntimeError::ProjectedValueUnavailable { ref name, ref type_name })
            if name == "matches[0].text" && type_name == "string"
    ));
    assert!(matches!(
        projected.materialize_async().await,
        Err(RuntimeError::ProjectedValueUnavailable { .. })
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn flat_search_match_projected_text_separates_slice_snapshot_and_stringify_metrics() {
    let text = SearchProjectedText::new("0123456789abcdefghijklmnopqrstuvwxyz");
    let mut match_record = Record::default();
    match_record.insert("title".to_string(), Value::String("first".into()));
    match_record.insert(
        "text".to_string(),
        Value::Projected(ProjectedValue::custom(
            "search.matches[0].text",
            text.clone(),
        )),
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

    // finish to_string(m.text)
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
                            builders::binary(record_a(), BinaryOp::Add, builders::num(1.0)),
                        ),
                        ("neg", builders::unary(UnaryOp::Negate, record_a())),
                        (
                            "cmp",
                            builders::binary(
                                record_a(),
                                BinaryOp::Less,
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
                        builders::binary(builders::var("total"), BinaryOp::Add, builders::var("i")),
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
    // finish empty(n)
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

struct OverrideProjectedValue {
    value: Value,
    calls: std::sync::Mutex<Vec<&'static str>>,
}

impl OverrideProjectedValue {
    fn new(value: Value) -> Arc<Self> {
        Arc::new(Self {
            value,
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn push_call(&self, name: &'static str) {
        self.calls.lock_recover().push(name);
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock_recover().clone()
    }
}

impl ProjectedHostDescriptor for OverrideProjectedValue {
    fn type_name(&self) -> &str {
        value_type_name(&self.value)
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, Option<ProjectedReadResponse>> {
        Box::pin(async move {
            match request {
                ProjectedReadRequest::Len => {
                    self.push_call("len");
                    value_len(&self.value).map(ProjectedReadResponse::Len)
                }
                ProjectedReadRequest::Empty => {
                    self.push_call("empty");
                    value_len(&self.value).map(|len| ProjectedReadResponse::Bool(len == 0))
                }
                ProjectedReadRequest::Truthy => {
                    self.push_call("truthy");
                    match is_truthy(&self.value) {
                        Ok(truthy) => Some(ProjectedReadResponse::Bool(truthy)),
                        Err(_) => None,
                    }
                }
                ProjectedReadRequest::Index(index) => {
                    self.push_call("get_index");
                    read_index_ref_direct(&self.value, &index)
                        .ok()
                        .map(ProjectedReadResponse::Value)
                }
                ProjectedReadRequest::Field(field) => {
                    self.push_call("get_field");
                    let field = Name {
                        symbol: intern_symbol(field.as_ref()),
                        text: field,
                    };
                    read_field_ref_direct(&self.value, &field)
                        .ok()
                        .map(ProjectedReadResponse::Value)
                }
                ProjectedReadRequest::Contains(needle) => {
                    self.push_call("contains");
                    Some(ProjectedReadResponse::Bool(
                        execute_contains_direct(&self.value, &needle).expect("contains override"),
                    ))
                }
                ProjectedReadRequest::Find { needle, start } => {
                    self.push_call("find");
                    execute_find_direct(&self.value, &needle, start)
                        .ok()
                        .map(ProjectedReadResponse::Value)
                }
                ProjectedReadRequest::GrepText(needle) => {
                    self.push_call("grep_text");
                    execute_grep_text_direct(&self.value, &needle)
                        .ok()
                        .map(ProjectedReadResponse::Value)
                }
                ProjectedReadRequest::Keys => {
                    self.push_call("keys");
                    match &self.value {
                        Value::Record(record) => Some(ProjectedReadResponse::Keys(
                            record.keys().map(ToString::to_string).collect(),
                        )),
                        _ => None,
                    }
                }
                ProjectedReadRequest::Values => {
                    self.push_call("values");
                    match &self.value {
                        Value::Record(record) => Some(ProjectedReadResponse::Value(Value::List(
                            record.values().cloned().collect::<Vec<_>>().into(),
                        ))),
                        _ => None,
                    }
                }
                ProjectedReadRequest::StartsWith(prefix) => {
                    self.push_call("starts_with");
                    let value = coerce_string(&self.value).expect("string receiver");
                    let prefix = coerce_string(&prefix).expect("string prefix");
                    Some(ProjectedReadResponse::Bool(
                        value.starts_with(prefix.as_ref()),
                    ))
                }
                ProjectedReadRequest::EndsWith(suffix) => {
                    self.push_call("ends_with");
                    let value = coerce_string(&self.value).expect("string receiver");
                    let suffix = coerce_string(&suffix).expect("string suffix");
                    Some(ProjectedReadResponse::Bool(
                        value.ends_with(suffix.as_ref()),
                    ))
                }
                ProjectedReadRequest::Split(needle) => {
                    self.push_call("split");
                    let value = coerce_string(&self.value).expect("string receiver");
                    let needle = coerce_string(&needle).expect("string needle");
                    Some(ProjectedReadResponse::Value(Value::List(
                        value
                            .split(needle.as_ref())
                            .map(|part| Value::String(part.to_string().into()))
                            .collect::<Vec<_>>()
                            .into(),
                    )))
                }
                ProjectedReadRequest::Join(sep) => {
                    self.push_call("join");
                    execute_join_builtin(&self.value, &sep)
                        .ok()
                        .map(ProjectedReadResponse::Value)
                }
                ProjectedReadRequest::Trim => {
                    self.push_call("trim");
                    let value = coerce_string(&self.value).expect("string receiver");
                    Some(ProjectedReadResponse::Value(Value::String(
                        value.trim().to_string().into(),
                    )))
                }
                ProjectedReadRequest::Slice { start, end } => {
                    self.push_call("slice");
                    match &self.value {
                        Value::String(value) => Some(ProjectedReadResponse::Value(Value::String(
                            slice_string(value, start, end).into(),
                        ))),
                        Value::List(items) => {
                            let Some((start, end)) = clamp_slice_bounds(start, end, items.len())
                            else {
                                return Some(ProjectedReadResponse::Value(Value::List(
                                    Vec::new().into(),
                                )));
                            };
                            Some(ProjectedReadResponse::Value(Value::List(
                                items[start..end].to_vec().into(),
                            )))
                        }
                        _ => None,
                    }
                }
                ProjectedReadRequest::Push(item) => {
                    self.push_call("push");
                    execute_push_builtin(&self.value, item)
                        .ok()
                        .map(ProjectedReadResponse::Value)
                }
                ProjectedReadRequest::ToNumber => {
                    self.push_call("to_number");
                    as_number(&self.value)
                        .ok()
                        .map(Value::Number)
                        .map(ProjectedReadResponse::Value)
                }
                ProjectedReadRequest::JsonParse => {
                    self.push_call("json_parse");
                    let value = coerce_string(&self.value).expect("json text");
                    serde_json::from_str::<serde_json::Value>(&value)
                        .ok()
                        .map(from_json)
                        .map(ProjectedReadResponse::Value)
                }
                ProjectedReadRequest::SliceBound => {
                    self.push_call("slice_bound");
                    as_slice_bound(&self.value).ok().map(|bound| {
                        ProjectedReadResponse::Value(match bound {
                            Some(value) => Value::Number(value as f64),
                            None => Value::Null,
                        })
                    })
                }
                ProjectedReadRequest::RangeBound => {
                    self.push_call("range_bound");
                    as_range_bound(&self.value)
                        .ok()
                        .map(|value| ProjectedReadResponse::Value(Value::Number(value as f64)))
                }
                ProjectedReadRequest::Materialize => {
                    self.push_call("materialize");
                    Some(ProjectedReadResponse::Value(self.value.clone()))
                }
                ProjectedReadRequest::Render => Some(ProjectedReadResponse::Text(
                    stringify_value(&self.value).expect("render projected override"),
                )),
            }
        })
    }
}

async fn assert_override_uses_hook(
    source: &str,
    finished: Expr,
    name: &'static str,
    value: Value,
    expected_hook: &'static str,
) {
    let projected_value = OverrideProjectedValue::new(value);
    let mut projected = ProjectedBindings::new();
    projected.insert(
        name,
        ProjectedValue::custom(
            name,
            projected_value.clone() as Arc<dyn ProjectedHostDescriptor>,
        ),
    );
    exec_with_projected(
        builders::program(vec![builders::finish(finished)]),
        &projected,
    )
    .await
    .expect("override projected operation should run");
    let calls = projected_value.calls();
    assert!(
        calls.contains(&expected_hook),
        "expected `{expected_hook}` override for `{source}`, got {calls:?}"
    );
    assert!(
        !calls.contains(&"materialize"),
        "`{source}` should use override hooks without materializing, got {calls:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn projected_host_descriptors_can_override_all_lazy_receiver_operations() {
    let record = from_json(serde_json::json!({ "a": 1, "b": 2 }));
    let list = from_json(serde_json::json!(["a", "b", "c"]));
    let p = || builders::var("p");
    let text = |value: &str| Value::String(value.into());

    let cases: Vec<(&str, Expr, Value, &'static str)> = vec![
        (
            "finish p.a",
            builders::field(p(), "a"),
            record.clone(),
            "get_field",
        ),
        (
            "finish p[1]",
            builders::index(p(), builders::num(1.0)),
            list.clone(),
            "get_index",
        ),
        (
            "finish len(p)",
            builders::builtin("len", vec![p()]),
            list.clone(),
            "len",
        ),
        (
            "finish empty(p)",
            builders::builtin("empty", vec![p()]),
            list.clone(),
            "empty",
        ),
        (
            "finish keys(p)",
            builders::builtin("keys", vec![p()]),
            record.clone(),
            "keys",
        ),
        (
            "finish values(p)",
            builders::builtin("values", vec![p()]),
            record.clone(),
            "values",
        ),
        (
            r#"finish contains(p, "b")"#,
            builders::builtin("contains", vec![p(), builders::string("b")]),
            list.clone(),
            "contains",
        ),
        (
            r#"finish find(p, "ph")"#,
            builders::builtin("find", vec![p(), builders::string("ph")]),
            text("alpha"),
            "find",
        ),
        (
            r#"finish grep_text(p, "beta")"#,
            builders::builtin("grep_text", vec![p(), builders::string("beta")]),
            text("alpha\nbeta\n"),
            "grep_text",
        ),
        (
            r#"finish starts_with(p, "al")"#,
            builders::builtin("starts_with", vec![p(), builders::string("al")]),
            text("alpha"),
            "starts_with",
        ),
        (
            r#"finish ends_with(p, "ha")"#,
            builders::builtin("ends_with", vec![p(), builders::string("ha")]),
            text("alpha"),
            "ends_with",
        ),
        (
            r#"finish split(p, ",")"#,
            builders::builtin("split", vec![p(), builders::string(",")]),
            text("a,b"),
            "split",
        ),
        (
            r#"finish join(p, "|")"#,
            builders::builtin("join", vec![p(), builders::string("|")]),
            list.clone(),
            "join",
        ),
        (
            "finish trim(p)",
            builders::builtin("trim", vec![p()]),
            text("  alpha  "),
            "trim",
        ),
        (
            "finish slice(p, 1, 3)",
            builders::builtin("slice", vec![p(), builders::num(1.0), builders::num(3.0)]),
            text("alpha"),
            "slice",
        ),
        (
            r#"finish push(p, "d")"#,
            builders::builtin("push", vec![p(), builders::string("d")]),
            list,
            "push",
        ),
        (
            "finish to_int(p)",
            builders::builtin("to_int", vec![p()]),
            text("42"),
            "to_number",
        ),
        (
            "finish to_float(p)",
            builders::builtin("to_float", vec![p()]),
            text("42.5"),
            "to_number",
        ),
        (
            "finish json_parse(p)",
            builders::builtin("json_parse", vec![p()]),
            text(r#"{"ok":true}"#),
            "json_parse",
        ),
        (
            r#"finish slice("abcdef", p, null)"#,
            builders::builtin(
                "slice",
                vec![builders::string("abcdef"), p(), builders::null()],
            ),
            Value::Number(2.0),
            "slice_bound",
        ),
        (
            "finish range(p, 4)",
            builders::builtin("range", vec![p(), builders::num(4.0)]),
            Value::Number(1.0),
            "range_bound",
        ),
        (
            "finish range(0, p, 2)",
            builders::builtin("range", vec![builders::num(0.0), p(), builders::num(2.0)]),
            Value::Number(4.0),
            "range_bound",
        ),
        (
            "finish p ? 1 : 2",
            builders::if_else(p(), builders::num(1.0), builders::num(2.0)),
            Value::Number(1.0),
            "truthy",
        ),
    ];

    for (source, finished, value, expected_hook) in cases {
        assert_override_uses_hook(source, finished, "p", value, expected_hook).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn image_values_expose_read_only_metadata_fields() {
    // finish [img.id, img.label, img.size, img.width, img.height, img.missing]
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
        // finish img
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

    // finish len(img)
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

#[tokio::test(flavor = "current_thread")]
async fn false_if_branch_and_finish_inside_loop_are_covered() {
    // `if false { out = 1 } else { out = 2 }` / `finish out`
    let value = exec(builders::program(vec![
        builders::if_else(
            builders::bool_lit(false),
            builders::block(vec![builders::assign("out", builders::num(1.0))]),
            builders::block(vec![builders::assign("out", builders::num(2.0))]),
        ),
        builders::finish(builders::var("out")),
    ]))
    .await
    .expect("else branch should succeed");
    assert_eq!(value, Value::Number(2.0));

    // `for x in [1, 2] { finish x }` / `finish 0`
    let value = exec(builders::program(vec![
        builders::for_in(
            "x",
            builders::list(vec![builders::num(1.0), builders::num(2.0)]),
            builders::block(vec![builders::finish(builders::var("x"))]),
        ),
        builders::finish(builders::num(0.0)),
    ]))
    .await
    .expect("finish inside loop should bubble out");
    assert_eq!(value, Value::Number(1.0));
}

/// Awaiting a list of process starts joins each handle through the durable
/// process-await seam and never reaches the tool batch. The record form this
/// replaces settled its fields recursively, which was the retired surface
/// dialect's rule: settlement is shallow over element positions (ADR 0096).
#[tokio::test(flavor = "current_thread")]
async fn await_list_process_starts_and_joins_handles() {
    struct BatchHost {
        calls: AtomicUsize,
        batches: AtomicUsize,
    }

    impl ExecutionHost for BatchHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::StartProcess(start) => {
                    self.calls.fetch_add(1, Ordering::Relaxed);
                    let mut handle = Record::new();
                    handle.insert("__handle__".to_string(), Value::String("process".into()));
                    handle.insert(
                        "process".to_string(),
                        Value::String(start.process_name.into()),
                    );
                    handle.insert(
                        "value".to_string(),
                        start.args.get("value").cloned().unwrap_or(Value::Null),
                    );
                    Ok(AbilityResult::Value(Value::Record(Arc::new(handle))))
                }
                AbilityOp::Await(handle) => {
                    let value = handle
                        .as_record()
                        .and_then(|record| record.get("value"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    Ok(AbilityResult::Value(value))
                }
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new("unsupported host ability")),
            }
        }
    }

    let host = BatchHost {
        calls: AtomicUsize::new(0),
        batches: AtomicUsize::new(0),
    };
    // process echo(value: str) { finish value }
    // result = await [start echo(value: "a"), start echo(value: "b")]
    // finish [result[0]?, result[1]?]
    let start_echo =
        |value: &str| builders::start("echo", vec![("value", builders::string(value))]);
    let unwrap_result = |index: f64| {
        builders::unwrap(builders::index(
            builders::var("result"),
            builders::num(index),
        ))
    };
    let program = builders::module(
        vec![builders::process(
            "echo",
            vec![builders::param("value", TypeExpr::Str)],
            builders::block(vec![builders::finish(builders::var("value"))]),
        )],
        vec![
            builders::assign(
                "result",
                builders::await_expr(builders::list(vec![start_echo("a"), start_echo("b")])),
            ),
            builders::finish(builders::list(vec![unwrap_result(0.0), unwrap_result(1.0)])),
        ],
    );
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &host)
        .await
        .expect("program should run");

    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::String("a".into()), Value::String("b".into())].into()
        ))
    );
    assert_eq!(host.calls.load(Ordering::Relaxed), 2);
    assert_eq!(host.batches.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn truthiness_covers_scalar_and_container_values() {
    assert!(!is_truthy(&Value::Null).expect("null truthiness"));
    assert!(!is_truthy(&Value::Bool(false)).expect("bool truthiness"));
    assert!(!is_truthy(&Value::Number(0.0)).expect("number truthiness"));
    assert!(!is_truthy(&Value::String(String::new().into())).expect("string truthiness"));
    assert!(is_truthy(&Value::Bool(true)).expect("bool truthiness"));
    assert!(is_truthy(&Value::Number(1.0)).expect("number truthiness"));
    assert!(is_truthy(&Value::List(Vec::new().into())).expect("list truthiness"));
    assert!(is_truthy(&Value::Record(Record::default().into())).expect("record truthiness"));
}

/// FIG-2865: a projection nested inside a container survives the snapshot wire
/// with its three canonical fields intact. Before, the wire only ever saw a
/// top-level projection; a nested one was written and read back with no way to
/// tell the placeholder from the live view.
#[test]
fn nested_projection_survives_the_snapshot_wire() {
    let snapshot = nested_projection_snapshot();
    let encoded = snapshot.to_canonical_bytes().expect("snapshot encode");
    let snapshot = Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");

    let Some(Value::List(rows)) = snapshot.globals().get("rows") else {
        panic!("expected the nested container to survive the snapshot wire");
    };
    let Some(Value::Projected(nested)) = rows.first() else {
        panic!("expected a nested projected placeholder");
    };
    assert_eq!(nested.name(), "report");
    assert_eq!(nested.value_type_name(), "string");
    assert_eq!(
        nested.projection_ref(),
        Some(&serde_json::json!({ "kind": "report", "id": 7 })),
        "`projection_ref` must cross the wire unchanged"
    );
    assert!(
        nested.is_unavailable(),
        "a decoded projection is a placeholder"
    );
}

fn nested_projection_snapshot() -> Snapshot {
    Snapshot::new(
        [(
            "rows".to_string(),
            Value::List(
                vec![Value::Projected(
                    ProjectedValue::custom_with_projection_ref(
                        "report",
                        Arc::new(SnapshotGuardProjectedValue::default()),
                        serde_json::json!({ "kind": "report", "id": 7 }),
                    ),
                )]
                .into(),
            ),
        )]
        .into_iter()
        .collect(),
    )
}
