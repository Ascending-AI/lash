//! String building through fused add-assign (FIG-3733).
//!
//! `s = s + rhs` and `s += rhs` on a bare name compile to one opcode that
//! appends into the accumulator's own buffer when nothing else shares it.
//! These cases pin the semantics that fusion must not move: ECMA-262's `+`
//! coercion and left-to-right order, the copy-on-write boundary that keeps a
//! captured alias on the bytes it saw, the durable writers that still flatten
//! a string to its text, and the size limit the fused path checks before it
//! grows.

use std::collections::BTreeSet;

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionMode, ExecutionOutcome,
    ProjectedBindings, ProjectedValue, Record, RuntimeError, Snapshot, State, Value, Vm,
    VmContinuation, VmRunOutcome,
};

/// Answers the one tool call the suspend tests make and re-supplies `report`
/// when the case asks for a projected operand.
struct Host {
    report: Option<Value>,
}

impl Host {
    fn plain() -> Self {
        Self { report: None }
    }

    fn live() -> Self {
        Self {
            report: Some(Value::String("live".into())),
        }
    }
}

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

    fn projected_bindings(&self) -> ProjectedBindings {
        let mut bindings = ProjectedBindings::new();
        if let Some(report) = &self.report {
            bindings.insert("report", ProjectedValue::scalar("report", report.clone()));
        }
        bindings
    }
}

fn compile(source: &str) -> lashlang::CompiledProgram {
    lash_typescript::testing::compile(source).unwrap_or_else(|error| panic!("`{source}`: {error}"))
}

fn compile_with_report(source: &str) -> lashlang::CompiledProgram {
    let globals = BTreeSet::from(["report".to_string()]);
    let program = lash_typescript::parse_with_globals(source, &globals)
        .unwrap_or_else(|error| panic!("`{source}` should parse: {error}"));
    lashlang::testing::harness::try_compile_program(&program)
        .unwrap_or_else(|error| panic!("`{source}` should compile: {error}"))
}

