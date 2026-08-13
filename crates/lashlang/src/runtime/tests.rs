use super::vm::{VM_CONTINUATION_FORMAT_VERSION, VmFrameReturnContinuation};
use super::*;
use crate::ast::{Expr, Program};
use lash_sansio::sync::{LockResultExt, MutexExt};
use std::fmt::Write as _;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Host;

impl Host {
    fn perform_resource_operation(
        operation: ResourceOperation,
    ) -> Result<Value, ExecutionHostError> {
        match operation.operation.as_str() {
            "echo" => {
                let value = operation
                    .args
                    .first()
                    .and_then(Value::as_record)
                    .and_then(|record| record.get("value"))
                    .cloned()
                    .unwrap_or(Value::Null);
                Ok(value)
            }
            "err" => Err(ExecutionHostError::new("boom")),
            other => Err(ExecutionHostError::new(format!(
                "unknown module operation: {other}"
            ))),
        }
    }
}

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(operation) => {
                Self::perform_resource_operation(operation).map(AbilityResult::Value)
            }
            AbilityOp::ResourceOperationBatch(batch) => Ok(AbilityResult::ResourceOperationBatch(
                ResourceOperationBatchResult {
                    results: batch
                        .operations
                        .into_iter()
                        .map(|operation| {
                            ResourceOperationResult::from_result(Self::perform_resource_operation(
                                operation,
                            ))
                        })
                        .collect(),
                },
            )),
            AbilityOp::Await(handle) => match handle {
                Value::Record(_) => Ok(AbilityResult::Value(Value::Null)),
                _ => Err(ExecutionHostError::new("expected handle record")),
            },
            AbilityOp::Print(_) => Ok(AbilityResult::Unit),
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

#[derive(Default)]
struct RejectingAwaitHost;

impl ExecutionHost for RejectingAwaitHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Await(_) => Err(ExecutionHostError::new(
                "unexpected generic handle await in resource operation test",
            )),
            other => Host.perform(other).await,
        }
    }
}

struct SlowToolHost;

impl ExecutionHost for SlowToolHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        if matches!(op, AbilityOp::ResourceOperation(_)) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Host.perform(op).await
    }
}

#[derive(Default)]
struct RecordingProcessHost {
    starts: Mutex<Vec<ProcessStart>>,
    events: Mutex<Vec<ProcessEvent>>,
    sleeps: Mutex<Vec<Sleep>>,
    signals: Mutex<Vec<ProcessSignal>>,
}

impl ExecutionHost for RecordingProcessHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(_) | AbilityOp::ResourceOperationBatch(_) => Err(
                ExecutionHostError::new("module operations are not supported by this host"),
            ),
            AbilityOp::StartProcess(start) => {
                self.starts.lock_recover().push(*start);
                let mut handle = Record::new();
                handle.insert("__handle__".to_string(), Value::String("process".into()));
                handle.insert("id".to_string(), Value::String("proc-1".into()));
                Ok(AbilityResult::Value(Value::Record(Arc::new(handle))))
            }
            AbilityOp::ProcessEvent(event) => {
                self.events.lock_recover().push(event);
                Ok(AbilityResult::Unit)
            }
            AbilityOp::Sleep(sleep) => {
                self.sleeps.lock_recover().push(sleep);
                Ok(AbilityResult::Value(Value::Null))
            }
            AbilityOp::WaitSignal { name } => {
                assert_eq!(name, "ready");
                Ok(AbilityResult::Value(Value::String("signal-payload".into())))
            }
            AbilityOp::SignalRun(signal) => {
                self.signals.lock_recover().push(signal);
                Ok(AbilityResult::Value(Value::Null))
            }
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

async fn exec(source: &str) -> Result<Value, RuntimeError> {
    let program = crate::parse(source).expect("program should parse");
    let mut state = State::new();
    match execute_program(&program, &mut state, &Host).await? {
        ExecutionOutcome::Finished(value) => Ok(value),
        ExecutionOutcome::Continued => panic!("expected `finish` in test program"),
        ExecutionOutcome::Failed(value) => panic!("unexpected process failure: {value}"),
    }
}

async fn exec_outcome(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    let program = crate::parse(source).expect("program should parse");
    let mut state = State::new();
    execute_program(&program, &mut state, &Host).await
}

#[tokio::test(flavor = "current_thread")]
async fn instruction_budget_exhaustion_is_typed_and_caret_rendered() {
    let source = "i = 0\nwhile i < 5000 { i = i + 1 }\nfinish i";
    let program = crate::parse(source).expect("program should parse");
    let env = ExecutionEnvironment::new(&Host)
        .traced()
        .with_execution_bounds(ExecutionBounds::new(
            ExecutionBound::instructions(1),
            ExecutionBound::Unbounded,
            ExecutionBound::Unbounded,
        ));
    let mut state = State::new();
    let error = execute_program(&program, &mut state, &env)
        .await
        .expect_err("long loop must exhaust its instruction budget");
    assert!(matches!(
        error,
        RuntimeError::InstructionBudgetExceeded { limit: 1 }
    ));
    let failure = env.take_runtime_failure().expect("traced runtime failure");
    let diagnostic = crate::format_runtime_diagnostic(source, &failure.error, failure.span);
    assert!(diagnostic.contains("instruction budget of 1 instructions exceeded"));
    assert!(diagnostic.contains('^'), "{diagnostic}");
}

