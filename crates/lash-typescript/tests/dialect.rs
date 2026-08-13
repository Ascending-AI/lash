use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
    Vm, VmRunOutcome,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unsupported test ability")),
        }
    }
}

fn run(source: &str) -> ExecutionOutcome {
    let program = lash_typescript::compile(source).expect("TypeScript should compile");
    futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host))
        .expect("TypeScript should execute")
}

fn finished(source: &str) -> Value {
    match run(source) {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

#[derive(Default)]
struct JournalHost(std::sync::Mutex<Vec<(String, Value)>>);

impl ExecutionHost for JournalHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        let (kind, value) = match op {
            AbilityOp::Print(value) => ("print", value),
            AbilityOp::Finish(value) => ("finish", value),
            _ => return Err(ExecutionHostError::new("unsupported journal ability")),
        };
        self.0
            .lock()
            .expect("journal lock")
            .push((kind.to_string(), value.clone()));
        Ok(AbilityResult::Value(value))
    }
}

#[test]
fn equivalent_lashlang_and_typescript_share_vm_behavior() {
    fn execute(
        program: &lashlang::CompiledProgram,
        host: &JournalHost,
    ) -> (ExecutionOutcome, Vec<u8>) {
        let mut state = State::new();
        let outcome = futures::executor::block_on(lashlang::execute(program, &mut state, host))
            .expect("execute equivalent program");
        let bytes = state
            .snapshot()
            .to_canonical_bytes()
            .expect("canonical state");
        (outcome, bytes)
    }

    let lashlang = lashlang::compile("value = 1 + 2\nprint value\nfinish value == 3")
        .expect("compile Lashlang");
    let typescript =
        lash_typescript::compile("const value: number = 1 + 2; print(value); finish(value === 3);")
            .expect("compile TypeScript");
    let lashlang_host = JournalHost::default();
    let typescript_host = JournalHost::default();

    assert_eq!(
        execute(&typescript, &typescript_host),
        execute(&lashlang, &lashlang_host)
    );
    assert_eq!(
        *typescript_host.0.lock().expect("TypeScript journal"),
        *lashlang_host.0.lock().expect("Lashlang journal")
    );
}

#[test]
fn primitives_coercion_equality_and_templates_follow_ecma() {
    assert_eq!(finished("finish(1 + '2');"), Value::String("12".into()));
    assert_eq!(finished("finish('' == 0);"), Value::Bool(true));
    assert_eq!(finished("finish(null == undefined);"), Value::Bool(true));
    assert_eq!(finished("finish(null === undefined);"), Value::Bool(false));
    assert_eq!(
        finished("finish(`answer=${40 + 2}`);"),
        Value::String("answer=42".into())
    );
    assert_eq!(
        finished("finish(typeof undefined);"),
        Value::String("undefined".into())
    );
}

#[test]
fn logical_operators_return_the_selected_operand() {
    assert_eq!(
        finished("finish(0 || 'fallback');"),
        Value::String("fallback".into())
    );
    assert_eq!(finished("finish('left' && 7);"), Value::Number(7.0));
    assert_eq!(finished("finish(null ?? 9);"), Value::Number(9.0));
    assert_eq!(finished("finish(0 ?? 9);"), Value::Number(0.0));
}

#[test]
fn object_aliases_and_argument_aliases_are_shared() {
    assert_eq!(
        finished("const a = { value: 1 }; const b = a; b.value = 2; finish(a.value);"),
        Value::Number(2.0)
    );
    assert_eq!(
        finished(
            "function setValue(x: { value: number }): void { x.value = 5; } const a = { value: 1 }; setValue(a); finish(a.value);"
        ),
        Value::Number(5.0)
    );
}

#[test]
fn captured_objects_keep_reference_identity() {
    assert_eq!(
        finished(
            "const state = { value: 1 }; const bump = () => { state.value = state.value + 1; }; bump(); finish(state.value);"
        ),
        Value::Number(2.0)
    );
}

#[test]
fn return_runs_and_can_be_replaced_by_finally() {
    assert_eq!(
        finished(
            "function answer(): number { try { return 1; } finally { return 2; } } finish(answer());"
        ),
        Value::Number(2.0)
    );
}

#[test]
fn undefined_erases_at_the_json_boundary() {
    let value = finished("finish([undefined, { absent: undefined, present: null }]);");
    assert_eq!(
        serde_json::to_value(value).expect("value serializes"),
        serde_json::json!([null, { "present": null }])
    );
}

