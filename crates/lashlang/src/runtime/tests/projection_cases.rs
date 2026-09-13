use super::*;

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

async fn exec_with_global(name: &str, value: Value, source: &str) -> Result<Value, RuntimeError> {
    let program = crate::parse(source).expect("program should parse");
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
    source: &str,
) -> Result<(Value, State), RuntimeError> {
    let program = crate::parse(source).expect("program should parse");
    let mut state = State::new();
    state.globals.insert(name.to_string(), value);
    let outcome = execute_compiled(&compile_program(&program), &mut state, &Host).await?;
    match outcome {
        ExecutionOutcome::Finished(value) => Ok((value, state)),
        ExecutionOutcome::Continued => panic!("expected `finish` in test program"),
        ExecutionOutcome::Failed(value) => panic!("unexpected process failure: {value}"),
    }
}

async fn assert_projected_parity(name: &str, value: Value, source: &str) {
    let (normal, _) = exec_with_global_state(name, value.clone(), source)
        .await
        .expect("normal global should run");
    let projected = projected_value_binding(name, value.clone());
    let (projected_scalar, _) = exec_with_projected(source, &projected)
        .await
        .expect("scalar projected binding should run");
    assert_eq!(
        to_json(&projected_scalar),
        to_json(&normal),
        "scalar projected binding diverged for `{source}`"
    );

    let custom_value = ProjectedFixture::new(value);
    let projected = projected_custom_binding(name, custom_value);
    let (projected_custom, _) = exec_with_projected(source, &projected)
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
    source: &str,
    projected: &ProjectedBindings,
) -> Result<(Value, State), RuntimeError> {
    let program = crate::parse(source).expect("program should parse");
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

    let (value, _) = exec_with_projected(
        "finish { n: len(history), first: history[0], missing: history[9] }",
        &projected,
    )
    .await
    .expect("projected read");

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(record["n"], Value::Number(2.0));
    assert_eq!(record["first"], Value::String("first".into()));
    assert_eq!(record["missing"], Value::Null);
    assert_eq!(list.get_count.load(Ordering::SeqCst), 1);
    assert_eq!(list.materialize_count.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn projected_bindings_are_read_only_and_not_snapshotted() {
    let list = TestProjectedValue::new(vec![Value::String("entry".into())]);
    let projected = projected_list_bindings("history", Arc::clone(&list));

    let err = exec_with_projected("history = []\nfinish history", &projected)
        .await
        .expect_err("projected root assignment should fail");
    assert!(err.to_string().contains("read-only projected binding"));

    let (_, state) = exec_with_projected("alias = history\nfinish alias[0]", &projected)
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

    let (value, _) = exec_with_projected(
        "finish { title: rules.title, first_body_item: rules.body[0] }",
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

    let (value, _) = exec_with_projected("print history\nfinish history", &projected)
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

    let (value, state) = exec_with_global_state(
        "r",
        Value::Record(Arc::new(result_record)),
        "m = r.matches[0]\nhead = slice(m.text, 10, 30)\nfinish { title: m.title, head: head }",
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

    let program = crate::parse("finish to_string(m.text)").expect("program should parse");
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
        r#"
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
        "#,
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
        r#"
        total = 0
        for i in range(input.start, input.end) {
          total = total + i
        }
        finish {
          range_values: range(input.start, input.end),
          total: total,
          validated: validate(input.item, Type { name: str, version: str })
        }
        "#,
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn projected_empty_rejects_scalar_like_normal_empty() {
    let normal = exec_with_global_state("n", Value::Number(1.0), "finish empty(n)")
        .await
        .expect_err("normal scalar empty should fail");
    let projected = projected_value_binding("n", Value::Number(1.0));
    let projected_err = exec_with_projected("finish empty(n)", &projected)
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
    exec_with_projected(source, &projected)
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

    assert_override_uses_hook("finish p.a", "p", record.clone(), "get_field").await;
    assert_override_uses_hook("finish p[1]", "p", list.clone(), "get_index").await;
    assert_override_uses_hook("finish len(p)", "p", list.clone(), "len").await;
    assert_override_uses_hook("finish empty(p)", "p", list.clone(), "empty").await;
    assert_override_uses_hook("finish keys(p)", "p", record.clone(), "keys").await;
    assert_override_uses_hook("finish values(p)", "p", record.clone(), "values").await;
    assert_override_uses_hook(r#"finish contains(p, "b")"#, "p", list.clone(), "contains").await;
    assert_override_uses_hook(
        r#"finish find(p, "ph")"#,
        "p",
        Value::String("alpha".into()),
        "find",
    )
    .await;
    assert_override_uses_hook(
        r#"finish grep_text(p, "beta")"#,
        "p",
        Value::String("alpha\nbeta\n".into()),
        "grep_text",
    )
    .await;
    assert_override_uses_hook(
        r#"finish starts_with(p, "al")"#,
        "p",
        Value::String("alpha".into()),
        "starts_with",
    )
    .await;
    assert_override_uses_hook(
        r#"finish ends_with(p, "ha")"#,
        "p",
        Value::String("alpha".into()),
        "ends_with",
    )
    .await;
    assert_override_uses_hook(
        r#"finish split(p, ",")"#,
        "p",
        Value::String("a,b".into()),
        "split",
    )
    .await;
    assert_override_uses_hook(r#"finish join(p, "|")"#, "p", list.clone(), "join").await;
    assert_override_uses_hook(
        "finish trim(p)",
        "p",
        Value::String("  alpha  ".into()),
        "trim",
    )
    .await;
    assert_override_uses_hook(
        "finish slice(p, 1, 3)",
        "p",
        Value::String("alpha".into()),
        "slice",
    )
    .await;
    assert_override_uses_hook("finish push(p, \"d\")", "p", list, "push").await;
    assert_override_uses_hook(
        "finish to_int(p)",
        "p",
        Value::String("42".into()),
        "to_number",
    )
    .await;
    assert_override_uses_hook(
        "finish to_float(p)",
        "p",
        Value::String("42.5".into()),
        "to_number",
    )
    .await;
    assert_override_uses_hook(
        "finish json_parse(p)",
        "p",
        Value::String("{\"ok\":true}".into()),
        "json_parse",
    )
    .await;
    assert_override_uses_hook(
        "finish slice(\"abcdef\", p, null)",
        "p",
        Value::Number(2.0),
        "slice_bound",
    )
    .await;
    assert_override_uses_hook("finish range(p, 4)", "p", Value::Number(1.0), "range_bound").await;
    assert_override_uses_hook(
        "finish range(0, p, 2)",
        "p",
        Value::Number(4.0),
        "range_bound",
    )
    .await;
    assert_override_uses_hook("finish p ? 1 : 2", "p", Value::Number(1.0), "truthy").await;
}

#[tokio::test(flavor = "current_thread")]
async fn image_values_expose_read_only_metadata_fields() {
    let value = exec_with_global(
        "img",
        test_image(),
        "finish [img.id, img.label, img.size, img.width, img.height, img.missing]",
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
        exec_with_global("img", image.clone(), "finish img")
            .await
            .expect("finish image"),
        image
    );
}

#[tokio::test(flavor = "current_thread")]
async fn image_values_are_immutable_and_len_is_unsupported() {
    let err = exec_with_global("img", test_image(), "img.label = \"other\"\nfinish img")
        .await
        .expect_err("image field assignment should fail");
    assert_eq!(err, RuntimeError::ImmutableImageFields);

    let err = exec_with_global("img", test_image(), "finish len(img)")
        .await
        .expect_err("len image should fail");
    assert_eq!(err, RuntimeError::LenUnsupported);
}

#[tokio::test(flavor = "current_thread")]
async fn false_if_branch_and_finish_inside_loop_are_covered() {
    let value = exec(
        r#"
        if false {
          out = 1
        } else {
          out = 2
        }
        finish out
        "#,
    )
    .await
    .expect("else branch should succeed");
    assert_eq!(value, Value::Number(2.0));

    let value = exec(
        r#"
        for x in [1, 2] {
          finish x
        }
        finish 0
        "#,
    )
    .await
    .expect("finish inside loop should bubble out");
    assert_eq!(value, Value::Number(1.0));
}

#[tokio::test(flavor = "current_thread")]
async fn await_record_process_starts_and_joins_handles() {
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
    let program = crate::parse(
        r#"
        process echo(value: str) { finish value }
        result = await {
          left: start echo(value: "a"),
          right: start echo(value: "b")
        }
        finish [result.left?, result.right?]
        "#,
    )
    .expect("program should parse");
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
