//! Nullish tests and path reads over a *projected* host binding.
//!
//! A session global the host supplies is a `Value::Projected`: a lazy handle the
//! runtime reads through. FIG-1446 made the ECMA coercions read the value behind
//! the handle. Two families were left testing or reading the handle itself, and
//! both answered differently depending on whether the guest took the projected
//! route or a route that happened to materialize first:
//!
//! * `IsNullish` tested `matches!(value, Null | Undefined)` on the wrapper, so a
//!   projected `null` looked present and `missing ?? "fallback"` yielded the
//!   projection (FIG-1479).
//! * The field/index arms read through `ProjectedValue::get_field` /
//!   `get_index`, whose scalar fallback is the dialect-blind `access.rs` read:
//!   `text.length` raised `can't read '.length' from string` while
//!   `text?.length` — which lowers through an optional-chain temporary, and
//!   temporaries materialize — returned 5 (FIG-1482).
//!
//! The rule pinned here: a projected binding is nullish, reads fields, and
//! indexes exactly as the value behind it does, and the projected route agrees
//! with the materializing one. Reads through a *custom* (host-descriptor)
//! projection stay lazy — the descriptor answers the field, nothing is dragged
//! across — which the last test pins.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome,
    ProjectedBindings, ProjectedFuture, ProjectedHostDescriptor, ProjectedReadRequest,
    ProjectedReadResponse, ProjectedValue, RuntimeError, State, Value,
};

/// Supplies the session's projected bindings the way a real host does — through
/// `ExecutionHost::projected_bindings`, so they are read-only bindings rather than
/// ordinary globals holding a handle. That matters for the laziness assertions:
/// a genuine projected binding is not written back at end of turn, so a
/// descriptor's read log contains exactly the reads the guest program caused.
struct Host {
    view: Option<Arc<RecordingView>>,
}

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new(
                "unsupported projected-path ability",
            )),
        }
    }

    fn projected_bindings(&self) -> ProjectedBindings {
        let mut bindings = ProjectedBindings::new();
        for (name, value) in [
            ("text", Value::String("hello".into())),
            // A surrogate pair, so `.length` and indexing have to agree with the
            // dialect's UTF-16 view and not with Rust's chars.
            ("astral", Value::String("a\u{1f600}b".into())),
            ("count", Value::Number(41.0)),
            ("missing", Value::Null),
            (
                "row",
                Value::Record(Arc::new(lashlang::Record::from_iter([
                    ("kind".to_string(), Value::String("tool".into())),
                    ("id".to_string(), Value::Number(7.0)),
                ]))),
            ),
        ] {
            bindings.insert(name, ProjectedValue::scalar(name, value));
        }
        if let Some(view) = &self.view {
            bindings.insert("view", ProjectedValue::custom("view", view.clone()));
        }
        bindings
    }
}

/// A host view that answers reads one at a time and records what it was asked, so
/// a test can tell a lazy field read from a whole-view materialization. It answers
/// only `Field("kind")`; every other request falls to the trait's `Missing`
/// default, which is the shape a minimal descriptor has — the documented example
/// implements `type_name` and nothing else.
struct RecordingView {
    asked: Mutex<Vec<String>>,
}

impl ProjectedHostDescriptor for RecordingView {
    fn type_name(&self) -> &str {
        "RecordingView"
    }

    fn read_one(
        &self,
        request: ProjectedReadRequest,
    ) -> ProjectedFuture<'_, ProjectedReadResponse> {
        let label = match &request {
            ProjectedReadRequest::Field(field) => format!("field:{field}"),
            ProjectedReadRequest::Materialize => "materialize".to_string(),
            other => format!("{other:?}"),
        };
        self.asked
            .lock()
            .expect("recording view log is not poisoned")
            .push(label);
        Box::pin(async move {
            match request {
                ProjectedReadRequest::Field(field) if field.as_ref() == "kind" => {
                    ProjectedReadResponse::Value(Value::String("tool".into()))
                }
                _ => ProjectedReadResponse::Missing,
            }
        })
    }
}

/// Compiles `source` as a cell of a session whose globals are projected host
/// bindings, then runs it. `view` is bound as the `view` global when given.
async fn execute_with_view(
    source: &str,
    view: Option<Arc<RecordingView>>,
) -> Result<ExecutionOutcome, RuntimeError> {
    let mut names = vec![
        "text".to_string(),
        "astral".to_string(),
        "count".to_string(),
        "missing".to_string(),
        "row".to_string(),
    ];
    if view.is_some() {
        names.push("view".to_string());
    }
    let globals = BTreeSet::from_iter(names);
    let program = lash_typescript::parse_with_globals(source, &globals)
        .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"));
    let program =
        lashlang::compile_ast_with_dialect(&program, lashlang::CompilationDialect::Typescript)
            .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"));
    let mut state = State::new();
    lashlang::execute(&program, &mut state, &Host { view }).await
}

async fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    execute_with_view(source, None).await
}

/// The value the cell finished with, with a projected wrapper stripped. A path
/// read over a projected source keeps the wrapper by design
/// (`ProjectedValue::propagate_field`), so these tests assert on the value
/// behind it; `projected_path_reads_stay_projected` pins the wrapper itself.
async fn finished(source: &str) -> Value {
    let outcome = execute(source)
        .await
        .unwrap_or_else(|error| panic!("`{source}` should execute: {error}"));
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("`{source}` should finish: {outcome:?}")
    };
    match value {
        Value::Projected(projected) => projected.materialize(),
        other => other,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_null_is_nullish_for_coalescing() {
    for (source, expected) in [
        (
            r#"finish(missing ?? "fallback");"#,
            Value::String("fallback".into()),
        ),
        (r#"finish(missing ?? 3);"#, Value::Number(3.0)),
        // A present projection is still present: `??` keeps it.
        (
            r#"finish(text ?? "fallback");"#,
            Value::String("hello".into()),
        ),
        (r#"finish(count ?? 3);"#, Value::Number(41.0)),
        // A projected field that is absent is nullish too, because the field read
        // propagates the projection onto its `undefined` result.
        (
            r#"finish(row.nope ?? "fallback");"#,
            Value::String("fallback".into()),
        ),
        // The materializing route and the projected route agree.
        (
            r#"const local = missing; finish(local ?? "fallback");"#,
            Value::String("fallback".into()),
        ),
        (
            r#"finish(missing?.length ?? "fallback");"#,
            Value::String("fallback".into()),
        ),
    ] {
        assert_eq!(finished(source).await, expected, "{source}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_scalars_read_their_dialect_fields() {
    for (source, expected) in [
        // The FIG-1482 shape: plain access errored where optional access worked.
        (r#"finish(text.length);"#, Value::Number(5.0)),
        (r#"finish(text?.length);"#, Value::Number(5.0)),
        (r#"finish(row.kind);"#, Value::String("tool".into())),
        (r#"finish(row.id);"#, Value::Number(7.0)),
        (r#"finish(row?.kind);"#, Value::String("tool".into())),
        // An absent key on a projected record is `undefined`, as it is on the
        // same record unprojected — not the `null` the dialect-blind read gave.
        (r#"finish(row.nope);"#, Value::Undefined),
        (
            r#"finish(typeof row.nope);"#,
            Value::String("undefined".into()),
        ),
    ] {
        assert_eq!(finished(source).await, expected, "{source}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_scalars_index_as_their_value() {
    for (source, expected) in [
        (r#"finish(text[1]);"#, Value::String("e".into())),
        (r#"finish(row["kind"]);"#, Value::String("tool".into())),
        (r#"finish(row["nope"]);"#, Value::Undefined),
        (r#"finish(text[99]);"#, Value::Undefined),
    ] {
        assert_eq!(finished(source).await, expected, "{source}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_records_assign_as_sources() {
    // `Object.assign` with a heap receiver runs before the materializing stdlib
    // dispatch, so a projected source used to match no source shape and be
    // dropped without a word.
    assert_eq!(
        finished(r#"finish(Object.assign({}, row).kind);"#).await,
        Value::String("tool".into())
    );
    assert_eq!(
        finished(r#"finish(Object.keys(Object.assign({}, row)).length);"#).await,
        Value::Number(2.0)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn projected_path_reads_stay_projected() {
    // Reading through a projection does not lose "this came from a projected
    // source": the wrapper the field read propagates is what the host is handed.
    let outcome = execute(r#"finish(row.kind);"#)
        .await
        .expect("a projected field read should execute");
    let ExecutionOutcome::Finished(Value::Projected(projected)) = outcome else {
        panic!("a projected field read should finish with a projected value: {outcome:?}")
    };
    assert_eq!(projected.name(), "row.kind");
    assert_eq!(projected.materialize(), Value::String("tool".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn custom_projection_field_reads_stay_lazy() {
    // Only *scalar* projections resolve through the dialect helpers; a custom
    // projection is a lazy host view, so the descriptor answers the field and the
    // view is never materialized to serve one property.
    let view = recording_view();
    let value = finished_projection(r#"finish(view.kind);"#, &view).await;
    // Asserted before materializing the handle below, which is itself a read.
    assert_eq!(asked(&view), vec!["field:kind".to_string()]);
    assert_eq!(value.materialize(), Value::String("tool".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn custom_projection_is_present_for_coalescing_without_being_read() {
    // A custom projection stands for a live host view, so `??` keeps it. Deciding
    // that by materializing would invert the answer for the ordinary descriptor:
    // `RecordingView` answers only `Field("kind")`, so `Materialize` falls to the
    // trait's `Missing` default, which reads back as `Value::Null` — a present view
    // judging itself absent. It would also be the one read this opcode must never
    // make, since a real view is a whole session's worth of data.
    let view = recording_view();
    let value = finished_projection(r#"finish(view ?? "fallback");"#, &view).await;
    assert_eq!(value.name(), "view");
    assert_eq!(
        asked(&view),
        Vec::<String>::new(),
        "answering `??` must not read the host view at all"
    );

    // The `??` fallback is still reachable through the view: an absent field of it
    // is nullish, and that read is the descriptor's own answer.
    let view = recording_view();
    assert_eq!(
        finished_with_view(r#"finish(view.nope ?? "fallback");"#, &view).await,
        Value::String("fallback".into())
    );
    assert_eq!(asked(&view), vec!["field:nope".to_string()]);
}

#[tokio::test(flavor = "current_thread")]
async fn projected_astral_strings_measure_and_index_as_utf16() {
    // The astral binding is 4 UTF-16 units and 3 chars, so a codepoint-based read
    // would answer 3 here and index the wrong unit.
    assert_eq!(
        finished(r#"finish(astral.length);"#).await,
        Value::Number(4.0)
    );
    assert_eq!(
        finished(r#"finish("a\u{1f600}b".length);"#).await,
        Value::Number(4.0),
        "the projected length must match the same literal unprojected"
    );
    assert_eq!(
        finished(r#"finish(astral[0]);"#).await,
        Value::String("a".into())
    );
    assert_eq!(
        finished(r#"finish(astral[3]);"#).await,
        Value::String("b".into())
    );

    // Indexing into the middle of the pair names a lone surrogate, which this
    // dialect refuses rather than mangle — projected and unprojected alike.
    for source in [r#"finish(astral[1]);"#, r#"finish("a\u{1f600}b"[1]);"#] {
        let error = execute(source)
            .await
            .expect_err("a lone surrogate index should be refused");
        assert!(
            error.to_string().contains("TS_LONE_SURROGATE_UNSUPPORTED"),
            "{source}: {error}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn projected_out_of_range_and_string_keys_agree_with_the_dialect() {
    for (source, expected) in [
        // A negative or non-index key is not an array index, so it is a property
        // lookup that finds nothing.
        (r#"finish(text[-1]);"#, Value::Undefined),
        (r#"finish("hello"[-1]);"#, Value::Undefined),
        (r#"finish(text[1.5]);"#, Value::Undefined),
        // A numeric *string* key is an index, on the projected route too.
        (r#"finish(text["1"]);"#, Value::String("e".into())),
        (r#"finish("hello"["1"]);"#, Value::String("e".into())),
        // Records key on the string form either way, and absent keys are
        // `undefined`, not `null`.
        (r#"finish(row["1"]);"#, Value::Undefined),
        (r#"finish(row[1]);"#, Value::Undefined),
    ] {
        assert_eq!(finished(source).await, expected, "{source}");
    }
}

fn recording_view() -> Arc<RecordingView> {
    Arc::new(RecordingView {
        asked: Mutex::new(Vec::new()),
    })
}

fn asked(view: &Arc<RecordingView>) -> Vec<String> {
    view.asked
        .lock()
        .expect("recording view log is not poisoned")
        .clone()
}

/// Runs `source` against a session carrying `view`, returning the finished value.
async fn finished_with_view(source: &str, view: &Arc<RecordingView>) -> Value {
    let outcome = execute_with_view(source, Some(view.clone()))
        .await
        .unwrap_or_else(|error| panic!("`{source}` should execute: {error}"));
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("`{source}` should finish: {outcome:?}")
    };
    match value {
        Value::Projected(projected) => projected.materialize(),
        other => other,
    }
}

/// As `finished_with_view`, but keeps the projection the cell finished with — the
/// point of the tests that assert *which* handle came back, and how little was read
/// to produce it.
async fn finished_projection(source: &str, view: &Arc<RecordingView>) -> ProjectedValue {
    let outcome = execute_with_view(source, Some(view.clone()))
        .await
        .unwrap_or_else(|error| panic!("`{source}` should execute: {error}"));
    let ExecutionOutcome::Finished(Value::Projected(value)) = outcome else {
        panic!("`{source}` should finish with a projection: {outcome:?}")
    };
    value
}
