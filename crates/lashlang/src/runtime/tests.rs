use super::vm::{VM_CONTINUATION_FORMAT_VERSION, VmFrameContinuation, VmFrameReturnContinuation};
use super::*;
use crate::ast::{
    AssignTarget, CoercingBinaryOp, Declaration, Expr, FunctionDecl, FunctionExpr, FunctionParam,
    OperandLogicalOp, Program, TypeExpr,
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
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
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
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        if matches!(op, AbilityOp::ResourceOperation(_)) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Host.perform(op).await
    }
}

#[derive(Default)]
struct FailedSleepObservationHost {
    observations: Mutex<Vec<crate::LashlangExecutionObservation>>,
}

impl ExecutionHost for FailedSleepObservationHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Sleep(_) => Err(ExecutionHostError::new("sleep refused")),
            other => Host.perform(other).await,
        }
    }

    fn observes_lashlang_execution(&self) -> bool {
        true
    }

    fn observe_lashlang_execution(&self, observation: crate::LashlangExecutionObservation) {
        self.observations.lock_recover().push(observation);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn failed_vm_effect_without_tool_fact_is_a_runtime_failure() {
    let program = compile_labeled_process_program(
        builders::module(
            vec![builders::process(
                "main",
                Vec::new(),
                builders::block(vec![
                    builders::labelled(
                        builders::label("sleep", None),
                        builders::sleep_for(builders::num(1.0)),
                    ),
                    builders::finish(builders::null()),
                ]),
            )],
            Vec::new(),
        ),
        "main",
    );
    let host = FailedSleepObservationHost::default();
    let mut state = State::new();
    let error = execute_compiled(&program, &mut state, &host)
        .await
        .expect_err("host refuses the sleep");
    assert_eq!(error.code(), "SleepFailed");
    assert!(
        host.observations
            .lock_recover()
            .iter()
            .any(|observation| matches!(
                observation,
                crate::LashlangExecutionObservation::NodeFailed {
                    failure: crate::LashlangExecutionFailure::Runtime { code, message },
                    ..
                } if code == "SleepFailed" && message.contains("sleep refused")
            ))
    );
}

#[derive(Default)]
struct RecordingProcessHost {
    sleeps: Mutex<Vec<Sleep>>,
}

impl ExecutionHost for RecordingProcessHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::ResourceOperation(_) | AbilityOp::ResourceOperationBatch(_) => Err(
                ExecutionHostError::new("module operations are not supported by this host"),
            ),

            AbilityOp::Sleep(sleep) => {
                self.sleeps.lock_recover().push(sleep);
                Ok(AbilityOutcome::Value(Value::Null))
            }

            AbilityOp::Finish(value) | AbilityOp::Fail(value) => Ok(AbilityOutcome::Value(value)),
            _ => Err(ExecutionHostError::new("unsupported host ability")),
        }
    }
}