fn execute(source: &str) -> Result<ExecutionOutcome, RuntimeError> {
    futures::executor::block_on(lashlang::execute(
        &compile(source),
        &mut State::new(),
        &Host::plain(),
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

/// Both spellings of `s = s + rhs` on a bare name build the same string, in a
/// loop and across the forms the fused opcode must answer: a literal, a
/// variable, an empty right, and an astral character.
#[test]
fn repeated_string_concat_builds_the_same_string_by_either_spelling() {
    assert_eq!(
        finished(
            "let s = \"\";\nfor (let i = 0; i < 8; i++) { s = s + \"x\"; }\nfinish(`${s.length}:${s}`);"
        ),
        text("8:xxxxxxxx")
    );
    assert_eq!(
        finished(
            "let s = \"\";\nfor (let i = 0; i < 8; i++) { s += \"y\"; }\nfinish(`${s.length}:${s}`);"
        ),
        text("8:yyyyyyyy")
    );
    assert_eq!(
        finished(
            "let s = \"a\";\nconst t = \"bc\";\ns += t;\ns += \"\";\ns += \"🙂\";\nfinish(s);"
        ),
        text("abc🙂")
    );
    assert_eq!(
        finished("let s = \"x\";\ns = s + s;\ns = s + s;\nfinish(`${s}:${s.length}`);"),
        text("xxxx:4")
    );
}

/// A binding that captured the accumulator keeps the bytes it saw: the fused
/// append writes through copy-on-write storage, so `t` still names the old
/// text after `s` grows.
#[test]
fn an_alias_taken_before_the_append_keeps_the_old_text() {
    assert_eq!(
        finished("let s = \"a\";\nconst t = s;\ns = s + \"x\";\nfinish(`${t}|${s}`);"),
        text("a|ax")
    );
    assert_eq!(
        finished("let s = \"a\";\nconst t = s;\ns += \"x\";\nfinish(`${t}|${s}`);"),
        text("a|ax")
    );
    // The alias through a container is the same share.
    assert_eq!(
        finished(
            "let s = \"a\";\nconst bag = [s];\ns += \"b\";\ns += \"c\";\nfinish(`${bag[0]}|${s}`);"
        ),
        text("a|abc")
    );
}

/// `+` coerces the way ECMA-262 does regardless of which side carries the
/// string: numbers, booleans, null, undefined, and arrays all stringify into
/// the append, and a number accumulator still adds.
#[test]
fn add_assign_coerces_both_operand_shapes() {
    assert_eq!(
        finished("let s = \"v\";\ns += 1;\ns += true;\ns += null;\ns += undefined;\nfinish(s);"),
        text("v1truenullundefined")
    );
    assert_eq!(
        finished("let s = \"v\";\ns += [1, 2];\nfinish(s);"),
        text("v1,2")
    );
    assert_eq!(finished("let n = 1;\nn += 2;\nfinish(`${n}`);"), text("3"));
    assert_eq!(
        finished("let n = 0.5;\nn = n + 0.25;\nfinish(`${n}`);"),
        text("0.75")
    );
}

/// The assignment's value is the stored value in operand position, and an
/// impure right operand — one that writes the same slot — still evaluates
/// strictly left-to-right, so the unfused order is the observed one.
#[test]
fn add_assign_value_and_evaluation_order_hold() {
    assert_eq!(
        finished("let s = \"a\";\nfinish(`${(s = s + \"x\")}|${s}`);"),
        text("ax|ax")
    );
    assert_eq!(
        finished("let s = \"a\";\nfinish(`${(s += \"x\")}|${s}`);"),
        text("ax|ax")
    );
    // The left operand reads `s` before the nested assignment writes it.
    assert_eq!(
        finished("let s = \"a\";\ns = s + (s = \"y\");\nfinish(s);"),
        text("ay")
    );
    assert_eq!(
        finished("let s = \"a\";\ns += (s = \"y\");\nfinish(s);"),
        text("ay")
    );
    // A nested assignment in a live list operand lands at its own position.
    assert_eq!(
        finished(
            "let s = \"a\";\nconst out = [(s = s + \"b\"), s];\ns += \"c\";\nfinish(JSON.stringify([out, s]));"
        ),
        text("[[\"ab\",\"ab\"],\"abc\"]")
    );
}

/// A projected operand materializes through the async path before the append:
/// `s + report` and `s += report` both read the host's live view.
#[tokio::test(flavor = "current_thread")]
async fn a_projected_operand_materializes_before_the_append() {
    let program = compile_with_report(
        r#"
        let s = "pre:";
        s = s + report;
        s += "/";
        s += report;
        finish(s);
        "#,
    );
    let host = Host::live();
    let mut state = State::new();
    assert_eq!(
        lashlang::execute(&program, &mut state, &host)
            .await
            .expect("execution should finish"),
        ExecutionOutcome::Finished(text("pre:live/live"))
    );
}

/// A turn parked between two concat loops resumes with the accumulator intact:
/// the continuation wire flattens the string, so what comes back is text, not
/// the copy-on-write shape the running VM used.
#[tokio::test(flavor = "current_thread")]
async fn a_concat_accumulator_survives_a_park_and_resume() {
    let program = compile(
        r#"
        let s = "";
        for (let i = 0; i < 3; i++) { s += "x"; }
        const probe = await tools.ping({});
        for (let i = 0; i < 3; i++) { s += "y"; }
        finish(`${s}:${s.length}:${probe}`);
        "#,
    );

    let host = Host::plain();
    let mut state = State::new();
    let mut vm = Vm::from_state(&program, &mut state, &host).expect("vm should build");
    assert!(matches!(
        vm.run_process_until_effect().await,
        Ok(VmRunOutcome::EffectCompleted)
    ));
    let continuation = vm
        .suspend()
        .expect("a turn parked between concat loops must be capturable");
    drop(vm);

    let bytes = serde_json::to_vec(&continuation).expect("continuation should serialize");
    // The accumulator is on the wire as flattened text, once.
    let json = String::from_utf8(bytes.clone()).expect("continuation json");
    assert!(
        json.contains("\"xxx\""),
        "accumulator should serialize as flat text: {json}"
    );

    let restored: VmContinuation =
        serde_json::from_slice(&bytes).expect("continuation should deserialize");

    let host = Host::plain();
    let mut resumed =
        Vm::resume_from(restored, &program, &host).expect("continuation should resume");
    assert_eq!(
        resumed
            .run_process_until_effect()
            .await
            .expect("the resumed turn should finish"),
        VmRunOutcome::Complete(ExecutionOutcome::Finished(text("xxxyyy:6:7")))
    );
}

/// A state snapshot round-trips a built string as its text: the global decoded
/// from canonical bytes appends exactly like the live binding did.
#[tokio::test(flavor = "current_thread")]
async fn a_snapshot_restored_string_appends_the_same_way() {
    let snapshot = Snapshot::new(Record::from_iter([(
        "acc".to_string(),
        Value::String("seed".into()),
    )]));
    let encoded = snapshot.to_canonical_bytes().expect("snapshot encode");
    let decoded = Snapshot::from_canonical_bytes(&encoded).expect("snapshot decode");
    let mut state = State::from_snapshot(decoded);

    let globals = BTreeSet::from(["acc".to_string()]);
    let program = lash_typescript::parse_with_globals(
        r#"
        let s = acc;
        s += ":x";
        s = s + ":y";
        finish(s);
        "#,
        &globals,
    )
    .expect("program should parse");
    let program =
        lashlang::testing::harness::try_compile_program(&program).expect("program should compile");

    let host = Host::plain();
    assert_eq!(
        lashlang::execute(&program, &mut state, &host)
            .await
            .expect("execution should finish"),
        ExecutionOutcome::Finished(text("seed:x:y"))
    );
}

/// The fused append enforces the same ceiling the unfused path does: doubling
/// past the limit fails with the size error rather than silently growing.
#[test]
fn a_concat_that_crosses_the_size_ceiling_still_refuses() {
    let source = r#"
        let s = "x";
        for (let i = 0; i < 23; i++) { s += s; }
        s += "x";
        finish(s);
        "#;
    let error = execute(source).expect_err("crossing the string ceiling must refuse");
    assert!(
        matches!(
            error,
            RuntimeError::MemoryLimitExceeded {
                limit,
                attempted
            } if limit == 8 * 1024 * 1024 && attempted > limit
        ),
        "unexpected error: {error:?}"
    );
}
