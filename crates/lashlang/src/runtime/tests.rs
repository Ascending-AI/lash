use super::vm::{VM_CONTINUATION_FORMAT_VERSION, VmFrameContinuation, VmFrameReturnContinuation};
use super::*;
use crate::ast::{
    AssignTarget, BinaryOp, Declaration, Expr, FunctionDecl, FunctionExpr, FunctionParam, Program,
    TypeExpr,
};
use crate::runtime::entry_points::compile_program_internal;
use crate::testing::ast_builders as builders;
/// The shared test host, named `Host` here because this crate's unit tests have
/// referred to it that way since before it was published.
use crate::testing::harness::EchoHost as Host;
use crate::testing::harness::{
    compile_labeled_process_program, compile_labeled_program, execute_compiled,
    execute_compiled_traced, execute_compiled_with_projected_bindings,
    test_environment as runtime_test_environment,
};
use lash_sansio::sync::MutexExt;
use std::fmt::Write as _;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

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
    events: Mutex<Vec<ProcessEvent>>,
    sleeps: Mutex<Vec<Sleep>>,
}

impl ExecutionHost for RecordingProcessHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(_) | AbilityOp::ResourceOperationBatch(_) => Err(
                ExecutionHostError::new("module operations are not supported by this host"),
            ),
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
            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityResult::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

/// `while i < <limit> { i = i + 1 }`
fn counting_loop(limit: f64) -> Expr {
    builders::while_loop(
        builders::binary(builders::var("i"), BinaryOp::Less, builders::num(limit)),
        builders::block(vec![builders::assign(
            "i",
            builders::binary(builders::var("i"), BinaryOp::Add, builders::num(1.0)),
        )]),
    )
}

/// `i = 0` / `while i < 5000 { i = i + 1 }` / `finish i`
fn long_counting_loop_program() -> Program {
    builders::program(vec![
        builders::assign("i", builders::num(0.0)),
        counting_loop(5000.0),
        builders::finish(builders::var("i")),
    ])
}

async fn exec(program: Program) -> Result<Value, RuntimeError> {
    let mut state = State::new();
    match execute_program(&program, &mut state, &Host).await? {
        ExecutionOutcome::Finished(value) => Ok(value),
        ExecutionOutcome::Continued => panic!("expected `finish` in test program"),
        ExecutionOutcome::Failed(value) => panic!("unexpected process failure: {value}"),
    }
}

async fn exec_outcome(program: Program) -> Result<ExecutionOutcome, RuntimeError> {
    let mut state = State::new();
    execute_program(&program, &mut state, &Host).await
}

#[tokio::test(flavor = "current_thread")]
async fn instruction_budget_exhaustion_is_typed_and_caret_rendered() {
    // `i = 0` / `while i < 5000 { i = i + 1 }` / `finish i`; the span covers
    // the whole loop statement, which is where the budget runs out.
    let source = "i = 0\nwhile i < 5000 { i = i + 1 }\nfinish i";
    let program =
        builders::with_expression_spans(long_counting_loop_program(), &[(0, 5), (6, 34), (35, 43)]);
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
    let program = builders::program(vec![builders::assign("value", builders::num(1.0))]);
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
    // `values = unique(range(0, 4000))` / `finish values.missing`
    let program = builders::program(vec![
        builders::assign(
            "values",
            builders::builtin(
                "unique",
                vec![builders::builtin(
                    "range",
                    vec![builders::num(0.0), builders::num(4000.0)],
                )],
            ),
        ),
        builders::finish(builders::field(builders::var("values"), "missing")),
    ]);
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
    // `finish unique(range(0, 4000))`
    let program = builders::program(vec![builders::finish(builders::builtin(
        "unique",
        vec![builders::builtin(
            "range",
            vec![builders::num(0.0), builders::num(4000.0)],
        )],
    ))]);
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
    // `value = tools.echo({ value: 1 })` / a hundred-step loop / `finish value`
    let program = builders::program(vec![
        builders::assign(
            "value",
            builders::receiver_call(
                builders::resource(&["tools"]),
                "echo",
                vec![builders::record(vec![("value", builders::num(1.0))])],
            ),
        ),
        builders::assign("i", builders::num(0.0)),
        counting_loop(100.0),
        builders::finish(builders::var("value")),
    ]);
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
    let program = long_counting_loop_program();
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
    // `finish 1 in "123"`
    let compiled = compile_program(&builders::program(vec![builders::finish(
        builders::binary(builders::num(1.0), BinaryOp::In, builders::string("123")),
    )]));
    let mut state = State::new();
    let error = execute(&compiled, &mut state, &Host)
        .await
        .expect_err("numeric string needles must not be coerced");
    assert_eq!(error, RuntimeError::InUnsupported);
}

