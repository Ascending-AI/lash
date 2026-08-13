use super::*;
use crate::ast::{AssignTarget, Expr, FunctionExpr, Program};
use crate::runtime::entry_points::compile_program_internal;
use crate::{AbilityOp, AbilityResult, ExecutionHostError};

struct TestHost;

impl ExecutionHost for TestHost {
    async fn perform(&self, _op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        Err(ExecutionHostError::new("test host performs no effects"))
    }
}

fn one_capture_program() -> CompiledProgram {
    compile_program_internal(&Program::block(vec![
        Expr::Assign {
            target: AssignTarget::variable("captured".into()),
            expr: Box::new(Expr::Number(1.0)),
        },
        Expr::Assign {
            target: AssignTarget::variable("f".into()),
            expr: Box::new(Expr::Function(Box::new(FunctionExpr {
                name: None,
                params: Vec::new(),
                captures: vec!["captured".into()],
                body: Box::new(Expr::Variable("captured".into())),
            }))),
        },
        Expr::Finish(Box::new(Expr::Call {
            function: Box::new(Expr::Variable("f".into())),
            args: Vec::new(),
        })),
    ]))
}

fn root_continuation(program: &CompiledProgram, heap: Heap, root: Option<Value>) -> VmContinuation {
    let mut continuation = empty_continuation(heap);
    continuation.slots = vec![None; program.chunk.slot_names.len()];
    continuation.projected_slots = vec![false; program.chunk.slot_names.len()];
    if let Some(root) = root {
        continuation.slots[0] = Some(root);
    }
    continuation
}

fn expect_capture_count_error(program: &CompiledProgram, continuation: VmContinuation) {
    assert!(matches!(
        Vm::resume_from(continuation, program, &TestHost),
        Err(ContinuationError::ClosureCaptureCountMismatch {
            index: 0,
            expected: 1,
            ..
        })
    ));
}

#[test]
fn resume_validates_closures_in_active_frames_globals_and_nested_containers() {
    let program = one_capture_program();

    for captures in [Vec::new(), vec![Value::Null, Value::Bool(true)]] {
        let mut heap = Heap::default();
        let closure = heap
            .allocate(HeapObject::Closure {
                function: 0,
                captures,
            })
            .expect("allocate malformed closure");
        expect_capture_count_error(&program, root_continuation(&program, heap, Some(closure)));
    }

    let mut global_heap = Heap::default();
    let global_closure = global_heap
        .allocate(HeapObject::Closure {
            function: 0,
            captures: Vec::new(),
        })
        .expect("allocate global closure");
    let mut global = root_continuation(&program, global_heap, None);
    global.globals.insert("f".to_string(), global_closure);
    expect_capture_count_error(&program, global);

    let mut nested_heap = Heap::default();
    let nested_closure = nested_heap
        .allocate(HeapObject::Closure {
            function: 0,
            captures: Vec::new(),
        })
        .expect("allocate nested closure");
    let nested_record = nested_heap
        .allocate(HeapObject::Record(Box::new({
            let mut record = Record::new();
            record.insert("closure".to_string(), nested_closure);
            record
        })))
        .expect("allocate nested record");
    let nested_list = nested_heap
        .allocate(HeapObject::List(vec![nested_record]))
        .expect("allocate nested list");
    expect_capture_count_error(
        &program,
        root_continuation(&program, nested_heap, Some(nested_list)),
    );

    let mut frame_heap = Heap::default();
    let frame_closure = frame_heap
        .allocate(HeapObject::Closure {
            function: 0,
            captures: Vec::new(),
        })
        .expect("allocate frame closure");
    let function = &program.chunk.functions[0];
    let call_ip = program
        .chunk
        .code
        .iter()
        .take(program.chunk.root_code_len)
        .position(|instruction| matches!(instruction, Instruction::Call { .. }))
        .expect("root call instruction");
    let mut frame = root_continuation(&program, frame_heap, None);
    frame.active_function = Some(0);
    frame.instruction_pointer = function.entry_ip;
    frame.slots = vec![None; function.slot_names.len()];
    frame.projected_slots = vec![false; function.slot_names.len()];
    let mut caller_slots = vec![None; program.chunk.slot_names.len()];
    caller_slots[0] = Some(frame_closure);
    frame.frame_stack.push(VmFrameContinuation {
        return_instruction_pointer: call_ip + 1,
        function: None,
        operand_stack_base: 0,
        slots: caller_slots,
        projected_slots: vec![false; program.chunk.slot_names.len()],
        globals: Record::new(),
        iterator_stack: Vec::new(),
        return_target: VmFrameReturnContinuation::Direct,
    });
    expect_capture_count_error(&program, frame);
}

#[test]
fn resume_reports_unknown_closure_function_indices_by_name() {
    let program = one_capture_program();
    let mut heap = Heap::default();
    let closure = heap
        .allocate(HeapObject::Closure {
            function: 99,
            captures: Vec::new(),
        })
        .expect("allocate unknown closure");
    assert!(matches!(
        Vm::resume_from(
            root_continuation(&program, heap, Some(closure)),
            &program,
            &TestHost,
        ),
        Err(ContinuationError::UnknownFunction { index: 99 })
    ));
}
