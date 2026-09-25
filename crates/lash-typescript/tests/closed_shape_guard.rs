//! The closed-shape field guard and the triggers that open a shape (FIG-3626,
//! ADR 0062 register entry 22).
//!
//! The linker refuses a read or write of a field an object literal's closed
//! shape lacks, as `tsc` does. The guard is sound only while nothing can have
//! given the object a field the linker cannot see, so each way an object can
//! get one opens its shape, and an open object answers as JavaScript does.
//!
//! One law per open trigger — a spread, a computed key, a computed-key write
//! (also through another binding of the object), and an escape by every route
//! a reference leaves a place by. The object a
//! trigger opens is linked as a cell is admitted and run, and it reads a field
//! its literal never listed as `undefined`, reads the field the trigger gave
//! it, takes a new field by plain assignment and keeps the fields it was built
//! with. With no trigger, the same literal refuses the same reads and writes,
//! naming the field and the literal's fields. The differential table checks
//! the same shapes against Node.

use lashlang::{
    AbilityOp, AbilityResult, ExecutionHost, ExecutionHostError, ExecutionOutcome, State, Value,
};

struct Host;

impl ExecutionHost for Host {
    async fn perform(&self, op: AbilityOp) -> Result<AbilityResult, ExecutionHostError> {
        match op {
            AbilityOp::Finish(value) => Ok(AbilityResult::Value(value)),
            AbilityOp::Print(_) => Ok(AbilityResult::Value(Value::Null)),
            _ => Err(ExecutionHostError::new("unexpected ability in a shape law")),
        }
    }
}

/// Each builds `o` from the fields `a: 1, b: 2`, opens it, and leaves it
/// holding `z: 3`, a field its literal does not list.
const TRIGGERS: &[(&str, &str)] = &[
    (
        "a spread",
        "const base = { a: 1, z: 3 }; const o = { ...base, b: 2 };",
    ),
    (
        "a computed key",
        "const key = 'z'; const o = { a: 1, b: 2, [key]: 3 };",
    ),
    (
        "a computed-key write",
        "const o = { a: 1, b: 2 }; const key = 'z'; o[key] = 3;",
    ),
    (
        "an escape to a function",
        "const o = { a: 1, b: 2 }; const grow = (x: any) => { x.z = 3; }; grow(o);",
    ),
    (
        "an escape to a builtin",
        "const o = { a: 1, b: 2 }; Object.assign(o, { z: 3 });",
    ),
    (
        "a computed-key write through another binding",
        "const o = { a: 1, b: 2 }; const alias = o; const key = 'z'; alias[key] = 3;",
    ),
    (
        "an escape into a container",
        "const o = { a: 1, b: 2 }; const box = [o]; box[0].z = 3;",
    ),
    (
        "an escape by return",
        "const o = { a: 1, b: 2 }; const same = () => o; const back = same(); back.z = 3;",
    ),
    (
        "an escape through globalThis",
        "const o = { a: 1, b: 2 }; const slot = globalThis.o; slot.z = 3;",
    ),
];

/// What a cell does once `o` is built, and what JavaScript answers.
const USES: &[(&str, &str)] = &[
    ("finish(String(o.missing));", "undefined"),
    ("finish(String(o.z));", "3"),
    ("o.fresh = 4; finish(String(o.fresh));", "4"),
    ("finish(`${o.a}|${o.b}`);", "1|2"),
];

/// Links `source` as a cell is admitted, then runs it.
fn run(source: &str) -> Result<String, String> {
    lash_typescript::link(source, &lashlang::testing::harness::test_environment())
        .map_err(|diagnostic| format!("{}: {diagnostic}", diagnostic.code.as_str()))?;
    let program = lash_typescript::testing::compile(source)
        .map_err(|diagnostic| format!("compile: {diagnostic}"))?;
    match futures::executor::block_on(lashlang::execute(&program, &mut State::new(), &Host)) {
        Ok(ExecutionOutcome::Finished(Value::String(answer))) => Ok(answer.to_string()),
        other => Err(format!("ran to {other:?}")),
    }
}

/// The link refusal of `source`, rendered.
fn refusal(source: &str) -> String {
    match lash_typescript::link(source, &lashlang::testing::harness::test_environment()) {
        Ok(_) => format!("`{source}` linked"),
        Err(diagnostic) => format!("{}: {}", diagnostic.code.as_str(), diagnostic.message),
    }
}

