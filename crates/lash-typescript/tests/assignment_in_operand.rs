//! An assignment nested inside a live operand (FIG-3075).
//!
//! The retired Lashlang parser refused assignment in expression position, and
//! ADR 0076 leaned on that refusal: with value semantics and an isolation copy
//! at every durable store, `f(x = [1], x)` would put a store between two live
//! operands and leave the operand stack borrowing an object a slot had just
//! taken ownership of.
//!
//! TypeScript is the sole dialect now (ADR 0096) and it omits the isolation
//! lowering, so the VM runs ECMA reference semantics: sharing between a slot
//! and a pending operand is ordinary, the durable boundary encodes the heap as
//! a shared graph when the forest form cannot hold it, and the assignment's
//! position in the operand order is exactly where its effect lands. These are
//! the witnesses for that, so the shape stays executable rather than resting on
//! a parser that no longer exists.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionMode, ExecutionOutcome,
    RuntimeError, State, Value, Vm, VmContinuation, VmRunOutcome,
};

/// Finishes, prints, and answers the one tool call the park witnesses make.
struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            AbilityOp::ResourceOperation(_) => Ok(AbilityResult::Value(Value::Number(7.0))),
            other => Err(ExecutionHostError::new(format!(
                "unexpected ability {other:?}"
            ))),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Process
    }
}

fn compile(source: &str) -> lashlang::CompiledProgram {
    lash_typescript::compile(source).unwrap_or_else(|error| panic!("`{source}`: {error}"))
}

fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    futures::executor::block_on(lashlang::execute(
        &compile(source),
        &mut State::new(),
        &Host,
    ))
}

fn finished(source: &str) -> Value {
    match execute(source).unwrap_or_else(|error| panic!("`{source}`: {error}")) {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

fn text(value: &str) -> Value {
    Value::String(value.into())
}

/// The ticket's shape, in both operand orders.
///
/// An operand evaluated before the assignment holds the pre-assignment value; an
/// operand evaluated after it reads the slot's new one. That is the ECMA-262
/// left-to-right order, and it is what the VM produces with the assignment
/// nested in a live list operand.
#[test]
fn an_assignment_nested_in_a_live_operand_lands_at_its_own_position() {
    assert_eq!(
        finished("let xs = [1];\nfinish(JSON.stringify([(xs = [2]), xs]));"),
        text("[[2],[2]]")
    );
    assert_eq!(
        finished("let xs = [1];\nfinish(JSON.stringify([xs, (xs = [2])]));"),
        text("[[1],[2]]")
    );
    // A call's arguments are the same operand flow: ADR 0076 named `f(x = [1], x)`
    // as the shape that would break the retired invariant.
    assert_eq!(
        finished(
            "function f(a: number[], b: number[]): string { return JSON.stringify([a, b]); }\nlet x = [1];\nfinish(f((x = [2]), x));"
        ),
        text("[[2],[2]]")
    );
}

/// The reference-semantics half: the object the assignment stored is the object
/// the slot holds, so a later mutation through either name is visible through
/// the operand that captured it.
#[test]
fn an_operand_and_the_slot_it_was_assigned_from_name_one_object() {
    assert_eq!(
        finished(
            "let xs = [1];\nconst out = [(xs = [2]), xs];\nxs.push(3);\nfinish(JSON.stringify(out));"
        ),
        text("[[2,3],[2,3]]")
    );
    // An in-place append while an operand of the same expression is live: the
    // VM mutates the heap object rather than a private copy, which is what the
    // operand holding it must see.
    assert_eq!(
        finished("let xs = [1];\nconst ys = xs;\nfinish(JSON.stringify([ys, xs.push(2), ys]));"),
        text("[[1,2],2,[1,2]]")
    );
}

/// Statement-position assignment still lowers and runs, in every form the
/// dialect spells it.
#[test]
fn statement_position_assignment_still_lowers_and_runs() {
    assert_eq!(
        finished("let n = 1;\nn = 2;\nn += 3;\nfinish(`${n}`);"),
        text("5")
    );
    assert_eq!(
        finished("const xs = [1];\nxs[0] = 9;\nfinish(JSON.stringify(xs));"),
        text("[9]")
    );
    assert_eq!(
        finished("const o = { a: 1 };\no.a = 2;\nfinish(JSON.stringify(o));"),
        text("{\"a\":2}")
    );
}

/// The durable half, which is what ADR 0076's sentence was really protecting: a
/// turn parks with the assignment's operand still pending, the continuation is
/// encoded and decoded, and the resumed turn still sees one shared object.
///
/// At the park the operand stack and the assigned slot both hold `Ref(2)`: the
/// borrowed handle outlives the store that created it and crosses the durable
/// boundary, which the validator accepts because an operand is a transient root
/// that confers no ownership.
#[tokio::test(flavor = "current_thread")]
async fn an_assignment_inside_a_pending_operand_survives_a_park_and_resume() {
    let program = compile(
        r#"
        let xs = [1];
        const out = [(xs = [2]), await tools.ping({}), xs];
        xs.push(3);
        finish(JSON.stringify(out));
        "#,
    );

    let host = Host;
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &host).expect("vm should build");
    assert!(matches!(
        vm.run_process_until_effect().await,
        Ok(VmRunOutcome::EffectCompleted)
    ));
    let continuation = vm
        .suspend()
        .expect("a turn holding an assignment's operand must be capturable");
    drop(vm);

    let bytes = serde_json::to_vec(&continuation).expect("continuation should serialize");
    let restored: VmContinuation =
        serde_json::from_slice(&bytes).expect("continuation should deserialize");

    let host = Host;
    let mut resumed =
        Vm::resume_from(restored, &program, &host).expect("continuation should resume");
    assert_eq!(
        resumed
            .run_process_until_effect()
            .await
            .expect("the resumed turn should finish"),
        VmRunOutcome::Complete(ExecutionOutcome::Finished(text("[[2,3],7,[2,3]]")))
    );
}