#[test]
fn accepted_string_methods_use_the_vm_intrinsics() {
    assert_eq!(
        finished("finish('  Lash  '.trim().toLowerCase());"),
        Value::String("lash".into())
    );
    assert_eq!(
        finished("finish('typescript'.startsWith('type'));"),
        Value::Bool(true)
    );
}

#[test]
fn unsupported_constructs_have_stable_named_diagnostics() {
    use lash_typescript::DiagnosticCode as Code;
    let cases = [
        ("class A {}", Code::ClassUnsupported),
        ("function* f() {}", Code::GeneratorUnsupported),
        ("async function f() {}", Code::AsyncUnsupported),
        ("enum E { A }", Code::EnumUnsupported),
        ("namespace N {}", Code::NamespaceUnsupported),
        ("const r = /x/;", Code::RegExpUnsupported),
        ("eval('1')", Code::EvalUnsupported),
        ("new Function('return 1')", Code::NewUnsupported),
        ("label: while (true) break label;", Code::LabelUnsupported),
        (
            "const x = { get value() { return 1; } };",
            Code::AccessorUnsupported,
        ),
        ("import('x')", Code::DynamicImportUnsupported),
        ("const x = { ...other };", Code::SpreadUnsupported),
        ("const x = other?.value;", Code::OptionalChainingUnsupported),
        ("const x = this;", Code::ThisUnsupported),
    ];
    for (source, expected) in cases {
        let error = lash_typescript::validate(source).expect_err(source);
        assert_eq!(error.code, expected, "{source}: {error}");
        assert!(error.to_string().starts_with(expected.as_str()));
    }
}

#[test]
fn temporal_dead_zone_and_const_assignment_reject_before_execution() {
    assert_eq!(
        lash_typescript::validate("const x = x;").unwrap_err().code,
        lash_typescript::DiagnosticCode::TemporalDeadZone
    );
    assert_eq!(
        lash_typescript::validate("const x = 1; x = 2;")
            .unwrap_err()
            .code,
        lash_typescript::DiagnosticCode::AssignConst
    );
}

struct DurabilityHost {
    stress_gc: bool,
}

impl ExecutionHost for DurabilityHost {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        Host.perform(op).await
    }

    fn collect_heap_every_allocation(&self) -> bool {
        self.stress_gc
    }
}

fn suspended_typescript_run(stress_gc: bool) -> (Vec<u8>, ExecutionOutcome) {
    futures::executor::block_on(async move {
        let program = lash_typescript::compile(
            "const shared = { value: 1 }; const alias = shared; print(alias.value); shared.value = 2; finish(alias.value);",
        )
        .expect("TypeScript should compile");
        let host = DurabilityHost { stress_gc };
        let mut state = State::new();
        let mut vm = Vm::from_state(&program, &mut state, &host).expect("install VM state");
        assert_eq!(
            vm.run_process_until_effect().await.expect("run to print"),
            VmRunOutcome::EffectCompleted
        );
        let continuation = vm.suspend().expect("capture TypeScript continuation");
        let wire_bytes = serde_json::to_vec(&continuation).expect("encode continuation");
        let mut canonical = serde_json::to_value(&continuation).expect("canonicalize continuation");
        canonical
            .as_object_mut()
            .expect("continuation object")
            .remove("active_execution_elapsed");
        let bytes = serde_json::to_vec(&canonical).expect("encode deterministic continuation");
        let restored =
            serde_json::from_slice(&wire_bytes).expect("decode in a fresh process image");
        let mut resumed = Vm::resume_from(restored, &program, &host).expect("resume TypeScript");
        let outcome = loop {
            match resumed
                .run_process_until_effect()
                .await
                .expect("finish resumed TypeScript")
            {
                VmRunOutcome::EffectCompleted => {}
                VmRunOutcome::Complete(outcome) => break outcome,
            }
        };
        (bytes, outcome)
    })
}

#[test]
fn continuation_is_cross_process_deterministic_and_gc_stable() {
    let normal = suspended_typescript_run(false);
    let stress = suspended_typescript_run(true);

    assert_eq!(normal, stress);
    assert_eq!(normal.1, ExecutionOutcome::Finished(Value::Number(2.0)));
}
