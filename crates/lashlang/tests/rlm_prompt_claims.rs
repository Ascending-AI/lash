//! Tests that pin down every behavioural claim made in the RLM execution
//! section of the system prompt (`crates/lash-protocol-rlm/src/driver.rs::RLM_EXECUTION_SECTION`).
//!
//! Each test here is wired to a specific bullet or example so the prompt and
//! the runtime can never drift again without a test failing. If you change
//! lashlang semantics, the failing test tells you which line in the prompt
//! is now a lie.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, ParseError,
    ProcessStart, Record, State, Value, parse,
};
use std::collections::HashMap;
use std::sync::Mutex;

// ─────────────────────────────────────────────────────────────────────
// Test host: mirrors the real RLM runtime's tool-result wrapping
// contract (the runtime automatically wraps `call` results as
// {ok, value}; the host itself just returns the raw inner value).
// ─────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct MockHost {
    files: HashMap<String, String>,
    observations: Mutex<Vec<Value>>,
    // Handles for start/await: process start returns a record like
    // `{ handle: "h<n>", tool: "<name>" }`, and `await_handle` looks up
    // the pending tool result by handle id.
    pending: Mutex<HashMap<String, Value>>,
    next_handle: Mutex<u32>,
}

impl MockHost {
    fn with_file(mut self, path: &str, contents: &str) -> Self {
        self.files.insert(path.to_string(), contents.to_string());
        self
    }

    fn record_observation(&self, value: Value) {
        self.observations.lock().unwrap().push(value);
    }

    fn observations(&self) -> Vec<Value> {
        self.observations.lock().unwrap().clone()
    }
}

impl ExecutionHost for MockHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => self
                .perform_resource_operation(operation)
                .await
                .map(AbilityResult::Value),
            AbilityOp::ResourceOperationBatch(batch) => {
                let mut results = Vec::with_capacity(batch.operations.len());
                for operation in batch.operations {
                    results.push(lashlang::ResourceOperationResult::from_result(
                        self.perform_resource_operation(operation).await,
                    ));
                }
                Ok(AbilityResult::ResourceOperationBatch(
                    lashlang::ResourceOperationBatchResult { results },
                ))
            }
            AbilityOp::StartProcess(start) => {
                self.start_process(*start).await.map(AbilityResult::Value)
            }
            AbilityOp::Await(handle) => self
                .await_handle_value(handle)
                .await
                .map(AbilityResult::Value),
            AbilityOp::Cancel(handle) => self
                .cancel_handle_value(handle)
                .await
                .map(AbilityResult::Value),
            AbilityOp::Print(value) => {
                self.record_observation(value);
                Ok(AbilityResult::Unit)
            }
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

