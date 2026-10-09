//! Durable restore of projections nested inside containers (FIG-2865).
//!
//! Both durable writers accept `Value::Projected`, nested occurrences
//! included, through one canonical shape. A projection is plain data (ADR 0132
//! §9): a scalar projection crosses as its name and value and a resource
//! projection as its `ResourceRef`, so nothing is rebound on restore. What
//! that buys a program is pinned here, from the outside, through the public
//! runtime API: a cell that puts a projected session binding inside a list,
//! or inside a heap `Error`, can park at its tool effect and resume reading
//! it. The
//! continuation wire used to refuse the value recursively, so
//! `const rows = [report]` made the turn uncapturable — while the `State`
//! snapshot persisted the identical value without complaint.
//!
//! A projection is one leaf of both encodings, a scalar member wherever it
//! sits. A scalar projection used to cross as its bare value, so a projected
//! record held by an object or an array became an inline record inside a
//! heap object, which both decoders refuse; and a binding that read a
//! projected record held it inline, so a closure capturing the binding took
//! it into the heap inline (FIG-5197).

use std::collections::BTreeSet;

use lash_core_execution::FleetFormat;
use lash_vm::{
    AbilityOp, AbilityOutcome, DurableBaseline, DurableFragment, ExecutionHost, ExecutionHostError,
    ExecutionMode, ExecutionOutcome, ProjectedBindings, ProjectedValue, State, Value, Vm,
    VmContinuation, VmInstance, VmRunOutcome,
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

fn compile(source: &str) -> lash_vm::CompiledProgram {
    let globals = BTreeSet::from(["report".to_string(), "rows".to_string()]);
    let program = lash_typescript::parse_with_globals(source, &globals)
        .unwrap_or_else(|error| panic!("`{source}` should parse: {error}"));
    lash_vm::testing::harness::try_compile_program(&program)
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
    let restored: VmContinuation = lash_vm::VmInstance::pristine()
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
    let restored: VmContinuation = lash_vm::VmInstance::pristine()
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

/// Binds `report` to the projected record `{ title: "q3" }` and runs cells.
struct RecordHost {
    process: bool,
}

impl ExecutionHost for RecordHost {
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
        if self.process {
            ExecutionMode::Process
        } else {
            ExecutionMode::Foreground
        }
    }

    fn projected_bindings(&self) -> ProjectedBindings {
        let mut bindings = ProjectedBindings::new();
        bindings.insert(
            "report",
            ProjectedValue::scalar(
                "report",
                lash_vm::from_json(serde_json::json!({ "title": "q3" })),
            ),
        );
        bindings
    }
}

/// Runs one cell over `state`, with the state's bindings and `report` in scope.
async fn run_cell(source: &str, state: &mut State) -> Value {
    let mut globals = state
        .binding_names()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    globals.insert("report".to_string());
    let program = lash_typescript::parse_with_globals(source, &globals)
        .unwrap_or_else(|error| panic!("`{source}` should parse: {error}"));
    let program = lash_vm::testing::harness::try_compile_program(&program)
        .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"));
    match lash_vm::execute(&program, state, &RecordHost { process: false })
        .await
        .expect("the cell should run")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

/// Captures `state` as the worker does, as durable fragments, and restores it
/// into a fresh instance.
fn reload(state: &State) -> State {
    let parts = state
        .durable_parts(&DurableBaseline::default(), FleetFormat::current())
        .expect("a state holding a projected record must be capturable");
    let bodies = parts.fragments.iter().map(|(name, fragment)| {
        let DurableFragment::Changed(body) = fragment else {
            panic!("a full capture writes every fragment")
        };
        (name.as_str(), body.as_slice())
    });
    let mut instance = VmInstance::pristine();
    instance
        .restore_durable_parts(&parts.header, bodies, FleetFormat::current())
        .expect("every captured fragment restores");
    instance.replace_state(State::new())
}

/// A projected record that a cell stores in an object or an array crosses a
/// reload, and a later cell reads it through its holder. A closure does not
/// outlive its cell's program (`cell_boundary_closures`), so its capture is
/// pinned by the park law below.
#[tokio::test(flavor = "current_thread")]
async fn a_projected_record_held_in_an_object_or_an_array_survives_a_reload() {
    for (holder, read) in [
        ("const holder = { doc: report, n: 1 };", "holder.doc.title"),
        ("const holder = [report];", "holder[0].title"),
    ] {
        let mut state = State::new();
        run_cell(&format!("{holder}\nfinish(1);"), &mut state).await;
        let mut state = reload(&state);
        assert_eq!(
            run_cell(&format!("finish({read});"), &mut state).await,
            Value::String("q3".into()),
            "`{holder}` read through `{read}` after a reload"
        );
    }
}

/// A turn holding a projected record in an object, an array and a closure
/// capture parks at its tool effect and reads each after resume.
#[tokio::test(flavor = "current_thread")]
async fn a_projected_record_held_in_an_object_an_array_and_a_closure_parks_and_resumes() {
    let program = compile(
        r#"
        const holder = { doc: report };
        const rows = [report];
        function make() { const doc = report; return () => doc.title; }
        const read = make();
        await tools.ping({});
        finish(holder.doc.title + rows[0].title + read());
        "#,
    );

    let host = RecordHost { process: true };
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &host).expect("vm should build");
    assert!(matches!(
        vm.run_process_until_effect().await,
        Ok(VmRunOutcome::EffectCompleted)
    ));
    let continuation = vm
        .suspend()
        .expect("a turn holding a projected record must be capturable");
    drop(vm);

    let bytes = serde_json::to_vec(&continuation).expect("continuation should serialize");
    let restored: VmContinuation = VmInstance::pristine()
        .open_continuation(&bytes)
        .expect("continuation should deserialize");

    let mut resumed =
        Vm::resume_from(restored, &program, &host).expect("continuation should resume");
    assert_eq!(
        resumed
            .run_process_until_effect()
            .await
            .expect("the resumed turn should finish"),
        VmRunOutcome::Complete(ExecutionOutcome::Finished(Value::String("q3q3q3".into())))
    );
}
