use lash_sansio::sync::MutexExt;
use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, Record,
    RuntimeError, State, TypeExpr, Value, parse,
};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[derive(Default)]
struct TestHost {
    files: HashMap<String, String>,
    globs: HashMap<String, Vec<String>>,
    observations: std::sync::Mutex<Vec<Value>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl TestHost {
    fn with_file(mut self, path: &str, content: &str) -> Self {
        self.files.insert(path.to_string(), content.to_string());
        self
    }
}

impl ExecutionHost for TestHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => self
                .perform_resource_operation(operation)
                .await
                .map(AbilityResult::Value),
            AbilityOp::ResourceOperationBatch(batch) => {
                let results = futures::future::join_all(
                    batch
                        .operations
                        .into_iter()
                        .map(|operation| self.perform_resource_operation(operation)),
                )
                .await
                .into_iter()
                .map(lashlang::ResourceOperationResult::from_result)
                .collect();
                Ok(AbilityResult::ResourceOperationBatch(
                    lashlang::ResourceOperationBatchResult::settled_in_input_order(results),
                ))
            }
            AbilityOp::StartProcess(start) => self
                .call_tool(&start.process_name, &start.args)
                .await
                .map(AbilityResult::Value),
            AbilityOp::Await(handle) => Ok(AbilityResult::Value(handle)),
            AbilityOp::Print(value) => {
                self.observations.lock_recover().push(value);
                Ok(AbilityResult::Unit)
            }
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

impl TestHost {
    async fn perform_resource_operation(
        &self,
        operation: lashlang::ResourceOperation,
    ) -> Result<Value, ExecutionHostError> {
        let empty = Record::new();
        let args = operation
            .args
            .first()
            .and_then(Value::as_record)
            .map_or(&empty, |record| record);
        let name = test_host_operation(&operation)?;
        self.call_tool(&name, args).await
    }