#[tokio::test(flavor = "current_thread")]
async fn effect_free_terminal_segment_enforces_tiny_instruction_budget() {
    let program = crate::parse("value = 1").expect("program should parse");
    let env = ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
        ExecutionBound::instructions(1),
        ExecutionBound::Unbounded,
        ExecutionBound::Unbounded,
    ));
    let mut state = State::new();
    assert!(matches!(
        execute_program(&program, &mut state, &env).await,
        Err(RuntimeError::InstructionBudgetExceeded { limit: 1 })
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn effect_free_intrinsic_dispatch_enforces_bounds_before_later_runtime_errors() {
    let program = crate::parse("values = unique(range(0, 4000))\nfinish values.missing")
        .expect("program should parse");
    let env = ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
        ExecutionBound::instructions(10),
        ExecutionBound::Unbounded,
        ExecutionBound::Unbounded,
    ));
    let mut state = State::new();
    assert!(matches!(
        execute_program(&program, &mut state, &env).await,
        Err(RuntimeError::InstructionBudgetExceeded { limit: 10 })
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn shaping_collection_work_consumes_instruction_budget() {
    let program = crate::parse("finish unique(range(0, 4000))").expect("program should parse");
    let env = ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
        ExecutionBound::instructions(10),
        ExecutionBound::Unbounded,
        ExecutionBound::Unbounded,
    ));
    let mut state = State::new();
    assert!(matches!(
        execute_program(&program, &mut state, &env).await,
        Err(RuntimeError::InstructionBudgetExceeded { limit: 10 })
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn deadline_excludes_awaited_tool_time() {
    let source = r#"
value = tools.echo({ value: 1 })
i = 0
while i < 5000 { i = i + 1 }
finish value
"#;
    let program = crate::parse(source).expect("program should parse");
    let env = ExecutionEnvironment::new(&SlowToolHost).with_execution_bounds(ExecutionBounds::new(
        ExecutionBound::Unbounded,
        ExecutionBound::millis(20),
        ExecutionBound::Unbounded,
    ));
    let mut state = State::new();
    let outcome = execute_program(&program, &mut state, &env)
        .await
        .expect("the tool's 50ms wait must not consume the 20ms VM deadline");
    assert!(matches!(outcome, ExecutionOutcome::Finished(_)));
}

#[tokio::test(flavor = "current_thread")]
async fn deadline_exhaustion_is_a_typed_runtime_error() {
    let source = "i = 0\nwhile i < 5000 { i = i + 1 }\nfinish i";
    let program = crate::parse(source).expect("program should parse");
    let env = ExecutionEnvironment::new(&Host).with_execution_bounds(ExecutionBounds::new(
        ExecutionBound::Unbounded,
        ExecutionBound::Bounded(std::time::Duration::from_nanos(1)),
        ExecutionBound::Unbounded,
    ));
    let mut state = State::new();
    let error = execute_program(&program, &mut state, &env)
        .await
        .expect_err("loop must exhaust its VM deadline");
    assert!(matches!(
        error,
        RuntimeError::ExecutionDeadlineExceeded { .. }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn string_membership_rejects_non_string_needles_without_coercion() {
    let compiled = crate::compile("finish 1 in \"123\"").expect("source should compile");
    let mut state = State::new();
    let error = execute(&compiled, &mut state, &Host)
        .await
        .expect_err("numeric string needles must not be coerced");
    assert_eq!(error, RuntimeError::InUnsupported);
}

fn compile_source(source: &str) -> Result<CompiledProgram, crate::ParseError> {
    let program = crate::parse(source)?;
    if source.contains("tools.")
        && let Ok(linked) = crate::LinkedModule::link(program.clone(), runtime_test_environment())
    {
        Ok(crate::compile_linked(&linked))
    } else {
        Ok(compile_program(&program))
    }
}

fn compile_labeled_source(source: &str) -> CompiledProgram {
    let program = crate::parse(source).expect("program should parse");
    let surface = runtime_test_environment().with_language_features(
        crate::LashlangLanguageFeatures::default().with_label_annotations(),
    );
    let linked = crate::LinkedModule::link(program, surface).expect("program should link");
    crate::compile_linked(&linked)
}

fn compile_labeled_process_source(source: &str, process_name: &str) -> CompiledProgram {
    let program = crate::parse(source).expect("program should parse");
    let surface = runtime_test_environment().with_language_features(
        crate::LashlangLanguageFeatures::default().with_label_annotations(),
    );
    let linked = crate::LinkedModule::link(program, surface).expect("program should link");
    crate::compile_linked_process(&linked, process_name).expect("process should compile")
}

fn assert_resource_call_unwrap_without_handle_await(compiled: &CompiledProgram) {
    let instructions = compiled_instruction_listing(compiled);
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::ResourceCallUnwrap { .. })),
        "compiled code should use ResourceCallUnwrap:\n{instructions}"
    );
    assert!(
        !compiled.chunk.code.iter().any(|instruction| matches!(
            instruction,
            Instruction::AwaitHandle | Instruction::AwaitHandleUnwrap
        )),
        "resource operation unwrap should not emit generic handle await instructions:\n{instructions}"
    );
}

fn compiled_instruction_listing(compiled: &CompiledProgram) -> String {
    let mut out = String::new();
    for (index, instruction) in compiled.chunk.code.iter().copied().enumerate() {
        writeln!(
            out,
            "{index:04}: {}",
            instruction_snapshot(&compiled.chunk, instruction)
        )
        .unwrap();
    }
    out
}

fn compile_program(program: &Program) -> CompiledProgram {
    super::entry_points::compile_program_internal(program)
}

fn runtime_test_environment() -> crate::LashlangHostEnvironment {
    let mut resources = crate::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "echo",
            "echo",
            crate::TypeExpr::Any,
            crate::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "err",
            "err",
            crate::TypeExpr::Any,
            crate::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "missing",
            "missing",
            crate::TypeExpr::Any,
            crate::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["tools"],
            "Tools",
            "spawn",
            "spawn",
            crate::TypeExpr::Any,
            crate::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    crate::LashlangHostEnvironment::new(resources, crate::LashlangAbilities::all())
}

async fn execute_program<H: ExecutionHost>(
    program: &Program,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, RuntimeError> {
    if let Ok(linked) = crate::LinkedModule::link(program.clone(), runtime_test_environment()) {
        let compiled = crate::compile_linked(&linked);
        return super::execute(&compiled, state, host).await;
    }
    super::execute(program, state, host).await
}

async fn execute_compiled<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, RuntimeError> {
    super::execute(program, state, host).await
}

async fn execute_compiled_with_projected_bindings<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
    projected: &ProjectedBindings,
) -> Result<ExecutionOutcome, RuntimeError> {
    let env = ExecutionEnvironment::new(host).with_projected_bindings(projected.clone());
    super::execute(program, state, &env).await
}

async fn execute_compiled_with_scratch<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
    scratch: &mut ExecutionScratch,
) -> Result<ExecutionOutcome, RuntimeError> {
    let env = ExecutionEnvironment::new(host).with_scratch(std::mem::take(scratch));
    let result = super::execute(program, state, &env).await;
    *scratch = env.take_recycled_scratch().unwrap_or_default();
    result
}

async fn execute_compiled_traced<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, RuntimeFailure> {
    let env = ExecutionEnvironment::new(host).traced();
    match super::execute(program, state, &env).await {
        Ok(outcome) => Ok(outcome),
        Err(error) => Err(env
            .take_runtime_failure()
            .unwrap_or(RuntimeFailure { error, span: None })),
    }
}

async fn execute_compiled_traced_with_projected_bindings<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
    projected: &ProjectedBindings,
) -> Result<ExecutionOutcome, RuntimeFailure> {
    let env = ExecutionEnvironment::new(host)
        .traced()
        .with_projected_bindings(projected.clone());
    match super::execute(program, state, &env).await {
        Ok(outcome) => Ok(outcome),
        Err(error) => Err(env
            .take_runtime_failure()
            .unwrap_or(RuntimeFailure { error, span: None })),
    }
}

async fn execute_compiled_process<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
) -> Result<ExecutionOutcome, RuntimeError> {
    let env = ExecutionEnvironment::new(host).process();
    super::execute(program, state, &env).await
}

async fn profile_compiled<H: ExecutionHost>(
    program: &CompiledProgram,
    state: &mut State,
    host: &H,
) -> Result<(ExecutionOutcome, ProfileReport), RuntimeError> {
    let env = ExecutionEnvironment::new(host).profiled();
    let outcome = super::execute(program, state, &env).await?;
    let profile = env.take_profile().expect("profile should be recorded");
    Ok((outcome, profile))
}

const GOLDEN_CONTRACT_SOURCE: &str = r#"
source = join(history, ",")
beta_index = find(source, "beta")
matches = grep_text(source, "beta")
counts = {}
for token in split(source, ",") {
  counts[token] = counts[token] + 1
}
Payload = Type {
  beta_index: int | null,
  matches: list[dict],
  counts: dict
}
finish validate(
  { beta_index: beta_index, matches: matches, counts: counts },
  Payload
)
"#;

#[test]
fn golden_parser_ast_contract_covers_lashlang_host_environment() {
    let program = crate::parse(GOLDEN_CONTRACT_SOURCE).expect("program should parse");
    insta::assert_snapshot!(
        "lashlang_parser_ast_contract",
        serde_json::to_string_pretty(&program).expect("program should serialize")
    );
}

#[test]
fn golden_compiled_bytecode_contract_covers_lashlang_host_environment() {
    insta::assert_snapshot!(
        "lashlang_compiled_bytecode_contract",
        compiled_program_snapshot(GOLDEN_CONTRACT_SOURCE)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn golden_runtime_diagnostic_contract_is_exact() {
    insta::assert_snapshot!(
        "lashlang_runtime_diagnostic_contract",
        runtime_diagnostic("x = 1\nfinish len(true)").await
    );
}

#[tokio::test(flavor = "current_thread")]
async fn golden_lashlang_diagnostic_corpus_is_exact() {
    let mut projected = ProjectedBindings::new();
    projected.insert(
        "history",
        ProjectedValue::scalar(
            "history",
            Value::List(vec![Value::String("entry".into())].into()),
        ),
    );

    let mut cases = Vec::new();
    cases.push(diagnostic_case(
        "parse_inline_if",
        format_parse_diagnostic("finish if true { 1 }"),
    ));
    cases.push(diagnostic_case(
        "parse_inline_for",
        format_parse_diagnostic("finish [for x in [1] { x }]"),
    ));
    cases.push(diagnostic_case(
        "parse_loop_control_outside_loop",
        format_parse_diagnostic("break"),
    ));
    cases.push(diagnostic_case(
        "parse_duplicate_type_field",
        format_parse_diagnostic("Payload = Type { nested: { ok: bool, ok: str } }"),
    ));
    cases.push(diagnostic_case(
        "parse_removed_parallel_keyword",
        format_parse_diagnostic("parallel {\n  start echo(value: 1)\n}"),
    ));
    cases.push(diagnostic_case(
        "link_membership_requires_container",
        link_diagnostic("finish 1 in 2"),
    ));
    cases.push(diagnostic_case(
        "link_membership_requires_list_item_type",
        link_diagnostic("finish \"one\" in [1, 2]"),
    ));
    cases.push(diagnostic_case(
        "link_record_membership_requires_string_key",
        link_diagnostic("finish 1 in { one: true }"),
    ));
    cases.push(diagnostic_case(
        "runtime_sort_mixed_types",
        runtime_diagnostic("finish sort([1, \"two\"])").await,
    ));
    cases.push(diagnostic_case(
        "runtime_sort_by_missing_path",
        runtime_diagnostic("finish sort_by([{ profile: { score: 1 } }, {}], \"profile.score\")")
            .await,
    ));
    cases.push(diagnostic_case(
        "runtime_sort_by_not_a_record",
        runtime_diagnostic("finish sort_by([{ score: 1 }, 2], \"score\")").await,
    ));
    cases.push(diagnostic_case(
        "runtime_sort_by_empty_path",
        runtime_diagnostic("finish sort_by([{ score: 1 }], \"\")").await,
    ));
    cases.push(diagnostic_case(
        "runtime_sum_non_number",
        runtime_diagnostic("finish sum([1, \"two\"])").await,
    ));
    cases.push(diagnostic_case(
        "runtime_min_incomparable",
        runtime_diagnostic("finish min([[1], [2]])").await,
    ));
    cases.push(diagnostic_case(
        "runtime_min_empty_list",
        runtime_diagnostic("finish min([])").await,
    ));
    cases.push(diagnostic_case(
        "runtime_max_requires_list",
        runtime_diagnostic("finish max({ value: 1 })").await,
    ));
    cases.push(diagnostic_case(
        "runtime_replace_requires_text",
        runtime_diagnostic("finish replace(1, \"1\", \"one\")").await,
    ));
    cases.push(diagnostic_case(
        "runtime_lower_requires_text",
        runtime_diagnostic("finish lower(false)").await,
    ));
    cases.push(diagnostic_case(
        "runtime_upper_requires_text",
        runtime_diagnostic("finish upper(1)").await,
    ));
    cases.push(diagnostic_case(
        "runtime_unique_requires_list",
        runtime_diagnostic("finish unique(1)").await,
    ));
    cases.push(diagnostic_case(
        "runtime_reverse_requires_list",
        runtime_diagnostic("finish reverse(1)").await,
    ));
    cases.push(diagnostic_case(
        "runtime_bad_wrapper_unwrap",
        runtime_diagnostic("finish ({ ok: false, error: \"boom\" })?").await,
    ));
    cases.push(diagnostic_case(
        "runtime_failed_resource_operation_unwrap",
        runtime_diagnostic("finish (await tools.err({})?)").await,
    ));
    cases.push(diagnostic_case(
        "runtime_invalid_await_handle",
        runtime_diagnostic("finish (await 1)?").await,
    ));
    cases.push(diagnostic_case(
        "runtime_read_only_projected_assignment",
        runtime_diagnostic_with_projected("history = []\nfinish history", &projected).await,
    ));
    cases.push(diagnostic_case(
        "runtime_invalid_validate_type_argument",
        runtime_diagnostic("finish validate({ ok: true }, \"not a type\")").await,
    ));
    cases.push(diagnostic_case(
        "runtime_validation_failure",
        runtime_diagnostic("finish validate({ count: \"x\" }, Type { count: int })").await,
    ));
    cases.push(diagnostic_case(
        "runtime_unknown_name",
        runtime_diagnostic("finish missing").await,
    ));
    cases.push(diagnostic_case(
        "runtime_unknown_builtin",
        runtime_diagnostic("finish nope()").await,
    ));
    cases.push(diagnostic_case(
        "link_unknown_resource_operation",
        link_diagnostic("finish await tools.does_not_exist({})?"),
    ));
    cases.push(diagnostic_case(
        "link_unresolved_receiver",
        link_diagnostic("value = 1\nfinish await value.echo({})?"),
    ));

    insta::assert_snapshot!("lashlang_diagnostic_corpus", cases.join("\n\n---\n\n"));
}

#[tokio::test(flavor = "current_thread")]
async fn labeled_aggregate_await_failure_points_at_failing_leaf_not_label() {
    let source = r#"@label(title: "Aggregate")
result = await {
  ok: tools.echo({ value: "ok" })?,
  bad: tools.err({})?
}
finish result"#;
    let compiled = compile_labeled_source(source);
    let mut state = State::new();
    let failure = execute_compiled_traced(&compiled, &mut state, &Host)
        .await
        .expect_err("later aggregate leaf should fail");
    let message = crate::format_runtime_diagnostic(source, &failure.error, failure.span);

    assert!(
        message.contains("`?` unwrapped failed module operation: boom"),
        "{message}"
    );
    assert!(message.contains("--> line 4, column 8"), "{message}");
    assert!(message.contains("bad: tools.err({})?"), "{message}");
    assert!(message.contains("       ^~~~~~~~~~~~~~"), "{message}");
    assert!(!message.contains("--> line 1"), "{message}");
}

fn format_parse_diagnostic(source: &str) -> String {
    let err = crate::parse(source).expect_err("parse should fail");
    crate::format_parse_diagnostic(source, &err)
}

fn link_diagnostic(source: &str) -> String {
    let program = crate::parse(source).expect("diagnostic source should parse");
    let error = crate::LinkedModule::link(program, runtime_test_environment())
        .expect_err("diagnostic source should fail to link");
    crate::format_link_diagnostic(source, &error)
}

async fn runtime_diagnostic(source: &str) -> String {
    runtime_diagnostic_with_projected(source, &ProjectedBindings::default()).await
}

async fn runtime_diagnostic_with_projected(source: &str, projected: &ProjectedBindings) -> String {
    let compiled = compile_source(source).expect("source should compile");
    let mut state = State::new();
    let failure =
        execute_compiled_traced_with_projected_bindings(&compiled, &mut state, &Host, projected)
            .await
            .expect_err("runtime should fail");
    crate::format_runtime_diagnostic(source, &failure.error, failure.span)
}

fn diagnostic_case(name: &str, diagnostic: String) -> String {
    format!("{name}\n{diagnostic}")
}

fn compiled_program_snapshot(source: &str) -> String {
    let compiled = compile_source(source).expect("source should compile");
    let chunk = &compiled.chunk;
    let mut out = String::new();

    let stats = compiled.compile_stats();
    writeln!(
        out,
        "compile_stats: total={} const_folded={} dynamic={} refs={}",
        stats.type_literals_total,
        stats.type_literals_const_folded,
        stats.type_literals_dynamic,
        stats.type_ref_sites
    )
    .unwrap();
    writeln!(
        out,
        "slots: [{}]",
        chunk
            .slot_names
            .iter()
            .map(|name| name.text.as_ref())
            .collect::<Vec<_>>()
            .join(", ")
    )
    .unwrap();
    writeln!(
        out,
        "names: [{}]",
        chunk
            .names
            .iter()
            .map(|name| name.text.as_ref())
            .collect::<Vec<_>>()
            .join(", ")
    )
    .unwrap();
    writeln!(out, "constants:").unwrap();
    for (index, value) in chunk.constants.iter().enumerate() {
        writeln!(out, "  c{index}: {}", compact_json(value)).unwrap();
    }
    writeln!(out, "code:").unwrap();
    for (index, instruction) in chunk.code.iter().copied().enumerate() {
        writeln!(
            out,
            "  {index:04}: {}",
            instruction_snapshot(chunk, instruction)
        )
        .unwrap();
    }

    out
}

fn instruction_snapshot(chunk: &Chunk, instruction: Instruction) -> String {
    match instruction {
        Instruction::PushConst(index) => {
            format!(
                "push_const c{index} {}",
                compact_json(&chunk.constants[index])
            )
        }
        Instruction::PushNull => "push_null".to_string(),
        Instruction::PushBool(value) => format!("push_bool {value}"),
        Instruction::PushNumber(value) => format!("push_number {value}"),
        Instruction::LoadName(slot) => format!("load_name {slot}:{}", slot_name(chunk, slot)),
        Instruction::DeepCopy => "deep_copy".to_string(),
        Instruction::StoreName(slot) => format!("store_name {slot}:{}", slot_name(chunk, slot)),
        Instruction::StoreConst { slot, constant } => format!(
            "store_const {slot}:{} c{constant} {}",
            slot_name(chunk, slot),
            compact_json(&chunk.constants[constant])
        ),
        Instruction::BuildTuple(count) => format!("build_tuple {count}"),
        Instruction::BuildList(count) => format!("build_list {count}"),
        Instruction::ListAppend => "list_append".to_string(),
        Instruction::BuildRecord(keys) => format!("build_record {}", keys_snapshot(chunk, keys)),
        Instruction::LoadField { slot, field } => format!(
            "load_field {slot}:{} .{}",
            slot_name(chunk, slot),
            name(chunk, field)
        ),
        Instruction::LoadFieldUnwrap { slot, field } => format!(
            "load_field_unwrap {slot}:{} .{}",
            slot_name(chunk, slot),
            name(chunk, field)
        ),
        Instruction::Field(field) => format!("field .{}", name(chunk, field)),
        Instruction::Index => "index".to_string(),
        Instruction::PathAssign { slot, path } => format!(
            "path_assign {slot}:{} {}",
            slot_name(chunk, slot),
            assign_path_snapshot(chunk, path)
        ),
        Instruction::ResultUnwrap => "result_unwrap".to_string(),
        Instruction::Unary(op) => format!("unary {op:?}"),
        Instruction::Binary(op) => format!("binary {op:?}"),
        Instruction::SlotNumberBinary { slot, op, right } => format!(
            "slot_number_binary {slot}:{} {op:?} {right}",
            slot_name(chunk, slot)
        ),
        Instruction::SlotNumberCompare { slot, op, right } => format!(
            "slot_number_compare {slot}:{} {op:?} {right}",
            slot_name(chunk, slot)
        ),
        Instruction::SlotNumberBinaryCompare {
            slot,
            binary_op,
            binary_right,
            compare_op,
            compare_right,
        } => format!(
            "slot_number_binary_compare {slot}:{} {binary_op:?} {binary_right} {compare_op:?} {compare_right}",
            slot_name(chunk, slot)
        ),
        Instruction::ToBool => "to_bool".to_string(),
        Instruction::Jump(target) => format!("jump {target}"),
        Instruction::JumpIfFalse(target) => format!("jump_if_false {target}"),
        Instruction::JumpIfCompareFalse { op, target } => {
            format!("jump_if_compare_false {op:?} {target}")
        }
        Instruction::JumpIfSlotNumberCompareFalse {
            slot,
            op,
            right,
            target,
        } => format!(
            "jump_if_slot_number_compare_false {slot}:{} {op:?} {right} {target}",
            slot_name(chunk, slot)
        ),
        Instruction::JumpIfSlotNumberBinaryCompareFalse {
            slot,
            binary_op,
            binary_right,
            compare_op,
            compare_right,
            target,
        } => format!(
            "jump_if_slot_number_binary_compare_false {slot}:{} {binary_op:?} {binary_right} {compare_op:?} {compare_right} {target}",
            slot_name(chunk, slot)
        ),
        Instruction::JumpIfTrue(target) => format!("jump_if_true {target}"),
        Instruction::ResourceCall { operation, argc } => {
            format!("resource_call {} argc={argc}", name_text(chunk, operation))
        }
        Instruction::ResourceCallUnwrap { operation, argc } => {
            format!(
                "resource_call_unwrap {} argc={argc}",
                name_text(chunk, operation)
            )
        }
        Instruction::ResourceOperationBatch(batch) => {
            let batch = &chunk.resource_operation_batches[batch];
            format!(
                "resource_operation_batch leaves={} values={} unwrap={}",
                batch.leaves.len(),
                batch.stack_value_count,
                batch.aggregate_unwrap
            )
        }
        Instruction::StartProcess { process, keys } => format!(
            "start_process {} {}",
            name_text(chunk, process),
            keys_snapshot(chunk, keys)
        ),
        Instruction::AwaitHandle => "await_handle".to_string(),
        Instruction::SleepFor => "sleep_for".to_string(),
        Instruction::SleepUntil => "sleep_until".to_string(),
        Instruction::ProcessWaitSignal { name } => {
            format!("process_wait_signal {}", name_text(chunk, name))
        }
        Instruction::ProcessSignalRun { name } => {
            format!("process_signal_run {}", name_text(chunk, name))
        }
        Instruction::AwaitHandleUnwrap => "await_handle_unwrap".to_string(),
        Instruction::CancelHandle => "cancel_handle".to_string(),
        Instruction::Intrinsic(op) => intrinsic_snapshot(chunk, op),
        Instruction::AddAssign(slot) => format!("add_assign {slot}:{}", slot_name(chunk, slot)),
        Instruction::AddAssignNumber { slot, right } => {
            format!(
                "add_assign_number {slot}:{} {right}",
                slot_name(chunk, slot)
            )
        }
        Instruction::AddAssignSlot { slot, right } => format!(
            "add_assign_slot {slot}:{} {right}:{}",
            slot_name(chunk, slot),
            slot_name(chunk, right)
        ),
        Instruction::AddAssignIndexNumber { slot, right } => format!(
            "add_assign_index_number {slot}:{} {right}",
            slot_name(chunk, slot)
        ),
        Instruction::AddAssignIndexSlotNumber { slot, index, right } => format!(
            "add_assign_index_slot_number {slot}:{} {index}:{} {right}",
            slot_name(chunk, slot),
            slot_name(chunk, index)
        ),
        Instruction::AppendAssign(slot) => {
            format!("append_assign {slot}:{}", slot_name(chunk, slot))
        }
        Instruction::Print => "print".to_string(),
        Instruction::Finish => "finish".to_string(),
        Instruction::ProcessYield => "process_yield".to_string(),
        Instruction::ProcessWake => "process_wake".to_string(),
        Instruction::ProcessFail => "process_fail".to_string(),
        Instruction::ObserveStep => "observe_step".to_string(),
        Instruction::Pop => "pop".to_string(),
        Instruction::BeginIter(slot) => format!("begin_iter {slot}:{}", slot_name(chunk, slot)),
        Instruction::BeginRangeIter { binding, argc } => {
            format!(
                "begin_range_iter {binding}:{} argc={argc}",
                slot_name(chunk, binding)
            )
        }
        Instruction::IterNext { jump_to } => format!("iter_next {jump_to}"),
        Instruction::DeepCopyLoopBinding(slot) => format!("deep_copy_loop_binding ${slot}"),
        Instruction::EndIter => "end_iter".to_string(),
        Instruction::ResolveTypeRef(slot) => {
            format!("resolve_type_ref {slot}:{}", slot_name(chunk, slot))
        }
        Instruction::WrapTypeLiteral => "wrap_type_literal".to_string(),
        Instruction::WrapHostDescriptor(type_name) => {
            format!("wrap_host_descriptor {}", name_text(chunk, type_name))
        }
        Instruction::MakeClosure { function, captures } => {
            format!("make_closure {function} captures={captures}")
        }
        Instruction::Call { argc } => format!("call argc={argc}"),
        Instruction::Map => "map_callback".to_string(),
        Instruction::Return => "return".to_string(),
        Instruction::PushHandler { .. } => "push_handler".to_string(),
        Instruction::PopHandler => "pop_handler".to_string(),
        Instruction::EnterFinally { .. } => "enter_finally".to_string(),
        Instruction::EndFinally => "end_finally".to_string(),
        Instruction::AbandonFinally => "abandon_finally".to_string(),
        Instruction::Throw => "throw".to_string(),
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).expect("value should serialize")
}

fn slot_name(chunk: &Chunk, index: usize) -> &str {
    chunk.slot_names[index].text.as_ref()
}

fn name_text(chunk: &Chunk, index: usize) -> &str {
    chunk.names[index].text.as_ref()
}

fn name(chunk: &Chunk, index: usize) -> &str {
    name_text(chunk, index)
}

fn keys_snapshot(chunk: &Chunk, index: usize) -> String {
    let keys = &chunk.key_lists[index];
    let names = keys
        .iter()
        .map(|key| name_text(chunk, *key))
        .collect::<Vec<_>>();
    format!("[{}]", names.join(", "))
}

fn assign_path_snapshot(chunk: &Chunk, index: usize) -> String {
    let path = &chunk.assign_paths[index];
    let mut rendered = String::new();
    for step in path.steps.iter() {
        match step {
            CompiledAssignPathStep::Field(field) => {
                write!(rendered, ".{}", name_text(chunk, *field)).unwrap();
            }
            CompiledAssignPathStep::Index => rendered.push_str("[dynamic]"),
        }
    }
    rendered
}

fn format_template_snapshot(chunk: &Chunk, index: usize) -> String {
    let template = &chunk.format_templates[index];
    let parts = template
        .parts
        .iter()
        .map(|part| match part {
            CompiledFormatPart::Literal(value) => format!("{value:?}"),
            CompiledFormatPart::Arg(index) => format!("arg{index}"),
        })
        .collect::<Vec<_>>()
        .join(" + ");
    format!(
        "argc={} min={} parts={parts}",
        template.argc, template.min_capacity
    )
}

fn intrinsic_snapshot(chunk: &Chunk, op: IntrinsicOp) -> String {
    let argc = op.fixed_argc().unwrap_or(0);
    match op {
        IntrinsicOp::Len => format!("intrinsic len argc={argc}"),
        IntrinsicOp::Empty => format!("intrinsic empty argc={argc}"),
        IntrinsicOp::Keys => format!("intrinsic keys argc={argc}"),
        IntrinsicOp::Values => format!("intrinsic values argc={argc}"),
        IntrinsicOp::Contains => format!("intrinsic contains argc={argc}"),
        IntrinsicOp::Find(_) => format!("intrinsic find argc={argc}"),
        IntrinsicOp::GrepText => format!("intrinsic grep_text argc={argc}"),
        IntrinsicOp::StartsWith => format!("intrinsic starts_with argc={argc}"),
        IntrinsicOp::EndsWith => format!("intrinsic ends_with argc={argc}"),
        IntrinsicOp::Split => format!("intrinsic split argc={argc}"),
        IntrinsicOp::Join => format!("intrinsic join argc={argc}"),
        IntrinsicOp::Trim => format!("intrinsic trim argc={argc}"),
        IntrinsicOp::Slice => format!("intrinsic slice argc={argc}"),
        IntrinsicOp::ToString => format!("intrinsic to_string argc={argc}"),
        IntrinsicOp::ToInt => format!("intrinsic to_int argc={argc}"),
        IntrinsicOp::ToFloat => format!("intrinsic to_float argc={argc}"),
        IntrinsicOp::JsonParse => format!("intrinsic json_parse argc={argc}"),
        IntrinsicOp::Format(_) => format!("intrinsic format argc={argc}"),
        IntrinsicOp::Validate => format!("intrinsic validate argc={argc}"),
        IntrinsicOp::Range(_) => format!("intrinsic range argc={argc}"),
        IntrinsicOp::CeilDiv => format!("intrinsic ceil_div argc={argc}"),
        IntrinsicOp::FloorDiv => format!("intrinsic floor_div argc={argc}"),
        IntrinsicOp::Push => format!("intrinsic push argc={argc}"),
        IntrinsicOp::Sort => format!("intrinsic sort argc={argc}"),
        IntrinsicOp::SortBy => format!("intrinsic sort_by argc={argc}"),
        IntrinsicOp::Sum => format!("intrinsic sum argc={argc}"),
        IntrinsicOp::Min => format!("intrinsic min argc={argc}"),
        IntrinsicOp::Max => format!("intrinsic max argc={argc}"),
        IntrinsicOp::Replace => format!("intrinsic replace argc={argc}"),
        IntrinsicOp::Lower => format!("intrinsic lower argc={argc}"),
        IntrinsicOp::Upper => format!("intrinsic upper argc={argc}"),
        IntrinsicOp::Unique => format!("intrinsic unique argc={argc}"),
        IntrinsicOp::Reverse => format!("intrinsic reverse argc={argc}"),
        IntrinsicOp::InvalidArity { name, .. } => {
            format!(
                "intrinsic invalid_arity({}) argc={argc}",
                name_text(chunk, name)
            )
        }
        IntrinsicOp::Unknown { name, .. } => {
            format!("intrinsic unknown({}) argc={argc}", name_text(chunk, name))
        }
        IntrinsicOp::ValidateCompiled(schema) => {
            format!("intrinsic validate_compiled schema#{schema}")
        }
        IntrinsicOp::PushAssign(slot) => {
            format!("intrinsic push_assign {slot}:{}", slot_name(chunk, slot))
        }
        IntrinsicOp::FormatCompiled(template) => {
            format!(
                "intrinsic format_compiled {}",
                format_template_snapshot(chunk, template)
            )
        }
        IntrinsicOp::FormatCompiledSlotNumber { template, slot } => format!(
            "intrinsic format_compiled_slot_number {} {slot}:{}",
            format_template_snapshot(chunk, template),
            slot_name(chunk, slot)
        ),
        IntrinsicOp::FormatCompiledSlotNumberBinary {
            template,
            slot,
            op,
            right,
        } => format!(
            "intrinsic format_compiled_slot_number_binary {} {slot}:{} {op:?} {right}",
            format_template_snapshot(chunk, template),
            slot_name(chunk, slot)
        ),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn value_helpers_and_display_cover_all_variants() {
    let mut record = Record::default();
    record.insert("k".to_string(), Value::Number(1.0));

    assert_eq!(Value::Null.to_string(), "null");
    assert_eq!(Value::Bool(true).to_string(), "true");
    assert_eq!(Value::Number(1.5).to_string(), "1.5");
    assert_eq!(Value::String("x".to_string().into()).to_string(), "x");
    assert_eq!(
        Value::List(vec![Value::Bool(true)].into()).to_string(),
        "[true]"
    );
    assert_eq!(
        Value::Record(record.clone().into()).as_record().unwrap()["k"],
        Value::Number(1.0)
    );
    assert!(Value::String("x".to_string().into()).as_record().is_none());
    assert!(Value::Record(record.into()).to_string().contains("\"k\":1"));
}

#[tokio::test(flavor = "current_thread")]
async fn compiler_folds_constant_list_and_record_literals() {
    let program = crate::parse(
        r#"
        items = [{ label: "a", weight: 1 }, { label: "b", weight: 2 }]
        finish items
        "#,
    )
    .expect("program should parse");
    let compiled = compile_program(&program);

    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::PushConst(_)))
    );
    assert!(
        !compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::BuildRecord(_)))
    );

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &Host)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(Value::List(items)) = outcome else {
        panic!("expected folded list result");
    };
    assert_eq!(items.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn compiler_propagates_safe_straight_line_constants() {
    let program = crate::parse(
        r#"
        items = [1, 2, 3]
        indexes = range(0, len(items))
        extended = push(indexes, len(items))
        finish extended
        "#,
    )
    .expect("program should parse");
    let compiled = compile_program(&program);

    assert!(
        !compiled.chunk.code.iter().any(|instruction| matches!(
            instruction,
            Instruction::Intrinsic(IntrinsicOp::Len | IntrinsicOp::Range(_) | IntrinsicOp::Push)
        )),
        "straight-line constant builtins should fold out of the runtime instruction stream"
    );

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &Host)
        .await
        .expect("program should run");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::List(
            vec![
                Value::Number(0.0),
                Value::Number(1.0),
                Value::Number(2.0),
                Value::Number(3.0),
            ]
            .into()
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn compiler_keeps_assignment_hot_paths_specialized() {
    let source = r#"
        items = []
        total = 0
        step = await tools.echo({ value: 3 })?
        items = push(items, 1)
        total = total + 2
        total = total + step
        finish { items: items, total: total }
        "#;
    let compiled = compile_source(source).expect("program should compile");

    assert!(
        compiled.chunk.code.iter().any(|instruction| {
            matches!(
                instruction,
                Instruction::Intrinsic(IntrinsicOp::PushAssign(_))
            )
        }),
        "`x = push(x, item)` should compile to the in-place push-assign opcode"
    );
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::AddAssignNumber { .. })),
        "`x = x + constant_number` should compile to numeric add-assign"
    );
    assert!(
        compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::AddAssignSlot { .. })),
        "`x = x + y` should compile to slot add-assign"
    );
    assert!(
        !compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::Intrinsic(IntrinsicOp::Push))),
        "the assignment form should not route through generic push"
    );
    assert!(
        !compiled
            .chunk
            .code
            .iter()
            .any(|instruction| matches!(instruction, Instruction::AddAssign(_))),
        "numeric assignment forms should not route through generic add-assign"
    );

    let mut state = State::new();
    let outcome = execute_compiled(&compiled, &mut state, &Host)
        .await
        .expect("program should run");
    let ExecutionOutcome::Finished(Value::Record(record)) = outcome else {
        panic!("expected record result");
    };
    assert_eq!(record["total"], Value::Number(5.0));
    assert_eq!(
        record["items"],
        Value::List(vec![Value::Number(1.0)].into())
    );
}

include!("tests/compiler_cases.rs");
include!("tests/projection_cases.rs");
include!("tests/async_and_cache_cases.rs");
include!("tests/continuation_cases.rs");
include!("tests/continuation_wire_cases.rs");
include!("tests/function_cases.rs");
include!("tests/exception_cases.rs");
include!("tests/exception_control_flow_cases.rs");