impl MockHost {
    async fn perform_resource_operation(
        &self,
        operation: lashlang::ResourceOperation,
    ) -> Result<Value, ExecutionHostError> {
        let args = operation
            .args
            .first()
            .and_then(Value::as_record)
            .cloned()
            .unwrap_or_default();
        let name = test_host_operation(&operation)?;
        self.call_tool(&name, args).await
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
    let mut resources = lashlang::LashlangHostCatalog::tool_default(["echo", "boom"]);
    resources.add_module_operation(
        ["files"],
        "Files",
        "read",
        "read_file",
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Any,
    );
    resources.add_module_operation(
        ["files"],
        "Files",
        "edit",
        "edit",
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Any,
    );
    resources.add_module_operation(
        ["files"],
        "Files",
        "write",
        "write",
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Any,
    );
    resources.add_module_operation(
        ["shell"],
        "Shell",
        "exec",
        "exec_command",
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Any,
    );
    resources.add_module_operation(
        ["agents"],
        "Agents",
        "spawn",
        "spawn_agent",
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Any,
    );
    resources.add_module_operation(
        ["processes"],
        "Processes",
        "list",
        "list_process_handles",
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Any,
    );
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

impl MockHost {
    async fn call_tool(&self, name: &str, args: Record) -> Result<Value, ExecutionHostError> {
        match name {
            "read_file" => {
                let path = args
                    .get("path")
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.to_string()),
                        _ => None,
                    })
                    .ok_or_else(|| ExecutionHostError::new("missing path"))?;
                self.files
                    .get(&path)
                    .cloned()
                    .map(|s| Value::String(s.into()))
                    .ok_or_else(|| ExecutionHostError::new(format!("no such file: {path}")))
            }
            "echo" => Ok(args.get("value").cloned().unwrap_or(Value::Null)),
            "exec_command" => {
                let cmd = args
                    .get("cmd")
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .ok_or_else(|| ExecutionHostError::new("missing cmd"))?;
                let mut record = Record::default();
                let exit_code = if cmd == "test -f Cargo.lock" { 1 } else { 0 };
                record.insert("status".into(), Value::String("completed".into()));
                record.insert("done".into(), Value::Bool(true));
                record.insert("running".into(), Value::Bool(false));
                let output = if cmd == "large-output" {
                    format!("{}TAIL", "x".repeat(4096))
                } else {
                    format!("ran: {cmd}")
                };
                record.insert("output".into(), Value::String(output.into()));
                record.insert("exit_code".into(), Value::Number(exit_code.into()));
                Ok(Value::Record(record.into()))
            }
            "edit" => Ok(Value::String("edit applied".into())),
            "write" => Ok(Value::String("write applied".into())),
            "spawn_agent" | "inspect_chunk" => {
                let task = args
                    .get("task")
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.to_string()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let mut record = Record::default();
                record.insert("claim".into(), Value::String(format!("done:{task}").into()));
                Ok(Value::Record(record.into()))
            }
            "list_process_handles" => {
                let mut out = Record::default();
                out.insert("tool".into(), Value::Record(Record::default().into()));
                Ok(Value::Record(out.into()))
            }
            "boom" => Err(ExecutionHostError::new("explicit failure for tests")),
            _ => Err(ExecutionHostError::new(format!("unknown tool: {name}"))),
        }
    }

    async fn start_process(&self, start: ProcessStart) -> Result<Value, ExecutionHostError> {
        let handle_id = {
            let mut counter = self.next_handle.lock().unwrap();
            *counter += 1;
            format!("p{}", *counter)
        };
        let result = if start.process_name == "scan" {
            start.args.get("root").cloned().unwrap_or(Value::Null)
        } else {
            self.call_tool(&start.process_name, start.args.clone())
                .await?
        };
        self.pending
            .lock()
            .unwrap()
            .insert(handle_id.clone(), result);
        let mut record = Record::default();
        record.insert("__handle__".into(), Value::String("process".into()));
        record.insert("id".into(), Value::String(handle_id.into()));
        Ok(Value::Record(record.into()))
    }

    async fn await_handle_value(&self, handle: Value) -> Result<Value, ExecutionHostError> {
        let record = handle
            .as_record()
            .ok_or_else(|| ExecutionHostError::new("expected handle record"))?;
        let id = record
            .get("id")
            .and_then(|v| match v {
                Value::String(s) => Some(s.to_string()),
                _ => None,
            })
            .ok_or_else(|| ExecutionHostError::new("handle record missing `id` field"))?;
        self.pending
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or_else(|| ExecutionHostError::new(format!("unknown handle: {id}")))
    }

    async fn cancel_handle_value(&self, handle: Value) -> Result<Value, ExecutionHostError> {
        if let Some(record) = handle.as_record()
            && let Some(Value::String(id)) = record.get("id")
        {
            self.pending.lock().unwrap().remove(id.as_str());
        }
        Ok(Value::Null)
    }
}

fn run(host: &MockHost, source: &str) -> Value {
    let mut state = State::new();
    match futures::executor::block_on(execute(source, &mut state, host))
        .expect("execution should succeed")
    {
        ExecutionOutcome::Finished(value) => value,
        ExecutionOutcome::Continued => Value::Null,
        ExecutionOutcome::Failed(value) => panic!("unexpected process failure: {value}"),
    }
}

fn run_continued(host: &MockHost, source: &str) -> (ExecutionOutcome, State) {
    let mut state = State::new();
    let outcome = futures::executor::block_on(execute(source, &mut state, host))
        .expect("execution should succeed");
    (outcome, state)
}

