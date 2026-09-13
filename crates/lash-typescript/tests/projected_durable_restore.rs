//! Durable restore of projections nested inside containers (FIG-2865).
//!
//! Both durable writers now accept `Value::Projected`, nested occurrences
//! included, through one canonical shape. What that buys a program is pinned
//! here, from the outside, through the public runtime API:
//!
//! * A cell that puts a projected session binding inside a list can park at its
//!   tool effect at all. The continuation wire used to refuse the value
//!   recursively, so `const rows = [report]` made the turn uncapturable — while
//!   the `State` snapshot persisted the identical value without complaint.
//! * A restored placeholder, top-level or nested, is re-bound to the live host
//!   view when the host re-supplies a binding of the same name. The old refresh
//!   was keyed on slot names, so the copy inside the list was never revisited.
//! * A placeholder nothing re-supplied refuses every read with a typed error.
//!   It used to materialize the unavailability sentence as the value, so a cell
//!   read an English diagnostic where the host's view belonged, and finished.

use std::collections::BTreeSet;

use lashlang::{
    AbilityOp, AbilityResult, CompilationDialect, ExecutionHost, ExecutionHostError, ExecutionMode,
    ExecutionOutcome, ProjectedBindings, ProjectedValue, Record, RuntimeError, Snapshot, State,
    Value, Vm, VmContinuation, VmRunOutcome,
};

/// Runs cells in process mode, answers the one tool call the park test makes,
/// and re-supplies `report` only when asked to.
struct RestoreHost {
    report: Option<Value>,
}

impl RestoreHost {
    fn live() -> Self {
        Self {
            report: Some(Value::String("live".into())),
        }
    }

    fn without_bindings() -> Self {
        Self { report: None }
    }
}

impl ExecutionHost for RestoreHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::ResourceOperation(_) => Ok(AbilityResult::Value(Value::Number(7.0))),
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
    lashlang::compile_ast_with_dialect(&program, CompilationDialect::Typescript)
        .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"))
}

/// A projected binding nested in a list parks with the turn and comes back as
/// the host's live view.
#[tokio::test(flavor = "current_thread")]
async fn a_nested_projection_parks_and_resumes_against_the_live_binding() {
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
    let restored: VmContinuation =
        serde_json::from_slice(&bytes).expect("continuation should deserialize");

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

/// The same continuation, resumed by a host that re-supplies nothing: the read
/// refuses rather than answering with a sentence.
#[tokio::test(flavor = "current_thread")]
async fn an_unrefreshed_nested_projection_refuses_the_read_after_resume() {
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
    vm.run_process_until_effect()
        .await
        .expect("the effect should complete");
    let continuation = vm.suspend().expect("capturable");
    drop(vm);

    let bytes = serde_json::to_vec(&continuation).expect("continuation should serialize");
    let restored: VmContinuation =
        serde_json::from_slice(&bytes).expect("continuation should deserialize");

    let host = RestoreHost::without_bindings();
    let mut resumed =
        Vm::resume_from(restored, &program, &host).expect("continuation should resume");
    let error = resumed
        .run_process_until_effect()
        .await
        .expect_err("an unrefreshed placeholder must refuse");
    assert!(
        matches!(
            error,
            RuntimeError::ProjectedValueUnavailable { ref name, ref type_name }
                if name == "report" && type_name == "string"
        ),
        "unexpected error: {error:?}"
    );
}

/// The snapshot wire's half of the same guarantee: a nested placeholder decoded
/// from canonical bytes is re-bound when the binding is re-supplied.
#[tokio::test(flavor = "current_thread")]
async fn a_nested_snapshot_placeholder_refreshes_from_a_re_supplied_binding() {
    let mut state = restored_state_with_a_nested_placeholder();
    let program = compile(r#"finish(rows[0] + "!");"#);
    let host = RestoreHost::live();
    let outcome = lashlang::execute(&program, &mut state, &host)
        .await
        .expect("the refreshed placeholder should read");
    assert_eq!(
        outcome,
        ExecutionOutcome::Finished(Value::String("live!".into()))
    );
}

/// And refuses, typed, when it is not.
#[tokio::test(flavor = "current_thread")]
async fn an_unrefreshed_nested_snapshot_placeholder_refuses_the_read() {
    let mut state = restored_state_with_a_nested_placeholder();
    let program = compile(r#"finish(rows[0] + "!");"#);
    let host = RestoreHost::without_bindings();
    let error = lashlang::execute(&program, &mut state, &host)
        .await
        .expect_err("an unrefreshed placeholder must refuse");
    assert!(
        matches!(
            error,
            RuntimeError::ProjectedValueUnavailable { ref name, ref type_name }
                if name == "report" && type_name == "string"
        ),
        "unexpected error: {error:?}"
    );
}

/// A state restored from canonical snapshot bytes whose `rows` global holds a
/// projection *inside* a list.
fn restored_state_with_a_nested_placeholder() -> State {
    let snapshot = Snapshot::new(Record::from_iter([(
        "rows".to_string(),
        Value::List(
            vec![Value::Projected(ProjectedValue::scalar(
                "report",
                Value::String("stale".into()),
            ))]
            .into(),
        ),
    )]));
    let encoded = snapshot.to_canonical_bytes().expect("snapshot encode");
    let decoded = Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");
    State::from_snapshot(decoded)
}