/// `while i < <limit> { i = i + 1 }`
fn counting_loop(limit: f64) -> Expr {
    builders::while_loop(
        builders::binary(
            builders::var("i"),
            CoercingBinaryOp::Less,
            builders::num(limit),
        ),
        builders::block(vec![builders::assign(
            "i",
            builders::binary(
                builders::var("i"),
                CoercingBinaryOp::Add,
                builders::num(1.0),
            ),
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
    ));
    let mut state = State::new();
    assert!(matches!(
        execute_program(&program, &mut state, &env).await,
        Err(RuntimeError::InstructionBudgetExceeded { limit: 10 })
    ));
}

/// FIG-3672 P2c: nothing in a run reads the wall clock, so the time a host
/// takes to answer an awaited tool changes neither the outcome nor the
/// instruction the run ends on.
#[tokio::test(flavor = "current_thread")]
async fn tool_wait_speed_changes_neither_outcome_nor_instruction_count() {
    // `value = tools.echo({ value: 1 })` / a hundred-step loop / `finish value`
    let program = compile_program_for_tests(builders::program(vec![
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
    ]));
    let mut slow_state = State::new();
    let mut slow_vm =
        Vm::from_state(&program, &mut slow_state, &SlowToolHost).expect("state should install");
    let slow_outcome = slow_vm
        .run_for_mode()
        .await
        .expect("the slow-tool run finishes");
    let mut fast_state = State::new();
    let mut fast_vm =
        Vm::from_state(&program, &mut fast_state, &Host).expect("state should install");
    let fast_outcome = fast_vm
        .run_for_mode()
        .await
        .expect("the instant-tool run finishes");
    assert_eq!(slow_outcome, fast_outcome);
    assert_eq!(
        slow_vm.instructions_executed(),
        fast_vm.instructions_executed()
    );
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
        crate::testing::harness::compile_linked_main(&linked)
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
        let compiled = crate::testing::harness::compile_linked_main(&linked);
        return super::execute(&compiled, state, host).await;
    }
    super::execute(
        &crate::compile_ast(program).expect("the program compiles"),
        state,
        host,
    )
    .await
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

fn instruction_snapshot(chunk: &Chunk, instruction: Instruction) -> String {
    match instruction {
        Instruction::PushConst(index) => {
            format!(
                "push_const c{index} {}",
                compact_json(&chunk.constants[index])
            )
        }
        Instruction::PendingTool { operation, argc } => format!("pending_tool {operation} {argc}"),
        Instruction::PendingTimer => "pending_timer".to_string(),
        Instruction::AwaitArray { consumer } => format!("await_array {consumer:?}"),
        Instruction::AwaitPending => "await_pending".to_string(),
        Instruction::PushNull => "push_null".to_string(),
        Instruction::PushUndefined => "push_undefined".to_string(),
        Instruction::PushBool(value) => format!("push_bool {value}"),
        Instruction::PushNumber(value) => format!("push_number {value}"),
        Instruction::LoadName(slot) => format!("load_name {slot}:{}", slot_name(chunk, slot)),
        Instruction::Duplicate => "duplicate".to_string(),
        Instruction::StoreName(slot) => format!("store_name {slot}:{}", slot_name(chunk, slot)),
        Instruction::BuildHeapList(count) => format!("build_heap_list {count}"),
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
        Instruction::CoercingUnary(op) => format!("coercing_unary {op:?}"),
        Instruction::CoercingBinary(op) => format!("coercing_binary {op:?}"),
        Instruction::IsNullish => "is_nullish".to_string(),
        Instruction::ToBool => "to_bool".to_string(),
        Instruction::Jump(target) => format!("jump {target}"),
        Instruction::JumpIfFalse(target) => format!("jump_if_false {target}"),
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
        Instruction::AwaitHandle => "await_handle".to_string(),
        Instruction::SleepFor => "sleep_for".to_string(),

        Instruction::AwaitHandleUnwrap => "await_handle_unwrap".to_string(),
        Instruction::Intrinsic(op) => intrinsic_snapshot(chunk, op),
        Instruction::CoercingAddAssign(slot) => {
            format!("coercing_add_assign {slot}:{}", slot_name(chunk, slot))
        }
        Instruction::Print => "print".to_string(),
        Instruction::Finish => "finish".to_string(),
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
        Instruction::WrapHostDescriptor(type_name) => {
            format!("wrap_host_descriptor {}", name_text(chunk, type_name))
        }
        Instruction::MakeClosure { function, captures } => {
            format!("make_closure {function} captures={captures}")
        }
        Instruction::Call { argc } => format!("call argc={argc}"),
        Instruction::CallDynamic => "call_dynamic".to_string(),
        Instruction::CallMethod { argc } => format!("call_method argc={argc}"),
        Instruction::CallMethodDynamic => "call_method_dynamic".to_string(),
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
        IntrinsicOp::TextSplit => format!("intrinsic text_split argc={argc}"),
        IntrinsicOp::TextJoin => format!("intrinsic text_join argc={argc}"),
        IntrinsicOp::IntrinsicDispatch(_) => {
            format!("intrinsic dispatch argc={argc}")
        }
        IntrinsicOp::HeapConstruct(_) => {
            format!("intrinsic heap_construct argc={argc}")
        }
        IntrinsicOp::HeapInstanceOf => {
            format!("intrinsic heap_instanceof argc={argc}")
        }
        IntrinsicOp::HeapDeleteMember => {
            format!("intrinsic heap_delete_member argc={argc}")
        }
        IntrinsicOp::RegExpIntrinsic(_) => {
            format!("intrinsic regexp argc={argc}")
        }
        IntrinsicOp::GlobalDelete => {
            format!("intrinsic global_delete argc={argc}")
        }
        IntrinsicOp::GlobalGet => {
            format!("intrinsic global_get argc={argc}")
        }
        IntrinsicOp::GlobalHas => {
            format!("intrinsic global_has argc={argc}")
        }
        IntrinsicOp::GlobalSet => {
            format!("intrinsic global_set argc={argc}")
        }
        IntrinsicOp::UriCodec(_) => {
            format!("intrinsic uri_codec argc={argc}")
        }
        IntrinsicOp::BindingCellNew => format!("intrinsic binding_cell_new argc={argc}"),
        IntrinsicOp::BindingCellGet => format!("intrinsic binding_cell_get argc={argc}"),
        IntrinsicOp::BindingCellSet => format!("intrinsic binding_cell_set argc={argc}"),
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
    }
}

/// F5 (FIG-3672 P9): the cancel checkpoint schedule's gaps double from 2^20
/// instructions and stop growing at 2^28, so a long run journals a bounded,
/// slowly growing number of checkpoint peeks.
#[test]
fn cancel_checkpoints_follow_the_geometric_schedule() {
    let first = CANCEL_CHECKPOINT_INSTRUCTIONS;
    assert_eq!(cancel_checkpoint_reached(0), 0);
    assert_eq!(cancel_checkpoint_reached(first - 1), 0);
    assert_eq!(cancel_checkpoint_reached(first), 1);
    assert_eq!(cancel_checkpoint_reached(3 * first - 1), 1);
    assert_eq!(cancel_checkpoint_reached(3 * first), 2);
    assert_eq!(cancel_checkpoint_reached(7 * first), 3);
    assert_eq!(cancel_checkpoint_reached(511 * first - 1), 8);
    assert_eq!(cancel_checkpoint_reached(511 * first), 9);
    let capped = 511 * first;
    assert_eq!(
        cancel_checkpoint_reached(capped + CANCEL_CHECKPOINT_INTERVAL_CAP - 1),
        9
    );
    assert_eq!(
        cancel_checkpoint_reached(capped + CANCEL_CHECKPOINT_INTERVAL_CAP),
        10
    );
    // A trillion instructions — hours of pure compute — journal a few
    // thousand checkpoints, not a million.
    let trillion = 1_000_000_000_000_u64;
    assert_eq!(
        cancel_checkpoint_reached(trillion),
        9 + (trillion - capped) / CANCEL_CHECKPOINT_INTERVAL_CAP
    );
    assert!(cancel_checkpoint_reached(trillion) < 4_000);
    for instructions in [first, 5 * first, 600 * first, trillion] {
        assert!(
            cancel_checkpoint_reached(instructions) <= cancel_checkpoint_reached(instructions + 1)
        );
    }
}

#[derive(Default)]
struct CheckpointCountingHost {
    checkpoints: Mutex<Vec<u64>>,
}

impl ExecutionHost for CheckpointCountingHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        Host.perform(op).await
    }

    async fn cancel_checkpoint(&self, checkpoint: u64) {
        self.checkpoints.lock_recover().push(checkpoint);
    }
}

