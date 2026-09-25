//! Call receivers (FIG-3700): a non-arrow function binds the receiver of a
//! member call in its receiver slot, and that slot is an ordinary frame slot,
//! so a continuation captured inside the call carries it like any other.

use super::continuation_cases::{
    find_instruction_continuation, round_trip_and_resume, uninterrupted_continuation_result,
};
use super::*;
use crate::ast::MethodKey;

/// `function (x) { i = 0; total = 0; while i < 5 { total = total + this.k;
/// i = i + 1 } return total + x }`, reading its receiver as `self`.
fn summing_method() -> Expr {
    Expr::Function(Box::new(FunctionExpr {
        name: None,
        js_name: None,
        receiver: Some("self".into()),
        params: vec!["x".into()],
        captures: Vec::new(),
        body: Box::new(builders::block(vec![
            builders::assign("i", builders::num(0.0)),
            builders::assign("total", builders::num(0.0)),
            builders::while_loop(
                builders::binary(builders::var("i"), BinaryOp::Less, builders::num(5.0)),
                builders::block(vec![
                    builders::assign(
                        "total",
                        builders::binary(
                            builders::var("total"),
                            BinaryOp::Add,
                            builders::field(builders::var("self"), "k"),
                        ),
                    ),
                    builders::assign(
                        "i",
                        builders::binary(builders::var("i"), BinaryOp::Add, builders::num(1.0)),
                    ),
                ]),
            ),
            Expr::Return(Box::new(builders::binary(
                builders::var("total"),
                BinaryOp::Add,
                builders::var("x"),
            ))),
        ])),
    }))
}

fn method_call_program() -> CompiledProgram {
    compile_program_for_tests(builders::program(vec![
        builders::assign(
            "o",
            builders::record(vec![("k", builders::num(3.0)), ("f", summing_method())]),
        ),
        builders::finish(Expr::MethodCall {
            receiver: Box::new(builders::var("o")),
            method: MethodKey::Field("f".into()),
            args: vec![builders::num(1.0)],
        }),
    ]))
}

/// The continuation's active frame is the method's: a receiver reference in a
/// slot, and the loop past its first iterations.
fn inside_the_method(continuation: &VmContinuation) -> bool {
    !continuation.frame_stack.is_empty()
        && continuation
            .slots
            .iter()
            .any(|slot| matches!(slot, Some(Value::Number(total)) if *total == 6.0))
        && continuation
            .slots
            .iter()
            .any(|slot| matches!(slot, Some(Value::Ref(_))))
}

#[tokio::test(flavor = "current_thread")]
async fn a_member_call_binds_the_receiver_resident_and_restored() {
    let program = method_call_program();
    let expected = uninterrupted_continuation_result(&program).await;
    assert_eq!(expected, ExecutionOutcome::Finished(Value::Number(16.0)));
    let continuation = find_instruction_continuation(&program, inside_the_method).await;
    assert_eq!(
        round_trip_and_resume(&program, continuation).await,
        expected
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_plain_call_binds_an_undefined_receiver() {
    let program = compile_program_for_tests(builders::program(vec![
        builders::assign(
            "f",
            Expr::Function(Box::new(FunctionExpr {
                name: None,
                js_name: None,
                receiver: Some("self".into()),
                params: Vec::new(),
                captures: Vec::new(),
                body: Box::new(Expr::Return(Box::new(builders::var("self")))),
            })),
        ),
        builders::finish(builders::call(builders::var("f"), Vec::new())),
    ]));
    assert_eq!(
        uninterrupted_continuation_result(&program).await,
        ExecutionOutcome::Finished(Value::Undefined)
    );
}

/// `[1, 2].map((x) => this_call(t, g, [x]))`: a callback frame whose callee
/// is called with an explicit receiver, suspended inside the receiver's
/// method and resumed.
#[tokio::test(flavor = "current_thread")]
async fn an_explicit_receiver_survives_a_callback_frame_round_trip() {
    let program = compile_program_for_tests(builders::program(vec![
        builders::assign("t", builders::record(vec![("k", builders::num(2.0))])),
        builders::assign("g", summing_method()),
        builders::finish(Expr::Map {
            items: Box::new(builders::list(vec![builders::num(1.0), builders::num(2.0)])),
            function: Box::new(Expr::Function(Box::new(FunctionExpr {
                name: None,
                js_name: None,
                receiver: None,
                params: vec!["x".into()],
                captures: vec!["t".into(), "g".into()],
                body: Box::new(Expr::ThisCall {
                    this: Box::new(builders::var("t")),
                    function: Box::new(builders::var("g")),
                    args: vec![builders::var("x")],
                }),
            }))),
        }),
    ]));
    let expected = uninterrupted_continuation_result(&program).await;
    assert_eq!(
        expected,
        ExecutionOutcome::Finished(Value::List(
            vec![Value::Number(11.0), Value::Number(12.0)].into()
        ))
    );
    let continuation = find_instruction_continuation(&program, |continuation| {
        continuation.frame_stack.len() == 2
            && continuation
                .slots
                .iter()
                .any(|slot| matches!(slot, Some(Value::Number(total)) if *total == 4.0))
    })
    .await;
    assert_eq!(
        round_trip_and_resume(&program, continuation).await,
        expected
    );
}

/// A continuation from the generation before receivers existed is refused
/// before it runs: its frames were laid out without receiver slots.
#[tokio::test(flavor = "current_thread")]
async fn a_pre_receiver_continuation_is_refused() {
    let program = method_call_program();
    let mut continuation = find_instruction_continuation(&program, inside_the_method).await;
    continuation.format_version = VM_CONTINUATION_FORMAT_VERSION - 1;
    let host = Host;
    let error = Vm::resume_from(continuation, &program, &host)
        .err()
        .expect("an older continuation generation must be refused");
    assert!(
        matches!(error, ContinuationError::FormatVersionMismatch { .. }),
        "{error:?}"
    );
}
