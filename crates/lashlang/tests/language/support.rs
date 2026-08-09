fn expect_string<'a>(args: &'a Record, key: &str) -> Result<&'a str, ExecutionHostError> {
    match args.get(key) {
        Some(Value::String(value)) => Ok(value),
        _ => Err(ExecutionHostError::new(format!(
            "missing string arg: {key}"
        ))),
    }
}

// ------------------------------------------------------------------
//  End-to-end Type literal integration tests
// ------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn end_to_end_type_value_is_json_schema_shaped() {
    let program = parse(
        r#"
        Books = Type {
          title: str,
          genre: enum["fiction", "non-fiction"],
          tags: list[str],
          meta: Type {
            pages: int,
            published: int
          },
          isbn: str?
        }
        finish Books
        "#,
    )
    .expect("should parse");
    let host = TestHost::default();
    let mut state = State::new();
    let outcome = lashlang::execute(&program, &mut state, &host)
        .await
        .expect("should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    let schema = lashlang::unwrap_type_value(&value)
        .and_then(Value::as_record)
        .expect("wrapped type");
    assert_eq!(schema["type"], Value::String("object".into()));
    let required = match &schema["required"] {
        Value::List(items) => items,
        _ => panic!("required must be list"),
    };
    // isbn is optional → 4 required
    assert_eq!(required.len(), 4);
}

#[tokio::test(flavor = "current_thread")]
async fn type_is_usable_as_a_tool_call_argument() {
    #[derive(Default)]
    struct CaptureHost {
        captured: std::sync::Mutex<Option<Value>>,
    }
    impl ExecutionHost for CaptureHost {
        async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
            match op {
                AbilityOp::ResourceOperation(operation) => {
                    *self.captured.lock_recover() = operation
                        .args
                        .first()
                        .and_then(Value::as_record)
                        .and_then(|record| record.get("output"))
                        .cloned();
                    Ok(AbilityResult::Value(Value::Null))
                }
                AbilityOp::Finish(value) | AbilityOp::Fail(value) => {
                    Ok(AbilityResult::Value(value))
                }
                _ => Err(ExecutionHostError::new("unsupported host ability")),
            }
        }
    }
    let host = CaptureHost::default();
    execute(
        r#"
        Shape = Type { name: str, labels: list[enum["a","b"]] }
        await agents.spawn({ task: "find X", output: Shape })
        finish null
        "#,
        &mut State::new(),
        &host,
    )
    .await
    .expect("should run");
    let captured = host.captured.lock_recover().clone().expect("captured arg");
    let inner = lashlang::unwrap_type_value(&captured).expect("wrapped type");
    let schema = inner.as_record().expect("schema record");
    assert_eq!(schema["type"], Value::String("object".into()));
    let props = schema["properties"].as_record().unwrap();
    let labels = props["labels"].as_record().unwrap();
    assert_eq!(labels["type"], Value::String("array".into()));
    let items = labels["items"].as_record().unwrap();
    let enum_values = match &items["enum"] {
        Value::List(items) => items,
        _ => panic!("enum should be list"),
    };
    assert_eq!(enum_values.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn validate_reuses_type_literals_for_intermediate_checks() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            raw = {
              name: "lashlang",
              version: "0.2.61",
              labels: ["agent", "runtime"]
            }
            package = validate(raw, Type {
              name: str,
              version: str,
              labels: list[str]
            })
            finish package
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("validate should succeed"),
    );
    let package = value.as_record().expect("package record");
    assert_eq!(
        package["name"],
        Value::String("lashlang".to_string().into())
    );

    let mut state = State::new();
    let err = execute(
        r#"
        finish validate(
          { name: "lashlang", labels: ["agent", 42] },
          Type { name: str, labels: list[str] }
        )
        "#,
        &mut state,
        &host,
    )
    .await
    .expect_err("validate should fail");
    let ExecuteError::Runtime(RuntimeError::ValidationFailed { reason }) = err else {
        panic!("expected validation runtime error");
    };
    assert!(
        reason.contains("$.labels[1]: expected string, got number"),
        "{reason}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn undefined_ref_in_type_produces_runtime_error() {
    let program = parse("finish Type { inner: Missing }").expect("should parse");
    let host = TestHost::default();
    let mut state = State::new();
    let err = lashlang::execute(&program, &mut state, &host)
        .await
        .expect_err("Missing is undefined");
    assert!(matches!(err, RuntimeError::UndefinedVariable { .. }));
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_round_trip_preserves_type_values() {
    let program = parse(
        r#"
        Books = Type { title: str, count: int }
        finish Books
        "#,
    )
    .expect("should parse");
    let host = TestHost::default();
    let mut state = State::new();
    let outcome = lashlang::execute(&program, &mut state, &host)
        .await
        .expect("should run");
    let ExecutionOutcome::Finished(value) = outcome else {
        panic!("expected finish");
    };
    let snapshot = state.snapshot();
    let serialized = snapshot.to_canonical_bytes().expect("serialize");
    let restored = lashlang::Snapshot::from_canonical_bytes(&serialized).expect("deserialize");
    let restored_state = State::from_snapshot(restored);
    // Re-execute a program that references Books — the ref should still resolve.
    let program2 = parse("finish Books").expect("parse");
    let mut state2 = restored_state;
    let outcome2 = lashlang::execute(&program2, &mut state2, &host)
        .await
        .expect("run");
    let ExecutionOutcome::Finished(v2) = outcome2 else {
        panic!("expected finish");
    };
    assert_eq!(value, v2);
}