/// Compiles a built program, linking it first when it names host modules.
///
/// The retired string helper decided this by looking for `tools.` in the source
/// text; a built program is asked directly whether it carries a resource
/// reference, which is the fact that mattered.
fn compile_program_for_tests(program: Program) -> CompiledProgram {
    if program_references_a_resource(&program.main)
        && let Ok(linked) = crate::LinkedModule::link(program.clone(), runtime_test_environment())
    {
        crate::compile_linked(&linked)
    } else {
        compile_program(&program)
    }
}

fn program_references_a_resource(expr: &Expr) -> bool {
    struct Finder(bool);

    impl crate::ExprVisitor for Finder {
        fn visit_expr(&mut self, expr: &Expr) {
            if matches!(expr, Expr::ResourceRef(_)) {
                self.0 = true;
                return;
            }
            crate::walk_expr(self, expr);
        }
    }

    let mut finder = Finder(false);
    crate::ExprVisitor::visit_expr(&mut finder, expr);
    finder.0
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

/// The golden contract program, built from the AST.
///
/// It was `join`/`find`/`grep_text`/`split` over a projected `history`, a
/// counting loop, a `Type { .. }` literal and a `validate` call — the shape the
/// lashlang host environment has to compile. None of those builtins has a
/// TypeScript spelling, so the program is stated here rather than parsed; the
/// bytecode snapshot below is unchanged, which is what proves the translation.
fn golden_contract_program() -> Program {
    builders::program(vec![
        builders::assign(
            "source",
            builders::builtin(
                "join",
                vec![builders::var("history"), builders::string(",")],
            ),
        ),
        builders::assign(
            "beta_index",
            builders::builtin(
                "find",
                vec![builders::var("source"), builders::string("beta")],
            ),
        ),
        builders::assign(
            "matches",
            builders::builtin(
                "grep_text",
                vec![builders::var("source"), builders::string("beta")],
            ),
        ),
        builders::assign("counts", builders::record(vec![])),
        builders::for_in(
            "token",
            builders::builtin(
                "split",
                vec![builders::var("source"), builders::string(",")],
            ),
            builders::block(vec![builders::assign_path(
                "counts",
                vec![builders::index_step(builders::var("token"))],
                builders::binary(
                    builders::index(builders::var("counts"), builders::var("token")),
                    BinaryOp::Add,
                    builders::num(1.0),
                ),
            )]),
        ),
        builders::assign(
            "Payload",
            builders::type_literal(TypeExpr::Object(vec![
                builders::type_field(
                    "beta_index",
                    TypeExpr::union(vec![TypeExpr::Int, TypeExpr::Null]),
                    false,
                ),
                builders::type_field("matches", TypeExpr::List(Box::new(TypeExpr::Dict)), false),
                builders::type_field("counts", TypeExpr::Dict, false),
            ])),
        ),
        builders::finish(builders::builtin(
            "validate",
            vec![
                builders::record(vec![
                    ("beta_index", builders::var("beta_index")),
                    ("matches", builders::var("matches")),
                    ("counts", builders::var("counts")),
                ]),
                builders::var("Payload"),
            ],
        )),
    ])
}

#[test]
fn golden_compiled_bytecode_contract_covers_lashlang_host_environment() {
    insta::assert_snapshot!(
        "lashlang_compiled_bytecode_contract",
        compiled_program_snapshot(golden_contract_program())
    );
}

// The diagnostic renderer's location block and caret run are pinned against an
// explicit span table (`builders::with_source_spans`) rather than a parsed one.
// FIG-3065: the TypeScript lowerer emits a `Program` with empty span vectors,
// so no front-end supplies these offsets any more; stating them keeps the
// renderer's exact output pinned and makes this the test that proves FIG-3065
// when it is fixed. The reachable, location-free half of the old corpus moved
// to `tests/diagnostic_rendering.rs`, which lowers real TypeScript.
#[tokio::test(flavor = "current_thread")]
async fn golden_runtime_diagnostic_contract_is_exact() {
    // `x = 1` / `finish len(true)`; the span covers the whole second statement.
    let source = "x = 1\nfinish len(true)";
    let program = builders::with_expression_spans(
        builders::program(vec![
            builders::assign("x", builders::num(1.0)),
            builders::finish(builders::builtin("len", vec![builders::bool_lit(true)])),
        ]),
        &[(0, 5), (6, 22)],
    );
    insta::assert_snapshot!(
        "lashlang_runtime_diagnostic_contract",
        runtime_diagnostic(program, source).await
    );
}

#[tokio::test(flavor = "current_thread")]
async fn labeled_aggregate_await_failure_points_at_failing_leaf_not_label() {
    let source = r#"@label(title: "Aggregate")
result = await {
  ok: tools.echo({ value: "ok" })?,
  bad: tools.err({})?
}
finish result"#;
    // The span is the failing leaf `tools.err({})?`, not the labelled
    // statement: that is the whole point of the assertion below.
    let program = builders::with_source_spans(
        builders::program(vec![
            builders::labelled(
                builders::label("Aggregate", None),
                builders::assign(
                    "result",
                    builders::await_expr(builders::record(vec![
                        (
                            "ok",
                            builders::unwrap(builders::receiver_call(
                                builders::resource(&["tools"]),
                                "echo",
                                vec![builders::record(vec![("value", builders::string("ok"))])],
                            )),
                        ),
                        (
                            "bad",
                            builders::unwrap(builders::receiver_call(
                                builders::resource(&["tools"]),
                                "err",
                                vec![builders::record(vec![])],
                            )),
                        ),
                    ])),
                ),
            ),
            builders::finish(builders::var("result")),
        ]),
        &[(&[0, 0, 0, 0, 1], 87, 101)],
    );
    let compiled = compile_labeled_program(program);
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

/// Renders the link refusal for `program` against `source`.
///
/// FIG-3065: with no span table the renderer emits the message and hint and no
/// location block, which is what the callers below assert on.
fn link_diagnostic(program: Program, source: &str) -> String {
    let error = crate::LinkedModule::link(program, runtime_test_environment())
        .expect_err("diagnostic program should fail to link");
    crate::format_link_diagnostic(source, &error)
}

async fn runtime_diagnostic(program: Program, source: &str) -> String {
    let compiled = compile_program_for_tests(program);
    let mut state = State::new();
    let failure = execute_compiled_traced(&compiled, &mut state, &Host)
        .await
        .expect_err("runtime should fail");
    crate::format_runtime_diagnostic(source, &failure.error, failure.span)
}

fn compiled_program_snapshot(program: Program) -> String {
    let compiled = compile_program_for_tests(program);
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
        Instruction::PendingTool { operation, argc } => format!("pending_tool {operation} {argc}"),
        Instruction::AwaitArray { settle } => format!("await_array {settle}"),
        Instruction::AwaitPending => "await_pending".to_string(),
        Instruction::PushNull => "push_null".to_string(),
        Instruction::PushUndefined => "push_undefined".to_string(),
        Instruction::PushBool(value) => format!("push_bool {value}"),
        Instruction::PushNumber(value) => format!("push_number {value}"),
        Instruction::LoadName(slot) => format!("load_name {slot}:{}", slot_name(chunk, slot)),
        Instruction::Duplicate => "duplicate".to_string(),
        Instruction::StoreName(slot) => format!("store_name {slot}:{}", slot_name(chunk, slot)),
        Instruction::BuildTuple(count) => format!("build_tuple {count}"),
        Instruction::BuildList(count) => format!("build_list {count}"),
        Instruction::BuildHeapList(count) => format!("build_heap_list {count}"),
        Instruction::ListAppend => "list_append".to_string(),
        Instruction::BuildRecord(keys) => format!("build_record {}", keys_snapshot(chunk, keys)),
        Instruction::BuildHeapRecord(keys) => {
            format!("build_heap_record {}", keys_snapshot(chunk, keys))
        }
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
        Instruction::HeapPathAssign { slot, path } => format!(
            "heap_path_assign {slot}:{} {}",
            slot_name(chunk, slot),
            assign_path_snapshot(chunk, path)
        ),
        Instruction::ResultUnwrap => "result_unwrap".to_string(),
        Instruction::Unary(op) => format!("unary {op:?}"),
        Instruction::Binary(op) => format!("binary {op:?}"),
        Instruction::JavaScriptUnary(op) => format!("javascript_unary {op:?}"),
        Instruction::JavaScriptBinary(op) => format!("javascript_binary {op:?}"),
        Instruction::IsNullish => "is_nullish".to_string(),
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
        Instruction::ResourceOperationListBatch(batch) => {
            let batch = &chunk.resource_operation_list_batches[batch];
            format!(
                "resource_operation_list_batch {} argc={} unwrap={} aggregate_unwrap={}",
                name_text(chunk, batch.operation),
                batch.argc,
                batch.unwrap,
                batch.aggregate_unwrap
            )
        }
        Instruction::AwaitHandle => "await_handle".to_string(),
        Instruction::SleepFor => "sleep_for".to_string(),
        Instruction::SleepUntil => "sleep_until".to_string(),
        Instruction::ProcessWaitSignal { name } => {
            format!("process_wait_signal {}", name_text(chunk, name))
        }
        Instruction::AwaitHandleUnwrap => "await_handle_unwrap".to_string(),
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
        Instruction::CallDynamic => "call_dynamic".to_string(),
        Instruction::Map => "map_callback".to_string(),
        Instruction::AsyncMap => "async_map_callback".to_string(),
        Instruction::Return => "return".to_string(),
        Instruction::PushHandler { .. } => "push_handler".to_string(),
        Instruction::PopHandler => "pop_handler".to_string(),
        Instruction::EnterFinally { .. } => "enter_finally".to_string(),
        Instruction::EndFinally => "end_finally".to_string(),
        Instruction::AbandonFinally => "abandon_finally".to_string(),
        Instruction::AbandonFinallyKeepValue => "abandon_finally_keep_value".to_string(),
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
        IntrinsicOp::JavaScriptSplit => format!("intrinsic typescript_split argc={argc}"),
        IntrinsicOp::JavaScriptJoin => format!("intrinsic typescript_join argc={argc}"),
        IntrinsicOp::JavaScriptStdlib(_) => {
            format!("intrinsic typescript_stdlib argc={argc}")
        }
        IntrinsicOp::JavaScriptHeapNew(_) => {
            format!("intrinsic typescript_heap_new argc={argc}")
        }
        IntrinsicOp::JavaScriptHeapInstanceOf => {
            format!("intrinsic typescript_heap_instanceof argc={argc}")
        }
        IntrinsicOp::JavaScriptHeapDeleteMember => {
            format!("intrinsic typescript_heap_delete_member argc={argc}")
        }
        IntrinsicOp::JavaScriptRegExp(_) => {
            format!("intrinsic typescript_regexp argc={argc}")
        }
        IntrinsicOp::JavaScriptGlobalDelete => {
            format!("intrinsic typescript_global_delete argc={argc}")
        }
        IntrinsicOp::JavaScriptGlobalHas => {
            format!("intrinsic typescript_global_has argc={argc}")
        }
        IntrinsicOp::JavaScriptGlobalSet => {
            format!("intrinsic typescript_global_set argc={argc}")
        }
        IntrinsicOp::JavaScriptUriCodec(_) => {
            format!("intrinsic typescript_uri_codec argc={argc}")
        }
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
    // `items = [{ label: "a", weight: 1 }, { label: "b", weight: 2 }]`
    // `finish items`
    let row = |label: &str, weight: f64| {
        builders::record(vec![
            ("label", builders::string(label)),
            ("weight", builders::num(weight)),
        ])
    };
    let program = builders::program(vec![
        builders::assign("items", builders::list(vec![row("a", 1.0), row("b", 2.0)])),
        builders::finish(builders::var("items")),
    ]);
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
    // `items = [1, 2, 3]` / `indexes = range(0, len(items))`
    // `extended = push(indexes, len(items))` / `finish extended`
    let program = builders::program(vec![
        builders::assign(
            "items",
            builders::list(vec![
                builders::num(1.0),
                builders::num(2.0),
                builders::num(3.0),
            ]),
        ),
        builders::assign(
            "indexes",
            builders::builtin(
                "range",
                vec![
                    builders::num(0.0),
                    builders::builtin("len", vec![builders::var("items")]),
                ],
            ),
        ),
        builders::assign(
            "extended",
            builders::builtin(
                "push",
                vec![
                    builders::var("indexes"),
                    builders::builtin("len", vec![builders::var("items")]),
                ],
            ),
        ),
        builders::finish(builders::var("extended")),
    ]);
    let compiled = compile_program(&program);

    // ADR 0096: the straight-line constant folder was part of Lashlang's
    // value-isolating semantics and went with the dialect, so these builtins now
    // run. What the test still pins is the result they must produce.

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
    // `items = []` / `total = 0` / `step = await tools.echo({ value: 3 })?` /
    // `items = push(items, 1)` / `total = total + 2` / `total = total + step` /
    // `finish { items: items, total: total }`
    let compiled = compile_program_for_tests(builders::program(vec![
        builders::assign("items", builders::list(vec![])),
        builders::assign("total", builders::num(0.0)),
        builders::assign(
            "step",
            builders::module_call(
                &["tools"],
                "echo",
                vec![builders::record(vec![("value", builders::num(3.0))])],
            ),
        ),
        builders::assign(
            "items",
            builders::builtin("push", vec![builders::var("items"), builders::num(1.0)]),
        ),
        builders::assign(
            "total",
            builders::binary(builders::var("total"), BinaryOp::Add, builders::num(2.0)),
        ),
        builders::assign(
            "total",
            builders::binary(builders::var("total"), BinaryOp::Add, builders::var("step")),
        ),
        builders::finish(builders::record(vec![
            ("items", builders::var("items")),
            ("total", builders::var("total")),
        ])),
    ]));

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

mod builtin_cases;
mod case_builders;
mod compiler_cases;
mod projection_cases;
use projection_cases::*;
mod async_and_cache_cases;
mod continuation_cases;
use continuation_cases::*;
mod continuation_wire_cases;
mod declared_function_cases;
mod exception_cases;
mod function_cases;
use exception_cases::*;
mod exception_control_flow_cases;
mod exception_review_cases;
mod exception_wire_cases;
mod typescript_exotic_cases;

#[path = "tests/wrapup_await_cases.rs"]
mod wrapup_await_cases;
