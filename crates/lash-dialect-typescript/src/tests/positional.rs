//! TS-CALL-SLOTS: a stable direct call and its exported packed entry share
//! one body, preserving argument/default evaluation and parked state.
use super::machine;

/// FIG-7192: rest bindings are lists when dispatching their array methods.
#[test]
fn rest_parameter_join_dispatches_as_a_list() {
    use lash_kernel_doc::Datum;

    let source = r#"
function f(a = 1, c) { return a + c; }
const rest = (...xs) => xs.join(",");
await control.finish({ positional: f(undefined, 2), rest: rest("a", "b") });
"#;
    let lowered = super::lower_against(source, &[], &machine::effects()).unwrap();
    lash_kernel_check::derive(&lowered.document, super::library()).unwrap();
    assert_eq!(
        machine::end(source),
        machine::Ended::Finished(Datum::Record(vec![
            ("positional".into(), Datum::Float(3.0.into())),
            ("rest".into(), Datum::Text("a,b".into())),
        ])),
    );
}

#[test]
fn rest_parameter_map_and_length_dispatch_as_a_list() {
    let source = r#"
const rest = (...xs) => xs.map(x => x + "!").join(",") + ":" + xs.length;
await control.finish(rest("a", "b"));
"#;
    let lowered = super::lower_against(source, &[], &machine::effects()).unwrap();
    lash_kernel_check::derive(&lowered.document, super::library()).unwrap();
    assert_eq!(
        machine::end(source),
        machine::Ended::Finished(lash_kernel_doc::Datum::Text("a!,b!:2".into())),
    );
}

#[test]
fn positional_and_packed_entries_agree_on_missing_extra_and_default_arguments() {
    let recorded = machine::run(
        r#"
      function f(a = 7, b = 9) { return a + ":" + b; }
      const g = f;
      console.log(f(), g(), f(undefined, 3), g(undefined, 3), f(1, 2, 99), g(1, 2, 99));
      function captured(a = 1, read = () => a) { a = 2; return read(); }
      const h = captured;
      console.log(captured(), h());
      console.log(f.length, f.name);
    "#,
        &[],
    );
    assert_eq!(recorded.end, "ok");
    assert_eq!(recorded.lines(), ["7:9 7:9 7:3 7:3 1:2 1:2", "2 2", "0 f"]);
}

#[test]
fn extra_arguments_evaluate_in_order_after_retained_arguments_are_copied() {
    let recorded = machine::run(
        r#"
      let n = 0; let log = "";
      function take(a, b) { return a + ":" + b; }
      function bump(tag) { log += tag; return ++n; }
      console.log(take(n, bump("B"), bump("X")), log, n);
      const g = take; n = 0; log = "";
      console.log(g(n, bump("B"), bump("X")), log, n);
    "#,
        &[],
    );
    assert_eq!(recorded.end, "ok");
    assert_eq!(recorded.lines(), ["0:1 BX 2", "0:1 BX 2"]);
}

#[test]
fn arguments_rest_and_reassigned_functions_keep_their_packed_entry() {
    let recorded = machine::run(
        r#"
      function args(a) { return arguments.length + ":" + arguments[1]; }
      function rest(a, ...tail) { return tail.join(":"); }
      function changed(a) { return a; }
      changed = (a, b) => b;
      console.log(args(1, 2), rest(1, 2, 3), changed(1, 8));
      function arrow(a) { const read = () => arguments[1]; return read(); }
      console.log(arrow(1, 9));
      function outer(a) { function same(a) { return a + 1; } return same(a); }
      console.log(outer(4));
    "#,
        &[],
    );
    assert_eq!(recorded.end, "ok");
    assert_eq!(recorded.lines(), ["2:2 2:3 8", "9", "5"]);
}

#[test]
fn array_callback_slots_agree_with_packed_calls() {
    for (callback, packed) in [
        ("(a, b) => a - b", false),
        ("function(a, b) { return a - b; }", false),
        ("(a, b = 4, c = 9, d = 7) => a - b + (d - 7)", false),
        ("function(a, ...rest) { return a - rest[0]; }", true),
        (
            "function(a, b) { return arguments[0] - arguments[1]; }",
            true,
        ),
    ] {
        let direct = format!("console.log([3, 1, 2].sort({callback}).join(','));");
        let generic = format!(
            "function call(xs, f) {{ return xs.sort(f); }} console.log(call([3, 1, 2], {callback}).join(','));"
        );
        let text = super::main_text(&direct);
        let lowered = super::lower(&direct).unwrap();
        let padded = super::linked_inspection::helper_reference(&lowered.document, "ts.pad")
            .is_some_and(|_| text.contains("invoke ts.pad("));
        assert_eq!(padded, packed, "{callback}\n{text}");
        let direct = machine::run(&direct, &[]);
        let generic = machine::run(&generic, &[]);
        assert_eq!(direct.end, "ok");
        assert_eq!(direct.end, generic.end);
        assert_eq!(direct.lines(), ["1,2,3"]);
        assert_eq!(direct.lines(), generic.lines());
    }
    for method in ["map", "filter"] {
        for (callback, packed) in [
            (
                "(value, index, receiver, missing = 7) => value + index + receiver.length + missing",
                false,
            ),
            (
                "function(value, index, receiver) { return value + index + receiver.length + this.offset; }",
                false,
            ),
            (
                "function(value, ...rest) { return value + rest[0] + rest[1].length; }",
                true,
            ),
            (
                "function(value) { return arguments.length + value + arguments[1] + arguments[2].length; }",
                true,
            ),
        ] {
            let direct =
                format!("console.log([1, , 3].{method}({callback}, {{offset: 5}}).join(','));");
            let generic = format!(
                "function call(xs, f, context) {{ return xs.{method}(f, context); }} console.log(call([1, , 3], {callback}, {{offset: 5}}).join(','));"
            );
            let text = super::main_text(&direct);
            let lowered = super::lower(&direct).unwrap();
            let padded = super::linked_inspection::helper_reference(&lowered.document, "ts.pad")
                .is_some_and(|_| text.contains("invoke ts.pad("));
            assert_eq!(padded, packed, "{callback}\n{text}");
            let direct = machine::run(&direct, &[]);
            let generic = machine::run(&generic, &[]);
            assert_eq!(direct.end, "ok");
            assert_eq!(direct.end, generic.end);
            assert_eq!(direct.lines(), generic.lines());
        }
    }
    for (source, expected) in [
        (
            "function compare(a, b) { return a - b; } const alias = compare; console.log([3, 1, 2].sort(compare).join(','), [3, 1, 2].sort(alias).join(','));",
            "1,2,3 1,2,3",
        ),
        (
            "function first(value) { return value; } const alias = first; console.log([1, 2].map(first).join(','), [1, 2].map(alias).join(','));",
            "1,2 1,2",
        ),
        (
            "console.log([3, 1, 2].map((value, index, receiver, {offset} = {offset: 7}) => value + index + receiver.length + offset).join(','));",
            "13,12,14",
        ),
        (
            "function run() { return [1, 2].map(value => value + this.offset).join(','); } console.log(run.call({offset: 5}));",
            "6,7",
        ),
        (
            "const xs = [1, 2]; function context() { xs.push(3); return {offset: 5}; } console.log(xs.map(function(value) { return value + this.offset; }, context()).join(','));",
            "6,7,8",
        ),
    ] {
        let recorded = machine::run(source, &[]);
        assert_eq!(recorded.end, "ok", "{source}");
        assert_eq!(recorded.lines(), [expected], "{source}");
    }
}