/// F5 (FIG-3672 P9): a long run hands its host exactly the checkpoints the
/// schedule places, once each and in order: a loop of a few million
/// instructions reaches three, not one per 2^20 instructions.
#[tokio::test(flavor = "current_thread")]
async fn a_long_run_reaches_only_the_scheduled_checkpoints() {
    let program = compile_labeled_program(builders::program(vec![
        builders::assign("n", builders::num(0.0)),
        builders::while_loop(
            builders::binary(
                builders::var("n"),
                CoercingBinaryOp::Less,
                builders::num(1_500_000.0),
            ),
            builders::block(vec![builders::assign(
                "n",
                builders::binary(
                    builders::var("n"),
                    CoercingBinaryOp::Add,
                    builders::num(1.0),
                ),
            )]),
        ),
    ]));
    let host = CheckpointCountingHost::default();
    let mut state = State::new();
    execute_compiled(&program, &mut state, &host)
        .await
        .expect("the loop runs to completion");
    let checkpoints = host.checkpoints.lock_recover().clone();
    let reached = u64::try_from(checkpoints.len()).expect("checkpoint count fits");
    assert!(
        (2..=4).contains(&reached),
        "a loop of a few million instructions reaches a handful of checkpoints: {checkpoints:?}"
    );
    assert_eq!(checkpoints, (1..=reached).collect::<Vec<_>>());
}

mod builtin_cases;
mod builtin_function_cases;
mod case_builders;
mod compiler_cases;
mod projection_cases;
use projection_cases::*;
mod async_and_cache_cases;
mod await_park_cases;
mod continuation_cases;
mod projection_provider_laws;
use continuation_cases::*;
mod continuation_wire_cases;
mod declared_function_cases;
mod exception_cases;
mod function_cases;
mod receiver_cases;
use exception_cases::*;
mod exception_control_flow_cases;
mod exception_review_cases;
mod exception_wire_cases;
mod typescript_exotic_cases;

#[path = "tests/wrapup_await_cases.rs"]
mod wrapup_await_cases;