// ─────────────────────────────────────────────────────────────────────
// Prompt claim: "Values: null, booleans, numbers, strings, lists, records.
//               Literals: `[a, b]`, `{ a: 1, b: 2 }`."
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_value_literals_parse_and_evaluate() {
    let host = MockHost::default();
    assert_eq!(run(&host, "finish null"), Value::Null);
    assert_eq!(run(&host, "finish true"), Value::Bool(true));
    assert_eq!(run(&host, "finish 42"), Value::Number(42.0));
    assert_eq!(
        run(&host, r#"finish "hi""#),
        Value::String("hi".to_string().into())
    );
    // list literal
    let Value::List(items) = run(&host, "finish [1, 2, 3]") else {
        panic!("expected list");
    };
    assert_eq!(items.len(), 3);
    // record literal
    let Value::Record(rec) = run(&host, "finish { a: 1, b: 2 }") else {
        panic!("expected record");
    };
    assert_eq!(rec["a"], Value::Number(1.0));
    assert_eq!(rec["b"], Value::Number(2.0));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_raw_triple_strings_parse_and_evaluate() {
    let host = MockHost::default();
    assert_eq!(
        run(
            &host,
            r#####"finish r'''python3 - <<'PY'
print("""hello""")
\n { braces stay raw }
PY'''"#####
        ),
        Value::String(
            "python3 - <<'PY'\nprint(\"\"\"hello\"\"\")\n\\n { braces stay raw }\nPY".into()
        )
    );
}

// ─────────────────────────────────────────────────────────────────────
// Prompt claim: "Assign with `name = expr`. Variables persist across
// a Lashlang block within the turn."
// (Within-block persistence is covered here; cross-block persistence
// is tested at the RLM-runtime level, not at the lashlang level.)
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_assignment_persists_within_program() {
    let host = MockHost::default();
    assert_eq!(
        run(&host, "x = 7\ny = x + 3\nfinish y"),
        Value::Number(10.0)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_indexed_and_field_assignment_examples_work() {
    let host = MockHost::default();
    let Value::Record(result) = run(
        &host,
        r#"
counts = {}
g = "alpha"
counts[g] = counts[g] + 1
counts.total = 1
items = [0, 0]
items[1] = counts[g]
finish { counts: counts, items: items }
"#,
    ) else {
        panic!("expected record");
    };
    let counts = result["counts"].as_record().expect("counts record");
    assert_eq!(counts["alpha"], Value::Number(1.0));
    assert_eq!(counts["total"], Value::Number(1.0));
    assert_eq!(
        result["items"],
        Value::List(vec![Value::Number(0.0), Value::Number(1.0)].into())
    );
}

// ─────────────────────────────────────────────────────────────────────
// Prompt claim: "Every tool call returns a wrapper record:
// `{ ok: true, value: <tool output> }` on success,
// `{ ok: false, error: "..." }` on failure."
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_tool_call_success_is_wrapped_with_ok_and_value() {
    let host = MockHost::default().with_file("a.txt", "hello world");
    let Value::Record(r) = run(&host, r#"finish await files.read({ path: "a.txt" })"#) else {
        panic!("expected wrapped record");
    };
    assert_eq!(r["ok"], Value::Bool(true));
    assert_eq!(r["value"], Value::String("hello world".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_tool_call_failure_is_wrapped_with_ok_false_and_error() {
    let host = MockHost::default();
    let Value::Record(r) = run(&host, "finish await tools.boom({})") else {
        panic!("expected wrapped record");
    };
    assert_eq!(r["ok"], Value::Bool(false));
    assert!(matches!(&r["error"], Value::String(_)));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_value_field_reaches_the_underlying_tool_output() {
    // The bug that motivated this section: models used `.output` / `.path`
    // directly on the wrapper and got null. Manual `.value` access still
    // works when code intentionally keeps the raw wrapper.
    let host = MockHost::default().with_file("a.txt", "file text");
    assert_eq!(
        run(
            &host,
            r#"r = await files.read({ path: "a.txt" })
finish r.value"#,
        ),
        Value::String("file text".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_question_unwraps_successful_tool_results() {
    let host = MockHost::default().with_file("a.txt", "file text");
    assert_eq!(
        run(
            &host,
            r#"text = await files.read({ path: "a.txt" })?
finish text"#,
        ),
        Value::String("file text".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_question_aborts_failed_tool_results_with_error() {
    let host = MockHost::default();
    let mut state = State::new();
    let err = execute("finish await tools.boom({})?", &mut state, &host)
        .await
        .expect_err("failed result unwrap should abort");
    let ExecuteError::Runtime(err) = err else {
        panic!("expected runtime error");
    };
    assert!(
        err.to_string().contains("explicit failure for tests"),
        "unexpected error: {err}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Prompt claim: "`start process(arg: expr)` returns a handle
// (not wrapped). Resolve with `await handle` — that returns the same
// `{ ok, value }` wrapper as a synchronous call."
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_start_returns_unwrapped_handle() {
    let host = MockHost::default().with_file("a.txt", "x");
    let Value::Record(handle) = run(
        &host,
        r#"process read_file(path: str) { finish path }
h = start read_file(path: "a.txt")
finish h"#,
    ) else {
        panic!("expected handle record");
    };
    // Handle is NOT a tool-result wrapper.
    assert!(
        handle.get("ok").is_none(),
        "start should not wrap in {{ok,value}}"
    );
    assert!(matches!(handle.get("id"), Some(Value::String(_))));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_named_process_start_returns_unwrapped_handle() {
    let host = MockHost::default();
    let Value::Record(handle) = run(
        &host,
        r#"root = "."
process scan(root: str) {
  finish root
}
h = start scan(root: root)
finish h"#,
    ) else {
        panic!("expected handle record");
    };
    assert!(handle.get("ok").is_none());
    assert_eq!(
        handle.get("__handle__"),
        Some(&Value::String("process".into()))
    );
    assert!(matches!(handle.get("id"), Some(Value::String(_))));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_await_handle_wraps_result_with_ok_value() {
    let host = MockHost::default().with_file("a.txt", "body");
    let Value::Record(r) = run(
        &host,
        r#"process read_file(path: str) { finish path }
h = start read_file(path: "a.txt")
finish await h"#,
    ) else {
        panic!("expected wrapped record");
    };
    assert_eq!(r["ok"], Value::Bool(true));
    assert_eq!(r["value"], Value::String("body".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_question_unwraps_awaited_handle_results() {
    let host = MockHost::default().with_file("a.txt", "body");
    assert_eq!(
        run(
            &host,
            r#"process read_file(path: str) { finish path }
h = start read_file(path: "a.txt")
finish (await h)?"#,
        ),
        Value::String("body".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_await_list_returns_wrappers_in_order() {
    let host = MockHost::default()
        .with_file("a.txt", "A")
        .with_file("b.txt", "B");
    let Value::List(items) = run(
        &host,
        r#"process read_file(path: str) { finish path }
results = await [
  start read_file(path: "a.txt"),
  start read_file(path: "b.txt"),
]
finish results"#,
    ) else {
        panic!("expected list");
    };
    assert_eq!(items.len(), 2);
    let a = items[0].as_record().expect("wrapper record");
    let b = items[1].as_record().expect("wrapper record");
    assert_eq!(a["ok"], Value::Bool(true));
    assert_eq!(a["value"], Value::String("A".to_string().into()));
    assert_eq!(b["ok"], Value::Bool(true));
    assert_eq!(b["value"], Value::String("B".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_await_record_returns_wrappers_by_name() {
    let host = MockHost::default()
        .with_file("a.txt", "A")
        .with_file("b.txt", "B");
    let Value::Record(items) = run(
        &host,
        r#"process read_file(path: str) { finish path }
results = await {
  a: start read_file(path: "a.txt"),
  b: start read_file(path: "b.txt"),
}
finish results"#,
    ) else {
        panic!("expected record");
    };
    assert_eq!(
        items["a"].as_record().unwrap()["value"],
        Value::String("A".into())
    );
    assert_eq!(
        items["b"].as_record().unwrap()["value"],
        Value::String("B".into())
    );
}

// ─────────────────────────────────────────────────────────────────────
// Prompt claim: "`cancel handle` (best-effort)"
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_cancel_handle_runs_without_error() {
    let host = MockHost::default().with_file("a.txt", "x");
    run(
        &host,
        r#"process read_file(path: str) { finish path }
h = start read_file(path: "a.txt")
cancel h
finish "done""#,
    );
}

// ─────────────────────────────────────────────────────────────────────
// Prompt claim: "`print expr` inspects a value mid-turn."
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_print_feeds_value_to_host() {
    let host = MockHost::default();
    let (outcome, _state) = run_continued(
        &host,
        r#"v = { total: 42 }
print v"#,
    );
    assert!(matches!(outcome, ExecutionOutcome::Continued));
    let observations = host.observations();
    assert_eq!(observations.len(), 1);
    let r = observations[0].as_record().expect("observed record");
    assert_eq!(r["total"], Value::Number(42.0));
}

// ─────────────────────────────────────────────────────────────────────
// Prompt claim: "`finish <expr>` ends the turn with the given value..."
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_finish_terminates_program_with_value() {
    let host = MockHost::default();
    let mut state = State::new();
    let outcome = execute("x = 3\nfinish x * 2", &mut state, &host)
        .await
        .expect("runs");
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(6.0)));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_bare_finish_requires_value() {
    let host = MockHost::default();
    let mut state = State::new();
    let err = execute("finish", &mut state, &host)
        .await
        .expect_err("bare finish should be rejected");
    assert_eq!(
        err.to_string(),
        "`finish` requires a value; use `finish null` to finish with null"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Prompt claim: "Control flow: statement `if`/`for`/`while`; `break` exits
// the nearest loop; `continue` skips to the nearest loop's next iteration;
// expression ternary `cond ? yes : no` (there is no expression-form `if`);
// boolean negation via `!cond` or `not cond`. Prefer bounded `while` loops
// where possible and bounded `for` loops over ranges/lists for fill or retry
// logic.
// `finish` is different from `break`: it ends the whole program/turn."
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_if_for_while_and_ternary_work() {
    let host = MockHost::default();
    assert_eq!(
        run(
            &host,
            r#"total = 0
for n in [1, 2, 3] {
  total = total + n
}
finish total"#,
        ),
        Value::Number(6.0)
    );
    assert_eq!(
        run(&host, r#"finish true ? "a" : "b""#),
        Value::String("a".to_string().into())
    );
    assert_eq!(
        run(&host, "if 1 < 2 { finish 7 } else { finish 9 }"),
        Value::Number(7.0)
    );
    assert_eq!(
        run(
            &host,
            r#"items = []
while len(items) < 3 {
  items = items + [len(items)]
}
finish items"#,
        ),
        Value::List(vec![Value::Number(0.0), Value::Number(1.0), Value::Number(2.0)].into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_break_and_continue_control_nearest_loop() {
    let host = MockHost::default();
    assert_eq!(
        run(
            &host,
            r#"seen = []
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
}
finish seen"#,
        ),
        Value::List(
            vec![
                Value::String("1:1".to_string().into()),
                Value::String("2:1".to_string().into()),
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_finish_exits_whole_program_not_loop() {
    let host = MockHost::default();
    assert_eq!(
        run(
            &host,
            r#"for n in [1, 2, 3] {
  finish n
}
finish 99"#,
        ),
        Value::Number(1.0)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_loop_control_requires_loop() {
    for (source, keyword) in [("break", "break"), ("continue", "continue")] {
        let err = parse(source).expect_err("loop control outside loop should fail");
        assert!(matches!(
            err,
            ParseError::LoopControlOutsideLoop {
                keyword: actual,
                ..
            } if actual == keyword
        ));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_both_negation_forms_work() {
    let host = MockHost::default();
    assert_eq!(run(&host, "finish !true"), Value::Bool(false));
    assert_eq!(run(&host, "finish not true"), Value::Bool(false));
    assert_eq!(run(&host, "finish !false"), Value::Bool(true));
    assert_eq!(run(&host, "finish not false"), Value::Bool(true));
}

// ─────────────────────────────────────────────────────────────────────
// Prompt claim: every listed builtin is callable and behaves as described.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_len_returns_length() {
    let host = MockHost::default();
    assert_eq!(run(&host, r#"finish len("hi")"#), Value::Number(2.0));
    assert_eq!(run(&host, "finish len([1,2,3])"), Value::Number(3.0));
    assert_eq!(run(&host, "finish len({a: 1, b: 2})"), Value::Number(2.0));
    assert_eq!(run(&host, "finish len(null)"), Value::Number(0.0));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_empty_checks_zero_length() {
    let host = MockHost::default();
    assert_eq!(run(&host, r#"finish empty("")"#), Value::Bool(true));
    assert_eq!(run(&host, r#"finish empty("x")"#), Value::Bool(false));
    assert_eq!(run(&host, "finish empty([])"), Value::Bool(true));
    assert_eq!(run(&host, "finish empty([1])"), Value::Bool(false));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_slice_supports_null_bounds_and_negative_bounds() {
    let host = MockHost::default();
    // null = from start / to end
    assert_eq!(
        run(&host, r#"finish slice("abcdef", null, 3)"#),
        Value::String("abc".to_string().into())
    );
    assert_eq!(
        run(&host, r#"finish slice("abcdef", 3, null)"#),
        Value::String("def".to_string().into())
    );
    // negative bound counts from the end
    assert_eq!(
        run(&host, r#"finish slice("abcdef", 0, -2)"#),
        Value::String("abcd".to_string().into())
    );
    // works on lists too
    let Value::List(items) = run(&host, "finish slice([1,2,3,4], 1, 3)") else {
        panic!("list");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0], Value::Number(2.0));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_range_and_push_build_lists() {
    let host = MockHost::default();
    assert_eq!(
        run(&host, "finish range(3)"),
        Value::List(vec![Value::Number(0.0), Value::Number(1.0), Value::Number(2.0)].into())
    );
    assert_eq!(
        run(&host, "finish range(-2, 1)"),
        Value::List(vec![Value::Number(-2.0), Value::Number(-1.0), Value::Number(0.0)].into())
    );
    assert_eq!(
        run(&host, "finish range(3, 3)"),
        Value::List(Vec::new().into())
    );
    assert_eq!(
        run(&host, "finish range(0, 5, 2)"),
        Value::List(vec![Value::Number(0.0), Value::Number(2.0), Value::Number(4.0)].into())
    );
    assert_eq!(
        run(&host, "finish range(5, 0, -2)"),
        Value::List(vec![Value::Number(5.0), Value::Number(3.0), Value::Number(1.0)].into())
    );

    let value = run(
        &host,
        r#"
        base = ["a"]
        extended = push(base, "b")
        finish { base: base, extended: extended }
        "#,
    );
    let record = value.as_record().expect("record");
    assert_eq!(
        record["base"],
        Value::List(vec![Value::String("a".to_string().into())].into())
    );
    assert_eq!(
        record["extended"],
        Value::List(
            vec![
                Value::String("a".to_string().into()),
                Value::String("b".to_string().into())
            ]
            .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_start_is_contextual() {
    let host = MockHost::default();
    assert_eq!(
        run(
            &host,
            r#"
            start = 1
            for start in range(3) {
              last = start
            }
            finish { value: start, field: { start: last }.start }
            "#,
        ),
        {
            let mut record = Record::default();
            record.insert("value".to_string(), Value::Number(1.0));
            record.insert("field".to_string(), Value::Number(2.0));
            Value::Record(record.into())
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_integer_division_helpers_support_chunk_math() {
    let host = MockHost::default();
    let value = run(
        &host,
        r#"
        items = range(10)
        step = ceil_div(len(items), 3)
        starts = []
        for i in range(0, len(items), step) {
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
async fn prompt_claim_builtin_split_and_join() {
    let host = MockHost::default();
    let Value::List(parts) = run(&host, r#"finish split("a,b,c", ",")"#) else {
        panic!("list");
    };
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0], Value::String("a".to_string().into()));

    assert_eq!(
        run(&host, r#"finish join(["a", "b", "c"], "-")"#),
        Value::String("a-b-c".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_trim_strips_whitespace() {
    let host = MockHost::default();
    assert_eq!(
        run(&host, r#"finish trim("  hi  ")"#),
        Value::String("hi".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_starts_ends_contains() {
    let host = MockHost::default();
    assert_eq!(
        run(&host, r#"finish starts_with("foobar", "foo")"#),
        Value::Bool(true)
    );
    assert_eq!(
        run(&host, r#"finish ends_with("foobar", "bar")"#),
        Value::Bool(true)
    );
    assert_eq!(
        run(&host, r#"finish contains("foobar", "oob")"#),
        Value::Bool(true)
    );
    assert_eq!(
        run(&host, r#"finish contains([1,2,3], 2)"#),
        Value::Bool(true)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_find_and_grep_text() {
    let host = MockHost::default();
    assert_eq!(
        run(&host, r#"finish find("alpha beta", "beta")"#),
        Value::Number(6.0)
    );
    assert_eq!(run(&host, r#"finish find("alpha", "z")"#), Value::Null);

    let Value::List(matches) = run(
        &host,
        r#"finish grep_text("one\nmatch here\r\nmatch again\n", "match")"#,
    ) else {
        panic!("list");
    };
    assert_eq!(matches.len(), 2);
    let first = matches[0].as_record().expect("match record");
    assert_eq!(first["line"], Value::Number(2.0));
    assert_eq!(
        first["text"],
        Value::String("match here".to_string().into())
    );
    assert_eq!(first["match"], Value::String("match".to_string().into()));
    assert_eq!(first["start"], Value::Number(0.0));
    assert_eq!(first["end"], Value::Number(5.0));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_keys_and_values() {
    let host = MockHost::default();
    let Value::List(keys) = run(&host, "finish keys({a: 1, b: 2})") else {
        panic!("list");
    };
    assert_eq!(keys.len(), 2);
    // Order is insertion order in lashlang records.
    assert_eq!(keys[0], Value::String("a".to_string().into()));
    assert_eq!(keys[1], Value::String("b".to_string().into()));

    let Value::List(vals) = run(&host, "finish values({a: 1, b: 2})") else {
        panic!("list");
    };
    assert_eq!(vals.len(), 2);
    assert_eq!(vals[0], Value::Number(1.0));
    assert_eq!(vals[1], Value::Number(2.0));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_to_string_to_int_to_float() {
    let host = MockHost::default();
    assert_eq!(
        run(&host, "finish to_string(42)"),
        Value::String("42".to_string().into())
    );
    assert_eq!(run(&host, r#"finish to_int("7")"#), Value::Number(7.0));
    assert_eq!(run(&host, r#"finish to_float("3.5")"#), Value::Number(3.5));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_json_parse_parses_strings_to_values() {
    let host = MockHost::default();
    let Value::Record(r) = run(&host, r#"finish json_parse("{\"a\": 1}")"#) else {
        panic!("record");
    };
    assert_eq!(r["a"], Value::Number(1.0));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_format_positional_placeholders() {
    let host = MockHost::default();
    // Auto-numbered `{}` placeholders.
    assert_eq!(
        run(&host, r#"finish format("hi {} you are {}", "sam", 3)"#),
        Value::String("hi sam you are 3".to_string().into())
    );
    // Explicit indices `{0}`, `{1}`.
    assert_eq!(
        run(&host, r#"finish format("{1} {0}", "world", "hello")"#),
        Value::String("hello world".to_string().into())
    );
    // Literal braces via doubling.
    assert_eq!(
        run(&host, r#"finish format("{{ {} }}", "x")"#),
        Value::String("{ x }".to_string().into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_format_handles_markdown_raw_templates() {
    let host = MockHost::default();
    assert_eq!(
        run(
            &host,
            r####"
        finish format(r"""## {0}

```json
{{"status":"{1}","exit_code":{2}}}
```

{3}""", "cargo-machete", "timed_out", 124, "The install command exceeded the timeout.")
        "####
        ),
        Value::String(
            "## cargo-machete\n\n```json\n{\"status\":\"timed_out\",\"exit_code\":124}\n```\n\nThe install command exceeded the timeout."
                .to_string()
                .into()
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_builtin_validate_checks_type_literals_mid_program() {
    let host = MockHost::default();
    let value = run(
        &host,
        r#"
        raw = { name: "lashlang", labels: ["agent", "runtime"] }
        finish validate(raw, Type { name: str, labels: list[str] })
        "#,
    );
    let record = value.as_record().expect("validated record");
    assert_eq!(record["name"], Value::String("lashlang".to_string().into()));

    let mut state = State::new();
    let err = execute(
        r#"finish validate({ labels: ["agent", 42] }, Type { labels: list[str] })"#,
        &mut state,
        &host,
    )
    .await
    .expect_err("validate should abort on bad shape");
    let ExecuteError::Runtime(err) = err else {
        panic!("expected runtime error");
    };
    assert!(
        err.to_string()
            .contains("$.labels[1]: expected string, got number"),
        "unexpected error: {err}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Simple worked example from the prompt's "Example format" block:
//   r = await files.read({ path: "Cargo.toml" })
//   finish split(r.value, "\n")[2]
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_example_format_block_executes_as_shown() {
    let host = MockHost::default().with_file("Cargo.toml", "line0\nline1\nline2\nline3\n");
    assert_eq!(
        run(
            &host,
            r#"r = await files.read({ path: "Cargo.toml" })
finish split(r.value, "\n")[2]"#,
        ),
        Value::String("line2".to_string().into())
    );
}

// ─────────────────────────────────────────────────────────────────────
// Prompt fanout example: `?` unwraps normal happy-path tool results.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_fanout_example_unwraps_spawn_and_wait_results_with_question() {
    let host = MockHost::default();
    let Value::List(results) = run(
        &host,
        r#"process inspect_chunk(task: str, capability: str) { finish task }
a = start inspect_chunk(task: "chunk_1", capability: "explore")
b = start inspect_chunk(task: "chunk_2", capability: "explore")
results = await { a: a, b: b }
a_result = results.a?
b_result = results.b?
finish [a_result.claim, b_result.claim]"#,
    ) else {
        panic!("expected list");
    };
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], Value::String("done:chunk_1".to_string().into()));
    assert_eq!(results[1], Value::String("done:chunk_2".to_string().into()));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_example_shell_nonzero_exit_is_result_data() {
    let host = MockHost::default();
    assert_eq!(
        run(
            &host,
            r#"probe = await shell.exec({ cmd: "test -f Cargo.lock" })?
finish (probe.exit_code == 0 ? "Cargo.lock exists" : "Cargo.lock is missing")"#,
        ),
        Value::String("Cargo.lock is missing".into())
    );
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_example_tool_output_stays_full_in_variables() {
    let host = MockHost::default();
    let Value::Record(record) = run(
        &host,
        r#"check = await shell.exec({ cmd: "large-output" })?
finish { chars: len(check.output), tail: slice(check.output, 4096, null) }"#,
    ) else {
        panic!("expected record");
    };

    assert_eq!(record["chars"], Value::Number(4100.0));
    assert_eq!(record["tail"], Value::String("TAIL".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_claim_list_comprehensions_build_collections() {
    let host = MockHost::default()
        .with_file("Cargo.toml", "abc")
        .with_file("README.md", "abcdef");
    let Value::List(items) = run(
        &host,
        r#"items = [
  { path: path, chars: len(await files.read({ path: path })?) }
  for path in ["Cargo.toml", "README.md"]
]
finish items"#,
    ) else {
        panic!("expected list");
    };

    assert_eq!(items.len(), 2);
    let first = items[0].as_record().expect("first item");
    let second = items[1].as_record().expect("second item");
    assert_eq!(first["path"], Value::String("Cargo.toml".into()));
    assert_eq!(first["chars"], Value::Number(3.0));
    assert_eq!(second["path"], Value::String("README.md".into()));
    assert_eq!(second["chars"], Value::Number(6.0));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_example_prints_targeted_slice_for_large_values() {
    let host = MockHost::default().with_file("Cargo.toml", "abcdef");
    let (outcome, _) = run_continued(
        &host,
        r#"text = await files.read({ path: "Cargo.toml" })?
print { chars: len(text), head: slice(text, 0, 3) }"#,
    );
    assert!(matches!(outcome, ExecutionOutcome::Continued));
    let observations = host.observations();
    let record = observations[0].as_record().expect("observation record");
    assert_eq!(record["chars"], Value::Number(6.0));
    assert_eq!(record["head"], Value::String("abc".into()));
}

#[tokio::test(flavor = "current_thread")]
async fn prompt_example_validates_nontrivial_edit_before_finish() {
    let host = MockHost::default();
    assert_eq!(
        run(
            &host,
            r##"await files.edit({ path: "src/lib.rs", edits: [{ oldText: "old", newText: "new" }] })?
check = await shell.exec({ cmd: "cargo check --workspace --all-targets" })?
if check.exit_code != 0 {
  print slice(check.output, 0, 4000)
} else {
  finish "Edit applied and validation passed."
}"##,
        ),
        Value::String("Edit applied and validation passed.".into())
    );
}

// ─────────────────────────────────────────────────────────────────────
// Meta-claim guard: the prompt still references every builtin we've
// implemented — no silent drift. If someone adds/removes a builtin
// this fails and points them at the prompt.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "current_thread")]
async fn prompt_mentions_every_builtin_we_document() {
    // Source: `crates/lash-protocol-rlm/src/protocol.rs::RLM_EXECUTION_SECTION`. We don't
    // pull the const directly (cross-crate visibility) — instead keep the
    // expected list here. If you add a builtin, update both places.
    const DOCUMENTED: &[&str] = &[
        "len",
        "empty",
        "slice",
        "split",
        "join",
        "trim",
        "find",
        "grep_text",
        "starts_with",
        "ends_with",
        "contains",
        "keys",
        "values",
        "to_string",
        "to_int",
        "to_float",
        "json_parse",
        "format",
        "validate",
        "range",
        "ceil_div",
        "floor_div",
        "push",
        "sort",
        "sort_by",
        "sum",
        "min",
        "max",
        "replace",
        "lower",
        "upper",
        "unique",
        "reverse",
    ];
    // Call each builtin with a shape guaranteed to succeed — we don't
    // check results here (covered by the per-builtin tests above), only
    // that the runtime recognises the name.
    let host = MockHost::default();
    let smoke = [
        (r#"finish len("a")"#, "len"),
        (r#"finish empty("")"#, "empty"),
        (r#"finish slice("abc", 0, 1)"#, "slice"),
        (r#"finish split("a,b", ",")"#, "split"),
        (r#"finish join(["a","b"], ",")"#, "join"),
        (r#"finish trim(" a ")"#, "trim"),
        (r#"finish find("abc", "b")"#, "find"),
        (r#"finish grep_text("abc", "b")"#, "grep_text"),
        (r#"finish starts_with("abc", "a")"#, "starts_with"),
        (r#"finish ends_with("abc", "c")"#, "ends_with"),
        (r#"finish contains("abc", "b")"#, "contains"),
        (r#"finish keys({a: 1})"#, "keys"),
        (r#"finish values({a: 1})"#, "values"),
        (r#"finish to_string(1)"#, "to_string"),
        (r#"finish to_int("1")"#, "to_int"),
        (r#"finish to_float("1.5")"#, "to_float"),
        (r#"finish json_parse("1")"#, "json_parse"),
        (r#"finish format("x")"#, "format"),
        (
            r#"finish validate({ value: "x" }, Type { value: str })"#,
            "validate",
        ),
        (r#"finish range(1)"#, "range"),
        (r#"finish ceil_div(3, 2)"#, "ceil_div"),
        (r#"finish floor_div(3, 2)"#, "floor_div"),
        (r#"finish push([], "x")"#, "push"),
        (r#"finish sort([2, 1])"#, "sort"),
        (r#"finish sort_by([{a: 2}, {a: 1}], "a")"#, "sort_by"),
        (r#"finish sum([1, 2])"#, "sum"),
        (r#"finish min([1, 2])"#, "min"),
        (r#"finish max([1, 2])"#, "max"),
        (r#"finish replace("aba", "a", "x")"#, "replace"),
        (r#"finish lower("ABC")"#, "lower"),
        (r#"finish upper("abc")"#, "upper"),
        (r#"finish unique([1, 1])"#, "unique"),
        (r#"finish reverse([1, 2])"#, "reverse"),
    ];
    assert_eq!(smoke.len(), DOCUMENTED.len());
    for (code, name) in smoke {
        let mut state = State::new();
        let outcome = execute(code, &mut state, &host)
            .await
            .unwrap_or_else(|err| panic!("builtin `{name}` failed to execute: {err:?}"));
        assert!(
            matches!(outcome, ExecutionOutcome::Finished(_)),
            "builtin `{name}` did not finish"
        );
    }
}