    async fn call_tool(&self, name: &str, args: &Record) -> Result<Value, ExecutionHostError> {
        match name {
            "read_file" => {
                let path = expect_string(args, "path")?;
                match self.files.get(path) {
                    Some(content) => Ok(Value::String(content.clone().into())),
                    None => Err(ExecutionHostError::new(format!("missing file: {path}"))),
                }
            }
            "glob" => {
                let pattern = expect_string(args, "pattern")?;
                let values: Vec<_> = self
                    .globs
                    .get(pattern)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|value| Value::String(value.into()))
                    .collect();
                Ok(Value::List(values.into()))
            }
            "sleep_echo" => {
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                loop {
                    let max = self.max_active.load(Ordering::SeqCst);
                    if active <= max {
                        break;
                    }
                    if self
                        .max_active
                        .compare_exchange(max, active, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(Value::String(
                    expect_string(args, "value")?.to_string().into(),
                ))
            }
            _ => Err(ExecutionHostError::new(format!("unknown tool: {name}"))),
        }
    }
}

fn test_host_operation(
    operation: &lashlang::ResourceOperation,
) -> Result<String, ExecutionHostError> {
    match &operation.receiver {
        Value::Resource(receiver) => test_host_environment()
            .resources
            .resolve_module_operation(
                &receiver.resource_type,
                &receiver.alias,
                &operation.operation,
            )
            .map(|binding| binding.host_operation.clone())
            .ok_or_else(|| {
                ExecutionHostError::new(format!(
                    "module `{}` of type `{}` does not expose operation `{}`",
                    receiver.alias, receiver.resource_type, operation.operation
                ))
            }),
        _ => Ok(operation.operation.clone()),
    }
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
    Runtime(#[from] RuntimeError),
}

async fn execute<H: ExecutionHost>(
    source: &str,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, ExecuteError> {
    let program = parse(source)?;
    let compiled = if let Ok(linked) =
        lashlang::LinkedModule::link(program.clone(), test_host_environment())
    {
        lashlang::compile_linked(&linked)
    } else if program_contains_start_process(&program.main) {
        let linked = lashlang::LinkedModule::link(program, test_host_environment())?;
        lashlang::compile_linked(&linked)
    } else {
        lashlang::compile(source)?
    };
    lashlang::execute(&compiled, state, host)
        .await
        .map_err(ExecuteError::Runtime)
}

fn test_host_environment() -> lashlang::LashlangHostEnvironment {
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["files"],
            "Files",
            "read",
            "read_file",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["files"],
            "Files",
            "glob",
            "glob",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["agents"],
            "Agents",
            "spawn",
            "spawn_agent",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "sleep_echo",
            "sleep_echo",
            TypeExpr::Any,
            TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    lashlang::LashlangHostEnvironment::new(resources, lashlang::LashlangAbilities::all())
}

fn program_contains_start_process(expr: &lashlang::Expr) -> bool {
    struct Finder(bool);

    impl lashlang::ExprVisitor for Finder {
        fn visit_expr(&mut self, expr: &lashlang::Expr) {
            if matches!(expr, lashlang::Expr::StartProcess(_)) {
                self.0 = true;
                return;
            }
            lashlang::walk_expr(self, expr);
        }
    }

    let mut finder = Finder(false);
    lashlang::ExprVisitor::visit_expr(&mut finder, expr);
    finder.0
}

fn program_len(program: &lashlang::Program) -> usize {
    match &program.main {
        lashlang::Expr::Block(expressions) => expressions.len(),
        _ => 1,
    }
}

async fn runtime_error(source: &str) -> RuntimeError {
    let host = TestHost::default();
    let mut state = State::new();
    match execute(source, &mut state, &host)
        .await
        .expect_err("execution should fail")
    {
        ExecuteError::Runtime(error) => error,
        ExecuteError::Parse(error) => panic!("expected runtime error, got parse error: {error:?}"),
        ExecuteError::Link(error) => panic!("expected runtime error, got link error: {error:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn parser_handles_precedence_and_await_record() {
    let program = parse(
        r#"
        total = 1 + 2 * 3
        process read(pattern: str, path: str) {
          finish { pattern: pattern, path: path }
        }
        fanout = await {
          left: start read(pattern: "src/*.rs", path: ""),
          right: start read(pattern: "", path: "src/lib.rs")
        }
        finish total
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_double_slash_comments() {
    let program = parse(
        r#"
        // setup
        total = 1 + 2
        // finish
        finish total
        "#,
    )
    .expect("program should parse");

    let lashlang::Expr::Block(expressions) = &program.main else {
        panic!("program should be a block");
    };
    assert_eq!(expressions.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_semicolons_as_statement_separators() {
    let program = parse(
        r#"
        x = 1; y = 2;
        finish x;
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn start_is_contextual_not_reserved() {
    let host = TestHost::default().with_file("a.txt", "async");
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            process read_file(path: str) { finish path }
            start = 1
            for start in range(3) {
              last = start
            }
            rec = { start: last }
            h = start read_file(path: "a.txt")
            finish { value: start, field: rec.start, awaited: (await h)? }
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("contextual start program should run"),
    );

    let record = value.as_record().expect("record");
    assert_eq!(record["value"], Value::Number(1.0));
    assert_eq!(record["field"], Value::Number(2.0));
    assert_eq!(record["awaited"], Value::String("async".to_string().into()));

    let err = runtime_error("finish start()").await;
    assert!(matches!(err, RuntimeError::UnknownBuiltin { name } if name == "start"));
}

#[tokio::test(flavor = "current_thread")]
async fn range_supports_python_style_steps() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            stepped = []
            for i in range(5, 0, -2) {
              stepped = push(stepped, i)
            }
            finish {
              up: range(0, 5, 2),
              down: range(5, 0, -2),
              empty_up: range(5, 0, 2),
              empty_down: range(0, 5, -2),
              iterated: stepped
            }
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("stepped ranges should run"),
    );

    let record = value.as_record().expect("record");
    assert_eq!(
        record["up"],
        Value::List(vec![Value::Number(0.0), Value::Number(2.0), Value::Number(4.0)].into())
    );
    assert_eq!(
        record["down"],
        Value::List(vec![Value::Number(5.0), Value::Number(3.0), Value::Number(1.0)].into())
    );
    assert_eq!(record["empty_up"], Value::List(Vec::new().into()));
    assert_eq!(record["empty_down"], Value::List(Vec::new().into()));
    assert_eq!(record["iterated"], record["down"]);
}

#[tokio::test(flavor = "current_thread")]
async fn integer_division_helpers_use_mathematical_rounding() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            items = range(10)
            stride = ceil_div(len(items), 3)
            starts = []
            for i in range(0, len(items), stride) {
              starts = push(starts, i)
            }
            finish {
              ceil_pos: ceil_div(10, 3),
              floor_pos: floor_div(10, 3),
              ceil_neg: ceil_div(-10, 3),
              floor_neg: floor_div(-10, 3),
              starts: starts
            }
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("division helpers should run"),
    );

    let record = value.as_record().expect("record");
    assert_eq!(record["ceil_pos"], Value::Number(4.0));
    assert_eq!(record["floor_pos"], Value::Number(3.0));
    assert_eq!(record["ceil_neg"], Value::Number(-3.0));
    assert_eq!(record["floor_neg"], Value::Number(-4.0));
    assert_eq!(
        record["starts"],
        Value::List(vec![Value::Number(0.0), Value::Number(4.0), Value::Number(8.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn numeric_helper_errors_are_rejected() {
    for source in [
        "finish range(0, 5, 0)",
        "finish range(0, 5, 1.5)",
        "finish range(1000001, 0, -1)",
        "finish ceil_div(1.5, 1)",
        "finish floor_div(1, 0)",
        "finish ceil_div(\"1\", 1)",
    ] {
        let err = runtime_error(source).await;
        assert!(matches!(
            err,
            RuntimeError::ZeroRangeStep
                | RuntimeError::InvalidRangeBound
                | RuntimeError::InvalidRangeBoundType { .. }
                | RuntimeError::RangeTooLarge { .. }
                | RuntimeError::InvalidIntegerDivisionArgument { .. }
                | RuntimeError::InvalidIntegerDivisionArgumentType { .. }
                | RuntimeError::IntegerDivisionByZero { .. }
        ));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_trailing_semicolon_after_raw_string() {
    let program = parse(
        r#"
        msg = r"hello";
        finish msg
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_treats_semicolon_like_whitespace_between_idents() {
    let with_semi = parse("x = 1;y = 2").expect("semicolon-separated should parse");
    let with_newline = parse("x = 1\ny = 2").expect("newline-separated should parse");
    assert_eq!(program_len(&with_semi), program_len(&with_newline));
    assert_eq!(program_len(&with_semi), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn tuple_comma_expressions_are_first_class_sequence_values() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            pair = 1, "x"
            singleton = (1,)
            empty_tuple = ()
            sliced = slice((1, 2, 3), 1, 3)
            seen = []
            for item in pair {
              seen = push(seen, to_string(item))
            }
            print pair
            finish {
              first: pair[0],
              second: pair[1],
              len_pair: len(pair),
              len_singleton: len(singleton),
              empty_empty: empty(empty_tuple),
              empty_pair: empty(pair),
              contains_x: contains(pair, "x"),
              joined: join(("a", "b"), "|"),
              slice_first: sliced[0],
              slice_len: len(sliced),
              truthy_pair: pair ? true : false,
              truthy_empty: empty_tuple ? true : false,
              concat_last: ((1,) + (2,))[1],
              seen: seen,
              tuple_eq: (1, 2) == (1, 2),
              tuple_not_list: (1, 2) == [1, 2],
              text: to_string((1, "x"))
            }
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("tuple program should run"),
    );

    let record = value.as_record().expect("record");
    assert_eq!(record["first"], Value::Number(1.0));
    assert_eq!(record["second"], Value::String("x".into()));
    assert_eq!(record["len_pair"], Value::Number(2.0));
    assert_eq!(record["len_singleton"], Value::Number(1.0));
    assert_eq!(record["empty_empty"], Value::Bool(true));
    assert_eq!(record["empty_pair"], Value::Bool(false));
    assert_eq!(record["contains_x"], Value::Bool(true));
    assert_eq!(record["joined"], Value::String("a|b".into()));
    assert_eq!(record["slice_first"], Value::Number(2.0));
    assert_eq!(record["slice_len"], Value::Number(2.0));
    assert_eq!(record["truthy_pair"], Value::Bool(true));
    assert_eq!(record["truthy_empty"], Value::Bool(false));
    assert_eq!(record["concat_last"], Value::Number(2.0));
    assert_eq!(
        record["seen"],
        Value::List(vec![Value::String("1".into()), Value::String("x".into())].into())
    );
    assert_eq!(record["tuple_eq"], Value::Bool(true));
    assert_eq!(record["tuple_not_list"], Value::Bool(false));
    assert_eq!(record["text"], Value::String(r#"(1, "x")"#.into()));

    let observations = host.observations.lock_recover();
    assert!(matches!(&observations[0], Value::Tuple(items) if items.len() == 2));
}

#[tokio::test(flavor = "current_thread")]
async fn tuples_are_immutable_and_do_not_mixed_concat_with_lists() {
    let err = runtime_error(
        r#"
        pair = (1, 2)
        pair[0] = 9
        finish pair
        "#,
    )
    .await;
    assert!(err.to_string().contains("tuples are immutable"), "{err:?}");

    let err = runtime_error("finish push((1, 2), 3)").await;
    assert!(
        err.to_string()
            .contains("`push` requires a list as the first argument"),
        "{err:?}"
    );

    let err = runtime_error("finish (1,) + [2]").await;
    assert!(
        err.to_string().contains("can't concatenate list and tuple"),
        "{err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tuple_snapshot_round_trip_preserves_tuple_identity() {
    let host = TestHost::default();
    let mut state = State::new();
    let outcome = execute("pair = 1, 2", &mut state, &host)
        .await
        .expect("tuple assignment should run");
    assert!(matches!(outcome, ExecutionOutcome::Continued));

    let encoded = state
        .snapshot()
        .to_canonical_bytes()
        .expect("snapshot encode");
    let snapshot = lashlang::Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");
    let restored = State::from_snapshot(snapshot);
    assert!(matches!(restored.globals()["pair"], Value::Tuple(_)));
}

#[tokio::test(flavor = "current_thread")]
async fn multiline_strings_are_expression_values() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r####"
            finish """first\n"quoted"
second"""
            "####,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(value, Value::String("first\n\"quoted\"\nsecond".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn single_quoted_strings_are_expression_values() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            finish 'it\'s ready\n'
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(value, Value::String("it's ready\n".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn triple_single_strings_are_expression_values() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            finish '''first\n'second'
third'''
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(value, Value::String("first\n'second'\nthird".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn raw_single_and_double_strings_preserve_backslashes() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            finish [r"path\to\file", R'\n stays raw']
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("path\\to\\file".into()),
                Value::String("\\n stays raw".into())
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_exact_shell_exec_date_command_string() {
    let program = parse(
        r#"
        now = await shell.exec({ cmd: "date '+%Y-%m-%d %H:%M:%S %Z (%z)'" })?
        finish now
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn record_strings_preserve_shell_date_command_text() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            finish { cmd: "date '+%Y-%m-%d %H:%M:%S %Z (%z)'" }
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(
        record["cmd"],
        Value::String("date '+%Y-%m-%d %H:%M:%S %Z (%z)'".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn string_literals_cover_shell_quotes_formats_and_raw_forms() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#####"
            finish [
              "date '+%Y-%m-%d %H:%M:%S %Z (%z)'",
              'printf "%s\\n" "$value"',
              "json: {\"cmd\":\"echo 'ok'\"}",
              "// not a comment # also not a comment",
              "${HOME:-/tmp} && echo %done",
              r"C:\Users\sam\file.txt",
              R'\n stays slash-n',
              """line "quoted" and 'single'
next""",
              '''line "double" and 'single'
next''',
              r"""python3 - <<'PY'
print("ok")
PY"""
            ]
            "#####,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("date '+%Y-%m-%d %H:%M:%S %Z (%z)'".into()),
                Value::String("printf \"%s\\n\" \"$value\"".into()),
                Value::String("json: {\"cmd\":\"echo 'ok'\"}".into()),
                Value::String("// not a comment # also not a comment".into()),
                Value::String("${HOME:-/tmp} && echo %done".into()),
                Value::String("C:\\Users\\sam\\file.txt".into()),
                Value::String("\\n stays slash-n".into()),
                Value::String("line \"quoted\" and 'single'\nnext".into()),
                Value::String("line \"double\" and 'single'\nnext".into()),
                Value::String("python3 - <<'PY'\nprint(\"ok\")\nPY".into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn strings_preserve_utf8_content() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#"
            finish "Grüße 東京"
            "#,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(value, Value::String("Grüße 東京".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn raw_triple_strings_preserve_patch_text() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r####"
            patch = r"""*** Begin Patch
*** Update File: src/lib.rs
@@
-old
+new
\n { braces stay raw }
*** End Patch"""
            finish patch
            "####,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(
        value,
        Value::String(
            "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-old\n+new\n\\n { braces stay raw }\n*** End Patch"
                .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn raw_triple_strings_preserve_script_text() {
    let host = TestHost::default();
    let mut state = State::new();
    let value = finished(
        execute(
            r#####"
            script = r'''python3 - <<'PY'
print("""hello""")
\n { braces stay raw }
PY'''
            finish script
            "#####,
            &mut state,
            &host,
        )
        .await
        .expect("program should run"),
    );

    assert_eq!(
        value,
        Value::String(
            "python3 - <<'PY'\nprint(\"\"\"hello\"\"\")\n\\n { braces stay raw }\nPY".into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn rust_style_raw_strings_are_not_valid_lashlang_strings() {
    let err = runtime_error(r####"finish r#"hello"#"####).await;
    assert!(
        format!("{err}").contains("unknown name `r`"),
        "old raw syntax should not lex as a string, got {err}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_comment_only_program() {
    let program = parse(
        r#"
        // comment one
        // comment two
        "#,
    )
    .expect("program should parse");

    assert!(program_len(&program) == 0);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_inline_trailing_comments_in_blocks() {
    let program = parse(
        r#"
        if true { // enter block
          value = 1 // assign
        } else { // fallback
          value = 2
        }
        finish value // done
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_else_if_chains() {
    let program = parse(
        r#"
        if false {
          answer = 1
        } else if true {
          answer = 2
        } else {
          answer = 3
        }
        finish answer
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_allows_await_record_in_expression_position() {
    let program = parse(
        r#"
        results = await {
          left: start glob(pattern: "src/*.rs"),
          right: start read_file(path: "src/lib.rs")
        }
        finish results
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn parser_allows_bare_expression_statements() {
    let program = parse(
        r#"
        "branch_a"
        finish "done"
        "#,
    )
    .expect("program should parse");

    let lashlang::Expr::Block(expressions) = &program.main else {
        panic!("program should be a block");
    };
    assert_eq!(expressions.len(), 2);
    assert!(matches!(expressions[0], lashlang::Expr::String(_)));
}

#[tokio::test(flavor = "current_thread")]
async fn parser_allows_finish_null_at_the_end_of_a_block_or_program() {
    let program = parse(
        r#"
        if true {
          finish null
        }
        finish null
        "#,
    )
    .expect("program should parse");

    let lashlang::Expr::Block(expressions) = &program.main else {
        panic!("program should be a block");
    };
    assert!(matches!(
        expressions.as_slice(),
        [
            lashlang::Expr::If { then_block, .. },
            lashlang::Expr::Finish(_)
        ] if matches!(then_block.as_ref(), lashlang::Expr::Block(items) if matches!(items.as_slice(), [lashlang::Expr::Finish(_)]))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn executes_programs_with_double_slash_comments() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        // Create some values first
        total = 6 / 2
        // Return the result
        finish total
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::Number(3.0));
}

#[tokio::test(flavor = "current_thread")]
async fn bare_finish_requires_value() {
    let host = TestHost::default();
    let mut state = State::new();

    let err = execute("finish", &mut state, &host)
        .await
        .expect_err("bare finish should be rejected");
    assert_eq!(
        err.to_string(),
        "`finish` requires a value; use `finish null` to finish with null"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn executes_inline_trailing_comments_inside_blocks() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        if true { // choose this branch
          total = 1 + 2 // add
        } else {
          total = 0
        }
        finish total // final answer
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::Number(3.0));
}

#[tokio::test(flavor = "current_thread")]
async fn double_slash_inside_strings_is_not_a_comment() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        url = "https://example.com/a//b"
        finish url
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::String("https://example.com/a//b".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn parser_accepts_ternary_in_call_arguments() {
    let program = parse(
        r#"
        result = format("{}", true ? "yes" : "no")
        finish result
        "#,
    )
    .expect("program should parse");

    assert_eq!(program_len(&program), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn executes_arithmetic_strings_and_finish() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        total = 1 + 2 * 3
        msg = format("total={}", total)
        finish msg
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("total=7".to_string().into()));
    assert_eq!(state.globals()["total"], Value::Number(7.0));
}

#[tokio::test(flavor = "current_thread")]
async fn executes_if_for_and_list_concat() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        nums = [1, 2, 3, 4]
        sum = 0
        labels = []
        for n in nums {
          sum = sum + n
          labels = labels + [format("n={}", n)]
        }
        if sum == 10 {
          result = join(labels, ",")
        } else {
          result = "bad"
        }
        finish result
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("n=1,n=2,n=3,n=4".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn list_comprehension_builds_filtered_lists_without_clobbering_outer_bindings() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        n = "outer"
        doubled = [n * 2 for n in [1, 2, 3, 4] if n % 2 == 0]
        finish { doubled: doubled, n: n }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let record = value.as_record().expect("expected record");
    assert_eq!(
        record["doubled"],
        Value::List(vec![Value::Number(4.0), Value::Number(8.0)].into())
    );
    assert_eq!(record["n"], Value::String("outer".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn list_comprehension_nested_clauses_preserve_python_ordering() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        pairs = [format("{}:{}", a, b) for a in ["x", "y"] for b in range(0, 3) if b != 1]
        finish pairs
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("x:0".into()),
                Value::String("x:2".into()),
                Value::String("y:0".into()),
                Value::String("y:2".into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn list_comprehension_allows_effectful_iterables_filters_and_elements() {
    let host = TestHost::default()
        .with_file("Cargo.toml", "abc")
        .with_file("README.md", "");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        paths = ["Cargo.toml", "README.md"]
        sizes = [
          len(await files.read({ path: path })?)
          for path in paths
          if len(await files.read({ path: path })?) > 0
        ]
        finish sizes
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::List(vec![Value::Number(3.0)].into()));
}

#[tokio::test(flavor = "current_thread")]
async fn break_exits_loop_and_restores_loop_binding() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        item = "outer"
        seen = []
        for item in [1, 2, 3] {
          if item == 2 {
            break
          }
          seen = seen + [item]
        }
        finish { seen: seen, item: item }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let record = value.as_record().expect("expected record");
    assert_eq!(record["seen"], Value::List(vec![Value::Number(1.0)].into()));
    assert_eq!(record["item"], Value::String("outer".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn continue_skips_to_next_iteration() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        seen = []
        for n in [1, 2, 3, 4] {
          if n == 2 {
            continue
          }
          seen = seen + [n]
        }
        finish seen
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::Number(1.0), Value::Number(3.0), Value::Number(4.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn while_loop_runs_until_condition_is_false() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        n = 0
        seen = []
        while n < 4 {
          seen = seen + [n]
          n = n + 1
        }
        finish { n: n, seen: seen }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("while loop should run"),
    );

    let record = value.as_record().expect("expected record");
    assert_eq!(record["n"], Value::Number(4.0));
    assert_eq!(
        record["seen"],
        Value::List(
            vec![
                Value::Number(0.0),
                Value::Number(1.0),
                Value::Number(2.0),
                Value::Number(3.0),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn break_exits_while_loop() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        n = 0
        while true {
          n = n + 1
          if n == 3 {
            break
          }
        }
        finish n
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("break should exit while"),
    );

    assert_eq!(value, Value::Number(3.0));
}

#[tokio::test(flavor = "current_thread")]
async fn continue_skips_to_next_while_condition() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        n = 0
        seen = []
        while n < 5 {
          n = n + 1
          if n == 2 {
            continue
          }
          if n == 4 {
            continue
          }
          seen = seen + [n]
        }
        finish seen
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("continue should jump to while condition"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::Number(1.0), Value::Number(3.0), Value::Number(5.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_loop_control_targets_nearest_loop() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        seen = []
        for outer in [1, 2] {
          for inner in [1, 2, 3] {
            if inner == 2 {
              continue
            }
            if inner == 3 {
              break
            }
            seen = seen + [format("{}:{}", outer, inner)]
          }
          seen = seen + [format("outer={}", outer)]
        }
        finish seen
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("1:1".to_string().into()),
                Value::String("outer=1".to_string().into()),
                Value::String("2:1".to_string().into()),
                Value::String("outer=2".to_string().into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_for_and_while_loop_control_targets_nearest_loop() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        seen = []
        for outer in [1, 2] {
          inner = 0
          while inner < 3 {
            inner = inner + 1
            if inner == 2 {
              continue
            }
            if inner == 3 {
              break
            }
            seen = seen + [format("{}:{}", outer, inner)]
          }
          seen = seen + [format("outer={}", outer)]
        }
        finish seen
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("nested for/while control should run"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("1:1".to_string().into()),
                Value::String("outer=1".to_string().into()),
                Value::String("2:1".to_string().into()),
                Value::String("outer=2".to_string().into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn finish_inside_loop_still_terminates_program() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        for n in [1, 2, 3] {
          finish n
        }
        finish 99
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::Number(1.0));
}

#[tokio::test(flavor = "current_thread")]
async fn ternary_selects_the_correct_branch() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        truthy = true ? "left" : "right"
        falsy = false ? "left" : "right"
        finish format("{}:{}", truthy, falsy)
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("left:right".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn ternary_is_right_associative() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        result = false ? 1 : true ? 2 : 3
        finish result
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::Number(2.0));
}

#[tokio::test(flavor = "current_thread")]
async fn ternary_has_lower_precedence_than_boolean_ops() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        result = false or true ? "yes" : "no"
        finish result
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("yes".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn ternary_short_circuits_unselected_branch() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        yes = true ? "ok" : missing_name
        no = false ? missing_name : "ok"
        finish format("{}:{}", yes, no)
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("ok:ok".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn unary_bang_aliases_not() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        a = !false
        b = !true
        finish [a, b]
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::Bool(true), Value::Bool(false)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn symbolic_boolean_aliases_match_word_operators() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        a = true && false
        b = false || true
        c = !false && (false || true)
        finish [a, b, c]
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::Bool(false), Value::Bool(true), Value::Bool(true)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn conditions_and_ternary_use_bounded_truthiness() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        a = 1 ? "yes" : "no"
        b = "" ? "yes" : "no"
        c = !0
        d = ![]
        finish [a, b, c, d]
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::String("yes".to_string().into()),
                Value::String("no".to_string().into()),
                Value::Bool(true),
                Value::Bool(false),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn string_concatenation_stringifies_non_string_side() {
    let host = TestHost::default().with_file("src/lib.rs", "pub fn main() {}");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        found = await files.read({ path: "src/lib.rs" })
        finish "status=" + found.ok + " value=" + found.value
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::String("status=true value=pub fn main() {}".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn arithmetic_and_string_builtins_coerce_scalars() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        total = true + 2
        scaled = "3" * 2
        joined = join(["a", 2, true], "-")
        split_num = split(101, 0)
        prefix = starts_with(123, 12)
        finish {
          total: total,
          scaled: scaled,
          joined: joined,
          split_num: split_num,
          prefix: prefix
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let record = value.as_record().expect("expected record");
    assert_eq!(record["total"], Value::Number(3.0));
    assert_eq!(record["scaled"], Value::Number(6.0));
    assert_eq!(
        record["joined"],
        Value::String("a-2-true".to_string().into())
    );
    assert_eq!(
        record["split_num"],
        Value::List(
            vec![
                Value::String("1".to_string().into()),
                Value::String("1".to_string().into())
            ]
            .into()
        )
    );
    assert_eq!(record["prefix"], Value::Bool(true));
}

#[tokio::test(flavor = "current_thread")]
async fn to_string_stringifies_records() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish to_string({ ok: true, count: 2 })
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::String("{\"count\":2,\"ok\":true}".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn observe_captures_intermediate_values_without_ending_execution() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        item = { ok: true, count: 2 }
        print item
        print "step done"
        finish "final"
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("final".to_string().into()));
    let observed = host.observations.lock_recover();
    assert_eq!(observed.len(), 2);
    assert_eq!(
        observed[0],
        Value::Record({
            let mut record = Record::default();
            record.insert("ok".to_string(), Value::Bool(true));
            record.insert("count".to_string(), Value::Number(2.0));
            record.into()
        })
    );
    assert_eq!(observed[1], Value::String("step done".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn execution_can_continue_without_finish() {
    let host = TestHost::default();
    let mut state = State::new();

    let outcome = execute(
        r#"
        counter = 1
        print counter
        "#,
        &mut state,
        &host,
    )
    .await
    .expect("execution should succeed");

    assert_eq!(outcome, ExecutionOutcome::Continued);
    assert_eq!(state.globals()["counter"], Value::Number(1.0));
    let observed = host.observations.lock_recover();
    assert_eq!(observed.as_slice(), &[Value::Number(1.0)]);
}

#[tokio::test(flavor = "current_thread")]
async fn ternary_fixes_tool_result_formatting_pattern() {
    let host = TestHost::default().with_file("src/lib.rs", "pub fn main() {}");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        found = await files.read({ path: "src/lib.rs" })
        missing = await files.read({ path: "src/missing.rs" })
        summary = format(
          "found={} missing={}",
          found.ok ? "ok" : format("failed: {}", found.error),
          missing.ok ? "ok" : format("failed: {}", missing.error)
        )
        finish summary
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::String(text) = value else {
        panic!("expected string");
    };
    assert!(text.contains("found=ok"));
    assert!(text.contains("missing=failed:"));
}

#[tokio::test(flavor = "current_thread")]
async fn format_supports_indexed_reordering() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish format("b={1} a={0}", "x", "y")
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("b=y a=x".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn format_without_placeholders_returns_literal_string() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish format("plain")
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("plain".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn format_supports_escaped_braces() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish format("{{{}}}", 1)
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("{1}".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn format_accepts_multiline_markdown_string_templates() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r####"
        finish format("""## Installed {0}

`{1}` is installed and available.

Tail:
{2}""", "cargo-machete", "cargo machete", "ok")
        "####,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::String(
            "## Installed cargo-machete\n\n`cargo machete` is installed and available.\n\nTail:\nok"
                .to_string()
                .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn format_accepts_raw_markdown_templates_with_literal_braces() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r####"
        finish format(r"""## {0}

```json
{{"status":"{1}","ok":true}}
```

Output:
{2}""", "cargo-machete", "installed", "ready")
        "####,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::String(
            "## cargo-machete\n\n```json\n{\"status\":\"installed\",\"ok\":true}\n```\n\nOutput:\nready"
                .to_string()
                .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn format_rejects_mixed_placeholder_styles_end_to_end() {
    let error = runtime_error(
        r#"
        finish format("{} {1}", "x", "y")
        "#,
    )
    .await;

    assert_eq!(
        error,
        RuntimeError::Format(lashlang::FormatError::MixedPlaceholderKinds)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn format_rejects_unused_args_end_to_end() {
    let error = runtime_error(
        r#"
        finish format("plain", 1)
        "#,
    )
    .await;

    assert_eq!(
        error,
        RuntimeError::Format(lashlang::FormatError::UnusedArgument { index: 0 })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn format_rejects_unmatched_braces_end_to_end() {
    let open_error = runtime_error(
        r#"
        finish format("{")
        "#,
    )
    .await;
    assert_eq!(
        open_error,
        RuntimeError::Format(lashlang::FormatError::UnmatchedOpenBrace)
    );

    let close_error = runtime_error(
        r#"
        finish format("}")
        "#,
    )
    .await;
    assert_eq!(
        close_error,
        RuntimeError::Format(lashlang::FormatError::UnmatchedCloseBrace)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn tool_calls_return_result_records() {
    let host = TestHost::default().with_file("src/lib.rs", "pub fn main() {}");
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        found = await files.read({ path: "src/lib.rs" })
        missing = await files.read({ path: "src/missing.rs" })
        finish { found: found, missing: missing }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(
        record["found"].as_record().unwrap()["ok"],
        Value::Bool(true)
    );
    assert_eq!(
        record["missing"].as_record().unwrap()["ok"],
        Value::Bool(false)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aggregate_await_resource_calls_run_concurrently_and_preserve_record_shape() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        results = await {
          left: tools.sleep_echo({ value: "a" })?,
          right: tools.sleep_echo({ value: "b" })?
        }
        finish results
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(record["left"], Value::String("a".into()));
    assert_eq!(record["right"], Value::String("b".into()));
    assert_eq!(
        host.max_active.load(Ordering::SeqCst),
        2,
        "aggregate await should dispatch independent tools concurrently"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_start_and_await_merges_distinct_results() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        process sleep_echo(value: str) { finish value }
        left = start sleep_echo(value: "a")
        right = start sleep_echo(value: "b")
        results = await { left: left, right: right }
        finish { left: results.left?, right: results.right? }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(record["left"], Value::String("a".to_string().into()));
    assert_eq!(record["right"], Value::String("b".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn await_list_returns_branch_results_in_order() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        process sleep_echo(value: str) { finish value }
        results = await [
          start sleep_echo(value: "a"),
          start sleep_echo(value: "b")
        ]
        finish results
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::List(results) = value else {
        panic!("expected result list");
    };
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].as_record().unwrap()["value"],
        Value::String("a".to_string().into())
    );
    assert_eq!(
        results[1].as_record().unwrap()["value"],
        Value::String("b".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn await_record_returns_record_results() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        process sleep_echo(value: str) { finish value }
        results = await {
          first: start sleep_echo(value: "a"),
          second: start sleep_echo(value: "b")
        }
        finish {
          first: results.first?,
          second: results.second?
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(record["first"], Value::String("a".to_string().into()));
    assert_eq!(record["second"], Value::String("b".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn removed_parallel_keyword_is_parse_error() {
    let err = lashlang::compile(
        r#"
        parallel {
          start sleep_echo(value: "a")
        }
        "#,
    )
    .expect_err("parallel keyword should be removed");
    assert!(format!("{err}").contains("unexpected `parallel`"));
}

#[tokio::test(flavor = "current_thread")]
async fn slice_null_bounds_default_to_start_or_end() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        values = [10, 20, 30, 40, 50]
        finish {
          list_tail: slice(values, 3, null),
          list_head: slice(values, null, 2),
          string_tail: slice("abcdef", 4, null),
          string_head: slice("abcdef", null, 2)
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(
        record["list_tail"],
        Value::List(vec![Value::Number(40.0), Value::Number(50.0)].into())
    );
    assert_eq!(
        record["list_head"],
        Value::List(vec![Value::Number(10.0), Value::Number(20.0)].into())
    );
    assert_eq!(
        record["string_tail"],
        Value::String("ef".to_string().into())
    );
    assert_eq!(
        record["string_head"],
        Value::String("ab".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn negative_indices_and_record_contains_are_supported() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        values = [10, 20, 30]
        text = "abc"
        finish {
          tail: values[-1],
          before_tail: values[-2],
          oob: values[-4],
          last_char: text[-1],
          record_has_key: contains({ foo: 1, bar: 2 }, "foo"),
          record_missing_key: contains({ foo: 1, bar: 2 }, "baz")
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(record["tail"], Value::Number(30.0));
    assert_eq!(record["before_tail"], Value::Number(20.0));
    assert_eq!(record["oob"], Value::Null);
    assert_eq!(record["last_char"], Value::String("c".to_string().into()));
    assert_eq!(record["record_has_key"], Value::Bool(true));
    assert_eq!(record["record_missing_key"], Value::Bool(false));
}

#[tokio::test(flavor = "current_thread")]
async fn membership_operator_supports_lists_record_keys_and_string_substrings() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        needle = 2
        haystack = [1, 2, 3]
        substring = "bc"
        text = "abcd"
        finish {
          list_present: needle in haystack,
          list_missing: 4 in [1, 2, 3],
          record_present: "foo" in { foo: 1, bar: 2 },
          record_missing: "baz" in { foo: 1, bar: 2 },
          string_present: substring in text,
          string_missing: "xz" in text,
          string_negated: !("xz" in text)
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(record["list_present"], Value::Bool(true));
    assert_eq!(record["list_missing"], Value::Bool(false));
    assert_eq!(record["record_present"], Value::Bool(true));
    assert_eq!(record["record_missing"], Value::Bool(false));
    assert_eq!(record["string_present"], Value::Bool(true));
    assert_eq!(record["string_missing"], Value::Bool(false));
    assert_eq!(record["string_negated"], Value::Bool(true));
}

#[tokio::test(flavor = "current_thread")]
async fn shaping_builtins_are_deterministic_and_preserve_stable_order() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        rows = [
          { id: "first", profile: { score: 2 } },
          { id: "second", profile: { score: 1 } },
          { id: "third", profile: { score: 2 } }
        ]
        finish {
          sorted: sort([3, 1, 2]),
          sorted_by: sort_by(rows, "profile.score"),
          sum: sum([1, 2, 3]),
          min: min([3, 1, 2]),
          max: max([3, 1, 2]),
          replaced: replace("a-b-a", "a", "x"),
          lower: lower("Straße"),
          upper: upper("Straße"),
          unique: unique([1, 2, 1, 3, 2]),
          reversed: reverse([1, 2, 3])
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(
        record["sorted"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)].into())
    );
    let Value::List(sorted_by) = &record["sorted_by"] else {
        panic!("expected sorted rows");
    };
    let ids = sorted_by
        .iter()
        .map(|row| row.as_record().expect("row")["id"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            Value::String("second".into()),
            Value::String("first".into()),
            Value::String("third".into()),
        ]
    );
    assert_eq!(record["sum"], Value::Number(6.0));
    assert_eq!(record["min"], Value::Number(1.0));
    assert_eq!(record["max"], Value::Number(3.0));
    assert_eq!(record["replaced"], Value::String("x-b-x".into()));
    assert_eq!(record["lower"], Value::String("straße".into()));
    assert_eq!(record["upper"], Value::String("STRASSE".into()));
    assert_eq!(
        record["unique"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0), Value::Number(3.0)].into())
    );
    assert_eq!(
        record["reversed"],
        Value::List(vec![Value::Number(3.0), Value::Number(2.0), Value::Number(1.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn empty_extrema_are_typed_runtime_errors() {
    let host = TestHost::default();
    for builtin in ["min", "max"] {
        let mut state = State::new();
        let error = execute(&format!("finish {builtin}([])"), &mut state, &host)
            .await
            .expect_err("empty extrema must fail");
        assert!(
            matches!(error, ExecuteError::Runtime(RuntimeError::ShapingEmptyList { builtin: actual }) if actual == builtin)
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dynamic_record_indexing_reads_fields() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        key = "foo"
        record = { foo: 42 }
        finish { found: record[key], missing: record["missing"] }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(record["found"], Value::Number(42.0));
    assert_eq!(record["missing"], Value::Null);
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_and_field_assignment_update_collections() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        record = {}
        key = "count"
        record[key] = 1
        record.count = record.count + 1
        record.extra = "ok"
        items = [1, 2, 3]
        items[1] = 20
        items[-1] = 30
        finish { record: record, items: items }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    let counts = record["record"]
        .as_record()
        .expect("expected nested record");
    assert_eq!(counts["count"], Value::Number(2.0));
    assert_eq!(counts["extra"], Value::String("ok".into()));
    assert_eq!(
        record["items"],
        Value::List(vec![Value::Number(1.0), Value::Number(20.0), Value::Number(30.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_path_assignment_and_histogram_loops_work() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        state = { groups: { a: { counts: [1, 2] }, b: { counts: [3] } } }
        g = "a"
        state.groups[g].counts[1] = 5
        counts = {}
        labels = ["a", "b", "a", "c", "b", "a"]
        for label in labels {
          counts[label] = counts[label] + 1
        }
        finish { state: state, counts: counts }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    let state = record["state"].as_record().expect("expected state record");
    let groups = state["groups"].as_record().expect("expected groups record");
    let group_a = groups["a"].as_record().expect("expected group record");
    assert_eq!(
        group_a["counts"],
        Value::List(vec![Value::Number(1.0), Value::Number(5.0)].into())
    );
    let counts = record["counts"]
        .as_record()
        .expect("expected counts record");
    assert_eq!(counts["a"], Value::Number(3.0));
    assert_eq!(counts["b"], Value::Number(2.0));
    assert_eq!(counts["c"], Value::Number(1.0));
}

#[tokio::test(flavor = "current_thread")]
async fn path_assignment_preserves_alias_isolation() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        record = { x: 1, nested: { y: 1 }, items: [1, 2] }
        alias = record
        record.x = 2
        record.nested.y = 3
        record.items[0] = 9
        finish { record: record, alias: alias }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    let updated = record["record"]
        .as_record()
        .expect("expected updated record");
    let alias = record["alias"].as_record().expect("expected alias record");
    assert_eq!(updated["x"], Value::Number(2.0));
    assert_eq!(
        updated["nested"].as_record().unwrap()["y"],
        Value::Number(3.0)
    );
    assert_eq!(
        updated["items"],
        Value::List(vec![Value::Number(9.0), Value::Number(2.0)].into())
    );
    assert_eq!(alias["x"], Value::Number(1.0));
    assert_eq!(
        alias["nested"].as_record().unwrap()["y"],
        Value::Number(1.0)
    );
    assert_eq!(
        alias["items"],
        Value::List(vec![Value::Number(1.0), Value::Number(2.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn path_assignment_rhs_matches_pre_heap_value_semantics() {
    let host = TestHost::default();
    let mut state = State::new();
    execute(
        r#"
            a = {}
            b = []
            a.x = b
            b = push(b, 1)
        "#,
        &mut state,
        &host,
    )
    .await
    .expect("setup should succeed");
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("snapshot should encode");
    let snapshot =
        lashlang::Snapshot::from_canonical_bytes(&bytes).expect("snapshot should decode");
    let mut restored = State::from_snapshot(snapshot);
    let value = finished(
        execute("finish a.x", &mut restored, &host)
            .await
            .expect("restored execution should succeed"),
    );

    assert_eq!(value, Value::List(Vec::new().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn iterator_binding_matches_pre_heap_value_semantics() {
    let host = TestHost::default();
    let mut state = State::new();
    execute(
        r#"
            a = [[1]]
            for x in a {
              x = push(x, 2)
            }
        "#,
        &mut state,
        &host,
    )
    .await
    .expect("setup should succeed");
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("snapshot should encode");
    let snapshot =
        lashlang::Snapshot::from_canonical_bytes(&bytes).expect("snapshot should decode");
    let mut restored = State::from_snapshot(snapshot);
    let value = finished(
        execute("finish a", &mut restored, &host)
            .await
            .expect("restored execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::List(vec![Value::Number(1.0)].into())].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn push_insertion_matches_pre_heap_value_semantics() {
    let value = finished(
        execute(
            r#"
            acc = []
            for n in range(0, 3) {
              row = [n]
              acc = push(acc, row)
              row = push(row, 99)
            }
            finish acc
            "#,
            &mut State::new(),
            &TestHost::default(),
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(
            vec![
                Value::List(vec![Value::Number(0.0)].into()),
                Value::List(vec![Value::Number(1.0)].into()),
                Value::List(vec![Value::Number(2.0)].into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn iterator_value_pushed_into_container_matches_pre_heap_semantics() {
    let value = finished(
        execute(
            r#"
            xs = [[1]]
            acc = []
            for x in xs {
              acc = push(acc, x)
              x = push(x, 9)
            }
            finish acc
            "#,
            &mut State::new(),
            &TestHost::default(),
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(
        value,
        Value::List(vec![Value::List(vec![Value::Number(1.0)].into())].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn nested_field_index_and_comprehension_inserts_keep_value_semantics() {
    let value = finished(
        execute(
            r#"
            source = { rows: [[[1]], [[2]]] }
            flattened = [inner for outer in source.rows for inner in outer]
            picked = source.rows[0][0]
            holder = { items: [] }
            holder.items = push(holder.items, picked)
            picked = push(picked, 9)
            for inner in source.rows[1] { inner = push(inner, 8) }
            finish { flattened: flattened, held: holder.items, source: source }
            "#,
            &mut State::new(),
            &TestHost::default(),
        )
        .await
        .expect("nested insertion program should succeed"),
    );
    let record = value.as_record().expect("result should be a record");
    assert_eq!(
        record["flattened"],
        Value::List(
            vec![
                Value::List(vec![Value::Number(1.0)].into()),
                Value::List(vec![Value::Number(2.0)].into()),
            ]
            .into()
        )
    );
    assert_eq!(
        record["held"],
        Value::List(vec![Value::List(vec![Value::Number(1.0)].into())].into())
    );
    assert_eq!(
        record["source"],
        Value::Record(Arc::new(Record::from_iter([(
            "rows".to_string(),
            Value::List(
                vec![
                    Value::List(vec![Value::List(vec![Value::Number(1.0)].into())].into()),
                    Value::List(vec![Value::List(vec![Value::Number(2.0)].into())].into()),
                ]
                .into()
            ),
        )])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn effect_result_to_path_isolated_from_later_field_reads() {
    let host = TestHost::default().with_file("a.txt", "original");
    let source = r#"
            holder = { value: null }
            holder.value = await files.read({ path: "a.txt" })?
            local = holder.value
            finish { local: local, stored: holder.value }
            "#;
    let linked = lashlang::LinkedModule::link(
        parse(source).expect("effect path program should parse"),
        test_host_environment(),
    )
    .expect("effect path program should link");
    let compiled = lashlang::compile_linked(&linked);
    let mut state = State::new();
    let value = finished(
        lashlang::execute(&compiled, &mut state, &host)
            .await
            .expect("effect path insertion should succeed"),
    );
    assert_eq!(
        value,
        Value::Record(Arc::new(Record::from_iter([
            ("local".to_string(), Value::String("original".into())),
            ("stored".to_string(), Value::String("original".into())),
        ])))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn heap_aware_global_patches_survive_next_cell_and_cold_restore() {
    let host = TestHost::default();
    let mut state = State::new();
    execute("seed = [1]", &mut state, &host)
        .await
        .expect("heap setup should succeed");
    assert!(
        state
            .set_default(
                "diary",
                Value::List(vec![Value::String("kept".into())].into()),
            )
            .expect("default should be metered")
    );

    let value = finished(
        execute("finish diary", &mut state, &host)
            .await
            .expect("patched global should reach the next cell"),
    );
    assert_eq!(
        value,
        Value::List(vec![Value::String("kept".into())].into())
    );

    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("patched snapshot should encode");
    let snapshot =
        lashlang::Snapshot::from_canonical_bytes(&bytes).expect("snapshot should decode");
    let mut restored = State::from_snapshot(snapshot);
    let value = finished(
        execute("finish diary", &mut restored, &host)
            .await
            .expect("patched global should survive cold restore"),
    );
    assert_eq!(
        value,
        Value::List(vec![Value::String("kept".into())].into())
    );

    assert!(restored.remove_global("diary").is_some());
    assert!(matches!(
        execute("finish diary", &mut restored, &host).await,
        Err(ExecuteError::Runtime(
            RuntimeError::UndefinedVariable { .. }
        ))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn path_assignment_reports_invalid_targets() {
    assert!(matches!(
        runtime_error("items = [1]\nitems[2] = 2").await,
        RuntimeError::ListAssignmentIndexOutOfBounds
    ));
    assert!(matches!(
        runtime_error("items = [1]\nitems[0.5] = 2").await,
        RuntimeError::InvalidListAssignmentIndex
    ));
    assert!(matches!(
        runtime_error("items = [1]\nitems[\"0\"] = 2").await,
        RuntimeError::InvalidListAssignmentIndex
    ));
    assert!(matches!(
        runtime_error("text = \"abc\"\ntext[0] = \"x\"").await,
        RuntimeError::CannotAssignIndex { actual } if actual == "string"
    ));
    assert!(matches!(
        runtime_error("record = {}\nrecord.missing.value = 1").await,
        RuntimeError::MissingAssignmentField { field } if field == "missing"
    ));
    assert!(matches!(
        runtime_error("record = { item: 1 }\nrecord.item.value = 2").await,
        RuntimeError::CannotAssignField { actual, .. } if actual == "number"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn else_if_chains_execute_without_extra_braces() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        score = 7
        if score > 10 {
          label = "large"
        } else if score > 5 {
          label = "medium"
        } else {
          label = "small"
        }
        finish label
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("medium".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn slice_supports_negative_bounds() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        values = [10, 20, 30, 40, 50]
        finish {
          list_tail: slice(values, -2, null),
          list_without_last: slice(values, null, -1),
          list_middle: slice(values, -4, -1),
          string_tail: slice("abcdef", -2, null),
          string_without_last: slice("abcdef", null, -1),
          string_middle: slice("abcdef", -5, -2)
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(
        record["list_tail"],
        Value::List(vec![Value::Number(40.0), Value::Number(50.0)].into())
    );
    assert_eq!(
        record["list_without_last"],
        Value::List(
            vec![
                Value::Number(10.0),
                Value::Number(20.0),
                Value::Number(30.0),
                Value::Number(40.0),
            ]
            .into()
        )
    );
    assert_eq!(
        record["list_middle"],
        Value::List(
            vec![
                Value::Number(20.0),
                Value::Number(30.0),
                Value::Number(40.0),
            ]
            .into()
        )
    );
    assert_eq!(
        record["string_tail"],
        Value::String("ef".to_string().into())
    );
    assert_eq!(
        record["string_without_last"],
        Value::String("abcde".to_string().into())
    );
    assert_eq!(
        record["string_middle"],
        Value::String("bcd".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn range_and_push_cover_common_collection_building() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        indexes = range(0, 3)
        extended = push(indexes, 3)
        loop_total = 0
        for n in range(0, 4) {
          loop_total = loop_total + n
        }
        finish {
          indexes: indexes,
          extended: extended,
          from_zero: range(3),
          negative: range(-2, 1),
          empty: range(5, 2),
          loop_total: loop_total
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(
        record["indexes"],
        Value::List(vec![Value::Number(0.0), Value::Number(1.0), Value::Number(2.0)].into())
    );
    assert_eq!(
        record["extended"],
        Value::List(
            vec![
                Value::Number(0.0),
                Value::Number(1.0),
                Value::Number(2.0),
                Value::Number(3.0),
            ]
            .into()
        )
    );
    assert_eq!(
        record["from_zero"],
        Value::List(vec![Value::Number(0.0), Value::Number(1.0), Value::Number(2.0)].into())
    );
    assert_eq!(
        record["negative"],
        Value::List(vec![Value::Number(-2.0), Value::Number(-1.0), Value::Number(0.0)].into())
    );
    assert_eq!(record["empty"], Value::List(Vec::new().into()));
    assert_eq!(record["loop_total"], Value::Number(6.0));
}

#[tokio::test(flavor = "current_thread")]
async fn for_loop_assignments_carry_across_iterations() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        raw = split(" x , y , z ", ",")
        parts = []
        count = 0
        snapshots = []
        for part in raw {
          parts = push(parts, trim(part))
          count = count + 1
          snapshots = push(snapshots, { part: trim(part), parts: parts, count: count })
        }
        finish { parts: parts, count: count, snapshots: snapshots }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(
        record["parts"],
        Value::List(
            vec![
                Value::String("x".into()),
                Value::String("y".into()),
                Value::String("z".into()),
            ]
            .into()
        )
    );
    assert_eq!(record["count"], Value::Number(3.0));
    let Value::List(snapshots) = &record["snapshots"] else {
        panic!("expected snapshots list");
    };
    assert_eq!(snapshots.len(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn await_record_accepts_commas_and_keyword_record_keys_execute() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        process sleep_echo(value: str) { finish value }
        result = await {
          fanout: start sleep_echo(value: "ok"),
          "with space": start sleep_echo(value: "quoted"),
        }
        finish {
          branch: result.fanout?,
          quoted_value: result["with space"]?
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(record["branch"], Value::String("ok".into()));
    assert_eq!(record["quoted_value"], Value::String("quoted".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn string_comparisons_are_lexicographic() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish {
          lt: "abc" < "def",
          gt: "xyz" > "abc",
          le: "abc" <= "abc",
          ge: "xyz" >= "abc"
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(record["lt"], Value::Bool(true));
    assert_eq!(record["gt"], Value::Bool(true));
    assert_eq!(record["le"], Value::Bool(true));
    assert_eq!(record["ge"], Value::Bool(true));
}

#[tokio::test(flavor = "current_thread")]
async fn stringification_preserves_integer_format_inside_containers() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        finish {
          list_text: to_string([1, 2]),
          record_text: to_string({ a: 1, b: 2.5 })
        }
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    let Value::Record(record) = value else {
        panic!("expected record");
    };
    assert_eq!(
        record["list_text"],
        Value::String("[1,2]".to_string().into())
    );
    assert_eq!(
        record["record_text"],
        Value::String("{\"a\":1,\"b\":2.5}".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_round_trip_preserves_repl_like_state() {
    let host = TestHost::default();
    let mut state = State::new();

    finished(
        execute(
            r#"
        counter = 1
        finish counter
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("first execution should succeed"),
    );

    let snapshot = state.snapshot();
    let encoded = snapshot
        .to_canonical_bytes()
        .expect("snapshot should serialize");
    let decoded =
        lashlang::Snapshot::from_canonical_bytes(&encoded).expect("snapshot should deserialize");
    let mut restored = State::from_snapshot(decoded);

    let value = finished(
        execute(
            r#"
        counter = counter + 1
        finish counter
        "#,
            &mut restored,
            &host,
        )
        .await
        .expect("restored execution should succeed"),
    );

    assert_eq!(value, Value::Number(2.0));
}

#[tokio::test(flavor = "current_thread")]
async fn json_and_record_helpers_work() {
    let host = TestHost::default();
    let mut state = State::new();

    let value = finished(
        execute(
            r#"
        obj = json_parse("{\"path\":\"src/lib.rs\",\"line\":7}")
        finish format("{}:{}", obj.path, obj.line)
        "#,
            &mut state,
            &host,
        )
        .await
        .expect("execution should succeed"),
    );

    assert_eq!(value, Value::String("src/lib.rs:7".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn parse_errors_are_parse_level_and_precise() {
    let error = parse(
        r#"
        if true {
          answer = 1
        "#,
    )
    .expect_err("parse should fail");

    match error {
        lashlang::ParseError::Expected { expected, .. } => assert_eq!(expected, "`}`"),
        other => panic!("unexpected parse error: {other:?}"),
    }
}

include!("language/support.rs");

include!("language/function_declarations.rs");