#[test]
fn every_open_trigger_lets_the_object_answer_as_javascript_does() {
    let mut failures = Vec::new();
    for (trigger, setup) in TRIGGERS {
        for (use_, expected) in USES {
            let source = format!("{setup} {use_}");
            match run(&source) {
                Ok(answer) if answer == *expected => {}
                outcome => failures.push(format!(
                    "{trigger}: `{source}` answered {outcome:?}, JavaScript answers {expected:?}"
                )),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_closed_literal_refuses_a_field_it_lacks_by_name() {
    let known = "its fields are `a`, `b`";
    let cases = [
        (
            "const o = { a: 1, b: 2 }; finish(String(o.missing));",
            format!("TS_LINK_ERROR: object has no field `missing`; {known}"),
        ),
        (
            "const o = { a: 1, b: 2 }; o.fresh = 4; finish(String(o.fresh));",
            format!("TS_LINK_ERROR: object has no field `fresh`; {known}"),
        ),
        // Another binding of the literal is guarded as the literal is.
        (
            "const o = { a: 1, b: 2 }; const alias = o; alias.fresh = 4; finish(1);",
            format!("TS_LINK_ERROR: object has no field `fresh`; {known}"),
        ),
        (
            "const o = {}; finish(String(o.missing));",
            "TS_LINK_ERROR: object has no field `missing`; it has no fields".to_string(),
        ),
        // A field of a closed nested literal names that literal's fields.
        (
            "const o = { a: 1, inner: { b: 2 } }; finish(String(o.inner.c));",
            "TS_LINK_ERROR: object has no field `c`; its fields are `b`".to_string(),
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(refusal(source), expected, "{source}");
    }
    assert_eq!(
        run("const o = { a: 1, b: 2 }; finish(`${o.a}|${o.b}`);").as_deref(),
        Ok("1|2")
    );
}

/// A use that reads the object without keeping its reference cannot give it
/// a field, so the shape stays closed through it; so does another binding of
/// it, which the guard follows, as `tsc` does.
#[test]
fn a_read_that_keeps_no_reference_leaves_the_shape_closed() {
    for use_ in [
        "const alias = o; alias.a = 5;",
        "const { a } = o;",
        "console.log(o);",
        "const keys = Object.keys(o);",
        "const listed = Array.isArray(o);",
        "const kind = typeof o;",
        "const same = o === o;",
        "const text = `${o.a}`;",
        "for (const key in o) { console.log(key); }",
    ] {
        let source = format!("const o = {{ a: 1, b: 2 }}; {use_} finish(String(o.missing));");
        assert_eq!(
            refusal(&source),
            "TS_LINK_ERROR: object has no field `missing`; its fields are `a`, `b`",
            "{source}"
        );
    }
}

/// The ruling is flow-insensitive: a trigger anywhere in the cell opens the
/// shape everywhere in it, before the trigger and on a loop's first pass too.
#[test]
fn a_trigger_opens_the_shape_before_it_as_well() {
    let cases = [
        (
            "const o = { a: 1 }; const before = String(o.z); o['z'] = 3; finish(`${before}|${o.z}`);",
            "undefined|3",
        ),
        (
            "const o = { a: 1 }; const seen = []; for (const key of ['z', 'y']) { seen.push(String(o.z)); o[key] = 3; } finish(seen.join(','));",
            "undefined,3",
        ),
        (
            "const o = { a: 1 }; const before = String(o.z); const grow = (x: any) => { x.z = 3; }; grow(o); finish(`${before}|${o.z}`);",
            "undefined|3",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(run(source).as_deref(), Ok(expected), "{source}");
    }
}

/// An escape through a place opens that place's object and what it reaches,
/// not the object holding it: the holder keeps its closed shape.
#[test]
fn an_escaping_field_opens_its_own_object_not_its_holder() {
    let source = "const o = { a: 1, inner: { b: 2 } }; const grow = (x: any) => { x.z = 3; }; grow(o.inner);";
    assert_eq!(
        run(&format!("{source} finish(String(o.inner.z));")).as_deref(),
        Ok("3")
    );
    assert_eq!(
        refusal(&format!("{source} finish(String(o.typo));")),
        "TS_LINK_ERROR: object has no field `typo`; its fields are `a`, `inner`"
    );
}

/// A closed literal still inherits `Object.prototype`, and `tsc --strict`
/// types `point.toString` from it, so the guard admits a read of an advertised
/// inherited method and the read answers the built-in function (FIG-3701).
/// The guard refused these before: `object has no field \`toString\``.
#[test]
fn a_closed_literal_reads_its_inherited_methods() {
    for (source, expected) in [
        (
            "const point = { x: 1 }; finish(typeof point.toString);",
            "function",
        ),
        (
            "const point = { x: 1 }; finish(String(point.hasOwnProperty === ({}).hasOwnProperty));",
            "true",
        ),
        (
            "const point = { x: 1 }; finish(point.valueOf.name);",
            "valueOf",
        ),
    ] {
        assert_eq!(run(source), Ok(expected.to_string()), "{source}");
    }
    // A name no prototype carries is still the literal's missing field.
    assert!(
        refusal("const point = { x: 1 }; finish(point.includes);")
            .contains("object has no field `includes`"),
    );
}
