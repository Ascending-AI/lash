//! Durable restore of projections nested inside containers (FIG-2865).
//!
//! Both durable writers accept `Value::Projected`, nested occurrences
//! included, through one canonical shape. A projection is plain data (ADR 0132
//! §9): a scalar projection crosses as its value and a resource projection as
//! its `ResourceRef`, so nothing is rebound on restore. What that buys a
//! program is pinned here, from the outside, through the public runtime API:
//! a cell that puts a projected session binding inside a list, or inside a
//! heap `Error`, can park at its tool effect and resume reading it. The
//! continuation wire used to refuse the value recursively, so
//! `const rows = [report]` made the turn uncapturable — while the `State`
//! snapshot persisted the identical value without complaint.

use std::collections::BTreeSet;

use lashlang::{
    AbilityOp, AbilityOutcome, ExecutionHost, ExecutionHostError, ExecutionMode, ExecutionOutcome,
    ProjectedBindings, ProjectedValue, State, Value, Vm, VmContinuation, VmRunOutcome,
};

/// Runs cells in process mode, answers the one tool call the park test makes,
/// and binds `report`.
struct RestoreHost {
    report: Option<Value>,
}

impl RestoreHost {
    fn live() -> Self {
        Self {
            report: Some(Value::String("live".into())),
        }
    }
}

impl ExecutionHost for RestoreHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityOutcome, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityOutcome::Value(value)),
            AbilityOp::ResourceOperation(_) => Ok(AbilityOutcome::Value(Value::Number(7.0))),
            other => Err(ExecutionHostError::new(format!(
                "unexpected ability {other:?}"
            ))),
        }
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Process
    }

    fn projected_bindings(&self) -> ProjectedBindings {
        let mut bindings = ProjectedBindings::new();
        if let Some(report) = &self.report {
            bindings.insert("report", ProjectedValue::scalar("report", report.clone()));
        }
        bindings
    }
}

fn compile(source: &str) -> lashlang::CompiledProgram {
    let globals = BTreeSet::from(["report".to_string(), "rows".to_string()]);
    let program = lash_typescript::parse_with_globals(source, &globals)
        .unwrap_or_else(|error| panic!("`{source}` should parse: {error}"));
    lashlang::testing::harness::try_compile_program(&program)
        .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"))
}

/// A projected binding nested in a list parks with the turn and reads the same
/// after resume.
#[tokio::test(flavor = "current_thread")]
async fn a_nested_projection_parks_and_resumes() {
    let program = compile(
        r#"
        const rows = [report];
        await tools.ping({});
        finish(rows[0] + "!");
        "#,
    );

    let host = RestoreHost::live();
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &host).expect("vm should build");
    assert!(matches!(
        vm.run_process_until_effect().await,
        Ok(VmRunOutcome::EffectCompleted)
    ));
    let continuation = vm
        .suspend()
        .expect("a turn holding a nested projection must be capturable");
    drop(vm);

    let bytes = serde_json::to_vec(&continuation).expect("continuation should serialize");
    let restored: VmContinuation = lashlang::VmInstance::pristine()
        .open_continuation(&bytes)
        .expect("continuation should deserialize");

    let host = RestoreHost::live();
    let mut resumed =
        Vm::resume_from(restored, &program, &host).expect("continuation should resume");
    assert_eq!(
        resumed
            .run_process_until_effect()
            .await
            .expect("the resumed turn should finish"),
        VmRunOutcome::Complete(ExecutionOutcome::Finished(Value::String("live!".into())))
    );
}

/// `cause` and `errors` are ordinary values on a heap `Error`, persisted by the
/// heap encoders, so a projection reaches a restore through them as surely as
/// through a list (FIG-2865).
#[tokio::test(flavor = "current_thread")]
async fn a_projection_inside_an_error_survives_a_park() {
    let program = compile(
        r#"
        const failure = new Error("boom", { cause: report });
        const group = new AggregateError([report], "all failed");
        await tools.ping({});
        finish(failure.cause + "/" + group.errors[0]);
        "#,
    );

    let host = RestoreHost::live();
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &host).expect("vm should build");
    assert!(matches!(
        vm.run_process_until_effect().await,
        Ok(VmRunOutcome::EffectCompleted)
    ));
    let continuation = vm
        .suspend()
        .expect("a turn holding a projection in an error must be capturable");
    drop(vm);

    let bytes = serde_json::to_vec(&continuation).expect("continuation should serialize");
    let restored: VmContinuation = lashlang::VmInstance::pristine()
        .open_continuation(&bytes)
        .expect("continuation should deserialize");

    let host = RestoreHost::live();
    let mut resumed =
        Vm::resume_from(restored, &program, &host).expect("continuation should resume");
    assert_eq!(
        resumed
            .run_process_until_effect()
            .await
            .expect("the resumed turn should finish"),
        VmRunOutcome::Complete(ExecutionOutcome::Finished(Value::String(
            "live/live".into()
        )))
    );
}
