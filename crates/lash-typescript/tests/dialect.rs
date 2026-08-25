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
fn type_level_typescript_syntax_is_erased_while_namespaces_and_decorators_reject() {
    assert_eq!(
        finished(
            r#"
            interface Box<T> { value: T }
            type Numeric = number;
            function identity<T>(value: T): T { return value; }
            const value: Numeric = (identity<number>(2) as number)!;
            finish(value satisfies Numeric);
            "#,
        ),
        Value::Number(2.0)
    );
    for (source, code) in [
        (
            "namespace N {}",
            lash_typescript::DiagnosticCode::NamespaceUnsupported,
        ),
        (
            "@sealed class C {}",
            lash_typescript::DiagnosticCode::DecoratorUnsupported,
        ),
    ] {
        assert_eq!(
            lash_typescript::compile(source)
                .expect_err("runtime-emitting TypeScript syntax must reject")
                .code,
            code
        );
    }
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
        ("namespace N {}", Code::NamespaceUnsupported),
        ("eval('1')", Code::EvalUnsupported),
        ("new Function('return 1')", Code::NewUnsupported),
        ("label: while (true) break label;", Code::LabelUnsupported),
        (
            "const x = { get value() { return 1; } };",
            Code::AccessorUnsupported,
        ),
        ("import('x')", Code::DynamicImportUnsupported),
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
fn resumed_typescript_can_capture_aliases_created_after_the_first_suspend() {
    futures::executor::block_on(async {
        let program = lash_typescript::compile(
            "const shared = { value: 1 }; print(1); const holder = { a: shared, b: shared }; print(2); finish(holder.a.value);",
        )
        .expect("TypeScript should compile");
        let mut state = State::new();
        let mut vm = Vm::from_state(&program, &mut state, &Host).expect("install TypeScript VM");
        assert_eq!(
            vm.run_process_until_effect().await.expect("first print"),
            VmRunOutcome::EffectCompleted
        );
        let first = vm.suspend().expect("first suspend");
        assert!(
            !first.reference_semantics,
            "the first heap is still a forest"
        );

        let encoded = serde_json::to_vec(&first).expect("encode continuation");
        let decoded = serde_json::from_slice(&encoded).expect("decode continuation");
        let mut resumed = Vm::resume_from(decoded, &program, &Host).expect("resume TypeScript VM");
        assert_eq!(
            resumed
                .run_process_until_effect()
                .await
                .expect("second print"),
            VmRunOutcome::EffectCompleted
        );
        let second = resumed
            .suspend()
            .expect("capture aliases created after resume");
        assert!(
            second.reference_semantics,
            "the shared heap requires the marker"
        );
    });
}

#[test]
fn lashlang_resume_refuses_an_authored_typescript_reference_marker() {
    futures::executor::block_on(async {
        let program = lashlang::compile("print 1\nfinish 2").expect("compile Lashlang");
        let mut state = State::new();
        let mut vm = Vm::from_state(&program, &mut state, &Host).expect("install Lashlang VM");
        vm.run_process_until_effect().await.expect("run to print");
        let continuation = vm.suspend().expect("capture Lashlang continuation");
        let mut authored = serde_json::to_value(continuation).expect("encode continuation");
        authored
            .as_object_mut()
            .expect("continuation object")
            .insert("reference_semantics".into(), serde_json::Value::Bool(true));
        let decoded = serde_json::from_value(authored).expect("decode authored continuation");

        assert!(
            Vm::resume_from(decoded, &program, &Host).is_err(),
            "a TypeScript reference marker must not select Lashlang VM semantics"
        );
    });
}

#[test]
fn lashlang_execution_refuses_a_shared_typescript_state() {
    futures::executor::block_on(async {
        let typescript = lash_typescript::compile(
            "const shared = { value: 1 }; const holder = { a: shared, b: shared }; finish(holder.a.value);",
        )
        .expect("compile TypeScript");
        let mut state = State::new();
        lashlang::execute(&typescript, &mut state, &Host)
            .await
            .expect("create shared TypeScript state");

        let lashlang = lashlang::compile("finish 1").expect("compile Lashlang");
        assert!(
            lashlang::execute(&lashlang, &mut state, &Host)
                .await
                .is_err(),
            "Lashlang must reject a shared heap regardless of the stored marker"
        );
    });
}

fn normalized_continuation_bytes(
    program: &lashlang::CompiledProgram,
    host: &impl ExecutionHost,
) -> Vec<u8> {
    futures::executor::block_on(async {
        let mut state = State::new();
        let mut vm = Vm::from_state(program, &mut state, host).expect("install VM state");
        assert_eq!(
            vm.run_process_until_effect().await.expect("run to print"),
            VmRunOutcome::EffectCompleted
        );
        let continuation = vm.suspend().expect("capture continuation");
        let mut canonical = serde_json::to_value(continuation).expect("canonicalize continuation");
        canonical
            .as_object_mut()
            .expect("continuation object")
            .remove("active_execution_elapsed");
        serde_json::to_vec(&canonical).expect("encode deterministic continuation")
    })
}

#[test]
fn equivalent_lashlang_and_typescript_have_identical_continuation_bytes() {
    let lashlang = lashlang::compile("print 1\nfinish 2").expect("compile Lashlang");
    let typescript = lash_typescript::compile("print(1); finish(2);").expect("compile TypeScript");

    assert_eq!(
        normalized_continuation_bytes(&typescript, &Host),
        normalized_continuation_bytes(&lashlang, &Host)
    );
}

#[test]
fn continuation_is_cross_process_deterministic_and_gc_stable() {
    let normal = suspended_typescript_run(false);
    let stress = suspended_typescript_run(true);

    assert_eq!(normal, stress);
    assert_eq!(normal.1, ExecutionOutcome::Finished(Value::Number(2.0)));
}

#[test]
fn typescript_determinism_process_probe() {
    if std::env::var_os("LASH_TYPESCRIPT_DETERMINISM_PROBE").is_none() {
        return;
    }
    let (bytes, outcome) = suspended_typescript_run(false);
    assert_eq!(outcome, ExecutionOutcome::Finished(Value::Number(2.0)));
    let encoded = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("TYPESCRIPT_CONTINUATION:{encoded}");
}

#[test]
fn independent_processes_dump_identical_typescript_continuations() {
    fn probe() -> String {
        let output = std::process::Command::new(std::env::current_exe().expect("current test exe"))
            .args([
                "--exact",
                "dialect::typescript_determinism_process_probe",
                "--nocapture",
            ])
            .env("LASH_TYPESCRIPT_DETERMINISM_PROBE", "1")
            .output()
            .expect("run continuation probe in a fresh process");
        assert!(
            output.status.success(),
            "probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("probe output is UTF-8")
            .lines()
            .find_map(|line| line.strip_prefix("TYPESCRIPT_CONTINUATION:"))
            .expect("probe emits continuation bytes")
            .to_owned()
    }

    assert_eq!(probe(), probe());
}

/// Every program the dialect accepts must survive the durability boundary and
/// must never publish a generated binding into the session's global surface.
mod durability {
    use super::*;

    /// Programs that exercise the whole accepted binding surface, each one
    /// suspended at a `print` effect and snapshotted the way an RLM session
    /// does between turns.
    const DURABLE_CORPUS: &[&str] = &[
        "const shared = { value: 1 }; const alias = shared; print('x'); finish(`${alias.value}`);",
        "function fact(n: number): number { if (n <= 1) { return 1; } return fact(n - 1) * n; } print('x'); finish(`${fact(5)}`);",
        "const top = 9; function outerFn(): number { function innerFn(): number { return top; } return innerFn(); } print('x'); finish(`${outerFn()}`);",
        "const base = 10; const outer = () => { const inner = () => base; return inner; }; print('x'); finish(`${outer()()}`);",
        "const items = [1, 2, 3]; const total = items.length; print('x'); finish(`${total}`);",
        "const g = (n: number): number => n + 1; const h = (n: number): number => g(n) * 2; print('x'); finish(`${h(3)}`);",
        // Root-level block scopes: their bindings must not publish generated
        // names into the durable global surface.
        "let r = 'x'; try { throw 'boom'; } catch (e) { r = e; } print('x'); finish(r);",
        "{ const inner = 1; } print('x'); finish('done');",
        "if (1) { const branch = 2; } print('x'); finish('done');",
        "try { const attempted = 1; } finally { const cleaned = 2; } print('x'); finish('done');",
        "const g = function self(n: number): number { if (n <= 0) { return 0; } return self(n - 1); }; print('x'); finish(`${g(3)}`);",
        "const key = { id: 1 }; const map = new Map([[key, 'value']]); const alias = map; print('x'); finish(`${alias.get(key)}|${alias === map}`);",
        "const set = new Set([NaN, -0, 2]); const alias = set; print('x'); finish(`${alias.has(NaN)}|${alias === set}`);",
        "const date = new Date('2000-02-29T12:34:56.789Z'); const alias = date; print('x'); finish(`${alias.toISOString()}|${alias === date}`);",
        "enum Status { Ready, Done = 4 } const alias = Status; print('x'); finish(`${alias.Ready}|${alias[4]}`);",
    ];

    fn suspended_continuation_json(source: &str) -> String {
        futures::executor::block_on(async move {
            let program = lash_typescript::compile(source)
                .unwrap_or_else(|error| panic!("compile `{source}`: {error}"));
            let mut state = State::new();
            let mut vm = Vm::from_state(&program, &mut state, &Host).expect("install VM state");
            assert_eq!(
                vm.run_process_until_effect().await.expect("run to print"),
                VmRunOutcome::EffectCompleted,
                "{source}"
            );
            let continuation = vm
                .suspend()
                .unwrap_or_else(|error| panic!("suspend `{source}`: {error}"));
            serde_json::to_string(&continuation).expect("encode continuation")
        })
    }

    fn suspend_and_snapshot(source: &str) -> Vec<String> {
        futures::executor::block_on(async move {
            let program = lash_typescript::compile(source)
                .unwrap_or_else(|error| panic!("compile `{source}`: {error}"));
            let mut state = State::new();
            let mut vm = Vm::from_state(&program, &mut state, &Host).expect("install VM state");
            assert_eq!(
                vm.run_process_until_effect().await.expect("run to print"),
                VmRunOutcome::EffectCompleted,
                "{source}"
            );
            // The continuation across an effect boundary must encode.
            let continuation = vm
                .suspend()
                .unwrap_or_else(|error| panic!("suspend `{source}`: {error}"));
            serde_json::to_vec(&continuation).expect("encode continuation");
            while let VmRunOutcome::EffectCompleted = vm
                .run_process_until_effect()
                .await
                .unwrap_or_else(|error| panic!("finish `{source}`: {error}"))
            {}
            drop(vm);
            // The between-turn snapshot must encode too.
            let snapshot = state.snapshot();
            snapshot
                .to_canonical_bytes()
                .unwrap_or_else(|error| panic!("snapshot `{source}`: {error}"));
            snapshot
                .globals()
                .iter()
                .map(|(name, _)| name.to_string())
                .collect::<Vec<_>>()
        })
    }

    #[test]
    fn accepted_programs_suspend_and_snapshot() {
        for source in DURABLE_CORPUS {
            suspend_and_snapshot(source);
        }
    }

    #[test]
    fn map_set_and_date_aliases_survive_a_continuation_restart() {
        futures::executor::block_on(async {
            let source = "const key={id:1}; const map=new Map([[key,'value']]); const mapAlias=map; const set=new Set([key,NaN]); const setAlias=set; const date=new Date('2000-02-29T12:34:56.789Z'); const dateAlias=date; print('park'); finish(`${mapAlias===map}|${mapAlias.get(key)}|${setAlias===set}|${setAlias.has(key)}|${setAlias.has(NaN)}|${dateAlias===date}|${dateAlias.toISOString()}`);";
            let program = lash_typescript::compile(source).expect("durable exotics compile");
            let mut state = State::new();
            let mut vm = Vm::from_state(&program, &mut state, &Host).expect("install VM state");
            assert_eq!(
                vm.run_process_until_effect().await.expect("run to park"),
                VmRunOutcome::EffectCompleted
            );
            let encoded = serde_json::to_vec(&vm.suspend().expect("suspend exotics"))
                .expect("encode continuation");
            let continuation = serde_json::from_slice(&encoded).expect("decode continuation");
            let mut resumed =
                Vm::resume_from(continuation, &program, &Host).expect("resume exotics");
            let VmRunOutcome::Complete(outcome) = resumed
                .run_process_until_effect()
                .await
                .expect("finish after restart")
            else {
                panic!("the resumed program should have no second effect");
            };
            assert_eq!(
                outcome,
                ExecutionOutcome::Finished(Value::String(
                    "true|value|true|true|true|true|2000-02-29T12:34:56.789Z".into()
                ))
            );
        });
    }

    /// The globals an RLM session carries between turns, which is also what the
    /// bound-variables prompt renders from.
    fn persisted_globals(source: &str) -> Vec<String> {
        let program = lash_typescript::compile(source)
            .unwrap_or_else(|error| panic!("compile `{source}`: {error}"));
        let mut state = State::new();
        futures::executor::block_on(lashlang::execute(&program, &mut state, &Host))
            .unwrap_or_else(|error| panic!("execute `{source}`: {error}"));
        state
            .snapshot()
            .globals()
            .iter()
            .map(|(name, _)| name.to_string())
            .collect()
    }

    #[test]
    fn no_generated_binding_reaches_the_global_surface() {
        for source in DURABLE_CORPUS {
            for name in persisted_globals(source) {
                assert!(
                    !name.starts_with("__typescript"),
                    "generated binding `{name}` leaked into the globals of `{source}`"
                );
            }
            // The suspended continuation is the other durable artifact.
            assert!(
                !suspended_continuation_json(source).contains("__typescript"),
                "a generated binding leaked into the continuation of `{source}`"
            );
        }
        // A block binding that shadows an outer name is the one shape that
        // still needs a generated slot. It stays out of the model-facing
        // surface, which filters the generated prefix.
        let shadowing = "const e = 'outer'; let seen = ''; try { throw 'boom'; } catch (e) { seen = e; } finish(`${e}|${seen}`);";
        assert!(
            persisted_globals(shadowing)
                .iter()
                .any(|name| name.starts_with(lash_typescript::GENERATED_BINDING_PREFIX)),
            "a shadowing binding needs a slot of its own"
        );
        assert!(
            lash_typescript::GENERATED_BINDING_PREFIX.starts_with("__typescript"),
            "the reserved prefix is what callers filter on"
        );
    }

    #[test]
    fn mutually_recursive_declarations_reject_before_they_can_be_persisted() {
        // v1 cannot durably encode the heap cycle these declarations require,
        // so the rejection is static — never a persistence failure later.
        for source in [
            "function isEven(n: number): boolean { if (n === 0) { return true; } return isOdd(n - 1); } function isOdd(n: number): boolean { if (n === 0) { return false; } return isEven(n - 1); } print('x'); finish(`${isEven(4)}`);",
            "function a(n: number): number { if (n === 0) { return 0; } return b(n - 1); } function b(n: number): number { return c(n); } function c(n: number): number { return 1 + a(n); } finish(`${a(3)}`);",
            "function shell(n: number): number { function up(k: number): number { if (k === 0) { return 0; } return down(k - 1) + 1; } function down(k: number): number { return up(k); } return up(n); } finish(`${shell(5)}`);",
        ] {
            let error = lash_typescript::compile(source)
                .expect_err("mutually recursive declarations must reject statically");
            assert_eq!(
                error.code,
                lash_typescript::DiagnosticCode::MutualRecursionUnsupported,
                "{source}"
            );
            assert!(
                error.to_string().contains(" -> "),
                "the diagnostic must name the cycle: {error}"
            );
        }
    }
}

/// A process suspended inside a `for…of` body resumes across a continuation
/// round-trip.
///
/// The for-of filter's register text rests on this; the claim had no test in
/// the layer, so the narrowing that let bodies do work was justified by an
/// assertion nothing executed.
#[test]
fn a_process_suspended_inside_for_of_resumes() {
    futures::executor::block_on(async {
        let program = lash_typescript::compile(
            "const xs = [1, 2, 3]; let total = 0; for (const x of xs) { print('step'); total = total + x; } finish(`${total}`);",
        )
        .expect("for-of with an effect compiles");
        let mut state = State::new();
        let mut vm = Vm::from_state(&program, &mut state, &Host).expect("install VM state");
        let mut suspensions = 0;
        let outcome = loop {
            match vm
                .run_process_until_effect()
                .await
                .expect("run to the next effect")
            {
                VmRunOutcome::EffectCompleted => {
                    // Round-trip the continuation at every effect boundary.
                    let continuation = vm.suspend().expect("suspend inside the loop");
                    let encoded = serde_json::to_vec(&continuation).expect("encode");
                    let decoded = serde_json::from_slice(&encoded).expect("decode");
                    vm = Vm::resume_from(decoded, &program, &Host).expect("resume");
                    suspensions += 1;
                }
                VmRunOutcome::Complete(outcome) => break outcome,
            }
        };
        assert!(suspensions >= 3, "each loop step suspends: {suspensions}");
        assert_eq!(
            outcome,
            ExecutionOutcome::Finished(Value::String("6".into()))
        );
    });
}

/// An array mutated with `push` survives a park in the middle of the loop that
/// is filling it.
///
/// `push`/`pop`/`shift`/`unshift` mutate a live heap array, so the half-filled
/// array is durable state at every suspension point inside the loop. This runs
/// to the first effect, captures the continuation, encodes and decodes it as
/// the durable path does, resumes from the decoded form, and asserts the
/// finished array is the one an uninterrupted run produces.
#[test]
fn array_mutators_survive_a_park_in_the_middle_of_the_loop() {
    let source = "const out: number[] = []; for (let i = 0; i < 4; i++) { out.push(i); print(out.length); } out.unshift(-1); out.pop(); finish(out.join(','));";
    let program = lash_typescript::compile(source).expect("the mutating loop should compile");

    let resumed = futures::executor::block_on(async {
        let mut state = State::new();
        let mut vm = Vm::from_state(&program, &mut state, &Host).expect("install VM state");
        assert_eq!(
            vm.run_process_until_effect()
                .await
                .expect("run to the first print"),
            VmRunOutcome::EffectCompleted
        );
        let continuation = vm.suspend().expect("capture the mid-loop continuation");
        let encoded = serde_json::to_vec(&continuation).expect("encode the continuation");
        let decoded = serde_json::from_slice(&encoded).expect("decode the continuation");
        let mut vm = Vm::resume_from(decoded, &program, &Host).expect("resume mid-loop");
        loop {
            match vm
                .run_process_until_effect()
                .await
                .expect("resume to finish")
            {
                VmRunOutcome::EffectCompleted => continue,
                other => break other,
            }
        }
    });

    let expected = ExecutionOutcome::Finished(Value::String("-1,0,1,2".into()));
    assert_eq!(
        run(source),
        expected,
        "the uninterrupted run is the reference"
    );
    assert_eq!(
        resumed,
        VmRunOutcome::Complete(expected),
        "the resumed run must reach the same finish"
    );
}
