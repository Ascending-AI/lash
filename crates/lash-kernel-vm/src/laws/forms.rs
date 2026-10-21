//! The forms, strict operations, the effect boundary, host reads, charges
//! and sessions: one case per rule, in kernel text.

use lash_kernel_doc::{Datum, Float, Handle, Integer};
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;

use super::embedder::{Embedder, Setup, int, record, result, run, text, uncaught, value};
use crate::{CellStart, End, Layout, Machine, Step, Target};

fn list<const N: usize>(items: [Datum; N]) -> Datum {
    Datum::List(items.to_vec())
}

fn tuple<const N: usize>(items: [Datum; N]) -> Datum {
    Datum::Tuple(items.to_vec())
}

/// `K-FORM-010`: a text index reads an existing record field.
#[test]
fn a_record_text_index_reads_the_field() {
    assert_eq!(value(r#"let r = {a: 7} return r["a"]"#), int(7));
}

/// `K-FORM-010`: a missing record field reads as absent.
#[test]
fn a_missing_record_text_index_reads_absent() {
    assert_eq!(value(r#"let r = {} return r["missing"]"#), Datum::Absent);
}

/// `K-FORM-006`, `K-VAL-011`: replacing a field keeps its position.
#[test]
fn a_record_text_index_replaces_without_reordering() {
    assert_eq!(
        value(r#"let r = {a: 1, b: 2} set r["a"] = 3 return r"#),
        record([("a", int(3)), ("b", int(2))]),
    );
}

/// `K-FORM-006`, `K-VAL-011`: a newly indexed field goes last.
#[test]
fn a_record_text_index_adds_the_field_last() {
    assert_eq!(
        value(r#"let r = {a: 1} set r["b"] = 2 return r"#),
        record([("a", int(1)), ("b", int(2))]),
    );
}

/// `K-FORM-007`: a text index removes the named record field.
#[test]
fn a_record_text_index_removes_the_field() {
    assert_eq!(
        value(r#"let r = {a: 1, b: 2} remove r["a"] return r"#),
        record([("b", int(2))]),
    );
}

/// `K-FORM-007`: removing a missing indexed field changes nothing.
#[test]
fn removing_a_missing_record_text_index_changes_nothing() {
    assert_eq!(
        value(r#"let r = {a: 1} remove r["missing"] return r"#),
        record([("a", int(1))]),
    );
}

fn rejects_non_text_record_indexes(operation: impl Fn(&str) -> String) {
    for index in [
        "null",
        "absent",
        "true",
        "1",
        "1.0",
        r#"b"00""#,
        "()",
        "[]",
        "map{}",
        "set{}",
        "{}",
        "fn() {}",
        "error_value",
        "task",
        "&worker",
        "handle",
        "ident.ref(r)",
    ] {
        let operation = operation(index);
        let mut embedder = Embedder::with(
            &format!(
                r#"
definition "function worker() -> Any\nkernel 1\nmetering statements\nbody {{  }}"

definition "function go(handle: Handle(\"table\")) -> Any\nkernel 1\nmetering statements\nbody {{ \n    let r = {{a: 1}}\n    let task = spawn call worker()\n    do join task\n    let error_value = null\n    try {{ if 1 {{}} }} catch e {{ set error_value = e }}\n    let caught = null\n    try {{ {operation_json} }} catch e {{ set caught = e.kind }}\n    return (caught, r)\n }}"

main {{}}"#,
                operation_json = crate::laws::embedder::fragment(&operation)
            ),
            Setup {
                start: CellStart {
                    target: Target::Entry(Name::new("go")),
                    args: vec![Datum::Handle(Handle {
                        kind: "table".into(),
                        id: "one".into(),
                    })],
                    session: None,
                    owner: lash_kernel_doc::heap::PageId([9; 32]),
                    modes: crate::EffectModes::new(),
                    host: Vec::new(),
                },
                ..Setup::default()
            },
        );
        assert_eq!(
            result(embedder.run_to_end(&[])),
            tuple([text("type_error"), record([("a", int(1))])]),
            "{operation}"
        );
    }
}

/// `K-FORM-010`: every non-text record index raises type_error.
#[test]
fn record_index_reads_reject_non_text() {
    rejects_non_text_record_indexes(|index| format!("let ignored = r[{index}]"));
}

/// `K-FORM-006`, `K-EVAL-007`: an invalid index leaves the record unchanged.
#[test]
fn record_index_assignments_reject_non_text() {
    rejects_non_text_record_indexes(|index| format!("set r[{index}] = 9"));
}

/// `K-FORM-007`, `K-EVAL-007`: an invalid index leaves the record unchanged.
#[test]
fn record_index_removals_reject_non_text() {
    rejects_non_text_record_indexes(|index| format!("remove r[{index}]"));
}

/// `K-ERR-003`: an uncaught integer is the thrown value itself.
#[test]
fn an_uncaught_throw_carries_the_integer_unchanged() {
    let (end, _) = run("main { throw 7 }");
    let End::Error(crate::RunError::Uncaught { error: raised, .. }) = end else {
        panic!("main must end in the uncaught value");
    };
    assert_eq!(raised, int(7));
}

/// `K-ERR-003`: collection contents and ordinary errors stay unchanged.
#[test]
fn an_uncaught_throw_preserves_collections_and_errors() {
    let expected = tuple([
        record([("a", list([int(1)]))]),
        Datum::Map(vec![(text("key"), int(2))]),
        Datum::Set(vec![int(3)]),
    ]);
    assert_eq!(
        run(r#"main { throw ({a: [1]}, map{"key": 2}, set{3}) }"#).0,
        End::Error(crate::RunError::Uncaught {
            error: expected,
            site: Some(Site::new(Unit::Main, [0])),
        }),
    );
    let error = Datum::Error(Box::new(lash_kernel_doc::ErrorDatum {
        kind: "boom".into(),
        message: "bang".into(),
        data: text("bang"),
    }));
    assert_eq!(
        run(r#"main { do perform 1("bang") as Any }"#).0,
        End::Error(crate::RunError::Uncaught {
            error,
            site: Some(Site::new(Unit::Main, [0, 0])),
        }),
    );
}

/// FIG-6810: an uncaught raise names where it was raised in document code:
/// the statement that raised, in the declared function that ran it rather
/// than at main's call; a failed `perform`'s own site; and for a library
/// call, its statement, whether the call answers in place or runs its
/// kernel body in a frame. A `catch` ends a raise it takes, and a raise a
/// `finally` held names its `try`.
#[test]
fn an_uncaught_raise_names_the_document_site_that_raised_it() {
    let site = |body: &str, layout: Layout, native_twice: bool| {
        let mut run = Embedder::with(
            body,
            Setup {
                layout,
                native_twice,
                ..Setup::default()
            },
        );
        match run.run_to_end(&[]) {
            End::Error(crate::RunError::Uncaught { site, .. }) => site,
            other => panic!("{body}: {other:?}"),
        }
    };
    let main = |path: &[u32]| Some(Site::new(Unit::Main, path));
    for (body, expected) in [
        ("main { let a = 1 throw 7 }", main(&[1])),
        (
            "definition \"function f() -> Any\\nkernel 1\\nmetering statements\\nbody {  let a = 1 throw 7  }\"\n main { let r = call f() return r }",
            Some(Site::new(Unit::Function(Name::new("f")), [1])),
        ),
        (
            r#"main { let a = 1 let b = perform 1("x") as Any return b }"#,
            main(&[1, 0]),
        ),
        ("main { try { throw 1 } catch e { } throw 2 }", main(&[1])),
        (
            "main { let a = 1 try { throw 1 } finally { let b = 2 } }",
            main(&[1]),
        ),
        (
            r#"main { let a = 1 let b = pair.twice("x") return b }"#,
            main(&[1]),
        ),
        (
            r#"main { let a = 1 let b = invoke pair.twice("x") return b }"#,
            main(&[1]),
        ),
    ] {
        for layout in [Layout(0), Layout(1)] {
            for native_twice in [true, false] {
                let actual = site(body, layout, native_twice);
                let expected = expected.clone().map(|site| match site.unit {
                    Unit::Function(name) => {
                        Site::new(super::embedder::function_unit(name.as_str()), site.path)
                    }
                    _ => site,
                });
                assert_eq!(
                    actual, expected,
                    "{body} in {layout:?}, native {native_twice}"
                );
            }
        }
    }
}

/// `K-EFF-002`: an uncaught value that cannot leave the run reports the
/// typed copy failure, rather than losing its data silently.
#[test]
fn an_uncaught_throw_reports_values_that_cannot_be_copied_out() {
    assert_eq!(uncaught("throw fn() {}"), "not_data");
    assert_eq!(uncaught("let r = {} set r.self = r throw r"), "cycle");
}

/// Each case: the rule it pins, `main`'s body, and what the run returns.
#[test]
fn forms_do_what_their_rules_say() {
    let cases: Vec<(&str, &str, Datum)> = vec![
        (
            "K-FORM-003: an inner block's variable shadows and ends with its block",
            "let x = 1 if true { let x = 2 set x = 3 } return x",
            int(1),
        ),
        (
            "K-FORM-003: a `let` does not see itself",
            "let x = 1 if true { let x = num.add(x, 1) return x }",
            int(2),
        ),
        (
            "K-FORM-005: a field is replaced, or added at the end",
            "let r = {a: 1} set r.b = 2 set r.a = 3 return r",
            record([("a", int(3)), ("b", int(2))]),
        ),
        (
            "K-FORM-006: a list index replaces or appends",
            "let xs = [1] set xs[1] = 2 set xs[0.0] = 0 return xs",
            list([int(0), int(2)]),
        ),
        (
            "K-FORM-006, K-KEY-003: a map keeps an entry's first key and position",
            "let m = map{} set m[1] = \"a\" set m[2] = \"b\" set m[1.0] = \"c\" return m",
            Datum::Map(vec![(int(1), text("c")), (int(2), text("b"))]),
        ),
        (
            "K-FORM-006: a set member is assigned true or false",
            "let s = set{1} set s[2] = true set s[1] = false set s[9] = false return (s, s[2], s[1])",
            tuple([
                Datum::Set(vec![int(2)]),
                Datum::Bool(true),
                Datum::Bool(false),
            ]),
        ),
        (
            "K-FORM-007: remove deletes, and does nothing when there is nothing",
            "let r = {a: 1, b: 2} let xs = [1, 2, 3] let m = map{\"k\": 1} \
             remove r.a remove r.zz remove xs[0] remove m[\"k\"] remove m[\"k\"] return (r, xs, m)",
            tuple([
                record([("b", int(2))]),
                list([int(2), int(3)]),
                Datum::Map(Vec::new()),
            ]),
        ),
        (
            "K-FORM-008: a literal that writes one key twice keeps the first key and the last value",
            "return (map{1: \"a\", 2: \"b\", 1.0: \"c\"}, set{1, 1.0, 2})",
            tuple([
                Datum::Map(vec![(int(1), text("c")), (int(2), text("b"))]),
                Datum::Set(vec![int(1), int(2)]),
            ]),
        ),
        (
            "K-FORM-009: a missing field is absent; an error has kind, message and data",
            "let r = {a: 1} try { let x = r.a.b } catch e { return (r.nope, e.kind, e.data, e.other) }",
            tuple([
                Datum::Absent,
                text("type_error"),
                Datum::Null,
                Datum::Absent,
            ]),
        ),
        (
            "K-FORM-010: index reads of a tuple, a map and a set",
            "let t = (\"a\", \"b\") let m = map{(1, \"x\"): \"v\"} let s = set{2} \
             return (t[1], m[(1.0, \"x\")], s[2], s[3])",
            tuple([text("b"), text("v"), Datum::Bool(true), Datum::Bool(false)]),
        ),
        (
            "K-FORM-014: break ends the innermost loop and continue starts its next iteration",
            "let out = [] for x in [1, 2, 3, 4] { if num.lt(x, 2) { continue } \
             if num.lt(3, x) { break } set out[list.len(out)] = x } return out",
            list([int(2), int(3)]),
        ),
        (
            "K-FORM-015: a function whose body ends without a return returns null",
            "let f = fn() { } let r = apply f() return r",
            Datum::Null,
        ),
        (
            "K-FORM-016: throw raises any value and catch binds it",
            "try { throw (1, \"two\") } catch e { return e }",
            tuple([int(1), text("two")]),
        ),
        (
            "K-FORM-017: finally runs on every departure, and its own departure replaces it",
            "let log = [] \
             let returns = fn() { try { return 1 } finally { set log[list.len(log)] = \"return\" } } \
             let replaces = fn() { while true { try { return 1 } finally { break } } return 2 } \
             let rethrows = fn() { try { throw \"a\" } catch e { throw \"b\" } finally { set log[list.len(log)] = \"throw\" } } \
             let a = apply returns() let b = apply replaces() let c = null \
             try { do apply rethrows() } catch e { set c = e } \
             for x in [1, 2] { try { continue } finally { set log[list.len(log)] = x } } \
             return (a, b, c, log)",
            tuple([
                int(1),
                int(2),
                text("b"),
                list([text("return"), text("throw"), int(1), int(2)]),
            ]),
        ),
        (
            "K-EVAL-007: an assignment that raises changes nothing",
            "let xs = [1] try { set xs[5] = 0 } catch e { } return xs",
            list([int(1)]),
        ),
        (
            "K-ITER-001: a loop gives a tuple's elements, a map's keys and a set's members",
            "let out = [] for x in (1, 2) { set out[list.len(out)] = x } \
             for k in map{\"a\": 1} { set out[list.len(out)] = k } \
             for s in set{true} { set out[list.len(out)] = s } return out",
            list([int(1), int(2), text("a"), Datum::Bool(true)]),
        ),
        (
            "K-ITER-002: a loop over a list reads live by position",
            "let xs = [1, 2] let out = [] for x in xs { \
             if num.lt(x, 3) { set xs[list.len(xs)] = num.add(x, 2) } \
             set out[list.len(out)] = x } return out",
            list([int(1), int(2), int(3), int(4)]),
        ),
        (
            "K-ITER-003, K-KEY-005: a loop over a map skips what was removed and visits what was added",
            "let m = map{\"a\": 1, \"b\": 2, \"c\": 3} let out = [] let first = true \
             for k in m { set out[list.len(out)] = k if first { set first = false \
             remove m[\"b\"] remove m[\"a\"] set m[\"a\"] = 9 set m[\"d\"] = 4 set m[\"c\"] = 0 } } return out",
            list([text("a"), text("c"), text("a"), text("d")]),
        ),
        (
            "K-KEY-002, K-KEY-003, K-KEY-005: map and set churn preserves equivalence, spelling and order",
            "let m = map{0.0: 0, 1: 1, 2: 2, 3: 3} let s = set{0.0, 1, 2, 3} \
             let i = 0 while num.lt(i, 128) { \
             remove m[0] remove m[1.0] remove m[2] remove s[0] remove s[1.0] remove s[2] \
             set m[0] = i set m[1] = i set m[2] = i \
             set s[0] = true set s[1] = true set s[2] = true set i = num.add(i, 1) } \
             set m[0.0] = 9 set s[0.0] = true return (m, s)",
            tuple([
                Datum::Map(vec![
                    (int(3), int(3)),
                    (int(0), int(9)),
                    (int(1), int(127)),
                    (int(2), int(127)),
                ]),
                Datum::Set(vec![int(3), int(0), int(1), int(2)]),
            ]),
        ),
        (
            "K-CLO-001: a closure and its defining scope share the variable",
            "let make = fn() { let n = 0 let bump = fn() { set n = num.add(n, 1) return n } \
             do apply bump() set n = num.add(n, 10) let seen = apply bump() return (n, seen) } \
             let r = apply make() return r",
            tuple([int(12), int(12)]),
        ),
        (
            "K-CLO-002: each iteration of a for has its own binding",
            "let fs = [] for i in [1, 2, 3] { set fs[list.len(fs)] = fn() { return i } } \
             let out = [] for f in fs { let v = apply f() set out[list.len(out)] = v } return out",
            list([int(1), int(2), int(3)]),
        ),
        (
            "K-CLO-003: a closure's return returns from the closure",
            "let out = [] for x in [1, 2] { let f = fn() { return x } let v = apply f() \
             set out[list.len(out)] = v } return out",
            list([int(1), int(2)]),
        ),
        (
            "K-FN-004: a parameter with no argument is absent",
            "let f = fn(a, b) { return (a, b) } let r = apply f(1) return r",
            tuple([int(1), Datum::Absent]),
        ),
        (
            "K-FN-006: a raise in a callee is raised at the calling statement",
            "let f = fn() { throw \"inner\" } let r = \"unset\" \
             try { set r = apply f() } catch e { return (e, r) }",
            tuple([text("inner"), text("unset")]),
        ),
        (
            "K-KEY-002: NaN is one key, also inside a tuple",
            "let m = map{} set m[(nan, 1)] = \"a\" set m[(nan, 1.0)] = \"b\" return (list.len(m), m[(nan, 1)])",
            tuple([int(1), text("b")]),
        ),
        (
            "K-KEY-004: a ref is a key and deref gives the object back",
            "let xs = [1] let r = ident.ref(xs) let m = map{r: \"v\"} let ys = deref(r) \
             set ys[1] = 2 return (xs, m[ident.ref(ys)])",
            tuple([list([int(1), int(2)]), text("v")]),
        ),
        (
            "K-EFF-002: an object reached twice is copied twice",
            "let x = [1] return (x, x)",
            tuple([list([int(1)]), list([int(1)])]),
        ),
        (
            "K-EFF-003, K-EFF-004: a result is a fresh graph and a wait copies nothing",
            "let a = [1] let b = a let r = perform 0(a) as Any set r[0] = 9 set a[1] = 2 return (a, b, r)",
            tuple([
                list([int(1), int(2)]),
                list([int(1), int(2)]),
                list([int(9)]),
            ]),
        ),
        (
            "K-EFF-005, K-EFF-006: a result's number decodes by the stated type",
            "let a = perform 2(\"1.0\") as Int let b = perform 2(\"1e3\") as Int let c = perform 2(\"1\") as Float let d = perform 2(\"2\") as Number let e = perform 2(\"2.0\") as Any let f = perform 2(\"90071992547409930000\") as Int return (a, b, c, d, e, f)",
            tuple([
                int(1),
                int(1000),
                Datum::Float(Float::new(1.0)),
                int(2),
                Datum::Float(Float::new(2.0)),
                Datum::Int(Integer::parse("90071992547409930000").unwrap()),
            ]),
        ),
        (
            "K-EFF-007: a union decodes by its first member that fits",
            "let a = perform 2(\"1.5\") as Union(Int, Text, Float) let v = {status: 200, extra: 1} let b = perform 0(v) as Record{status: Int, body?: Text, ..Any} return (a, b)",
            tuple([
                Datum::Float(Float::new(1.5)),
                record([("status", int(200)), ("extra", int(1))]),
            ]),
        ),
        (
            "K-EFF-009: an effect that fails raises the delivered error",
            "try { do perform 1(\"bang\") as Any } catch e { return (e.kind, e.message, e.data) }",
            tuple([text("boom"), text("bang"), text("bang")]),
        ),
        (
            "K-LIB-002: a call inside an expression runs the function to its end",
            "return [pair.twice(pair.twice(1)), text.concat(\"a\", \"b\")]",
            list([int(4), text("ab")]),
        ),
        (
            "K-LIB-004: a library body calls its function argument, which may wait",
            "let fetch = fn(x) { let y = num.add(x, 1) let r = perform 0(y) as Int return r } let r = invoke each.twice(fetch, 1) return r",
            int(3),
        ),
    ];
    for (rule, body, expected) in cases {
        assert_eq!(value(body), expected, "{rule}");
    }
}

/// Each case: the rule, `main`'s body, and the kind of the error the run
/// ends in (`K-EVAL-006`: every operation is strict, and nothing coerces).
#[test]
fn operations_raise_typed_errors_outside_their_operand_kinds() {
    let cases = [
        (
            "K-FORM-003",
            "let x = 1 if true { let y = 2 } return y",
            "unbound_variable",
        ),
        ("K-FORM-003", "set nowhere = 1", "unbound_variable"),
        ("K-FORM-005", "let xs = [] set xs.a = 1", "type_error"),
        (
            "K-FORM-006",
            "let xs = [1] set xs[3] = 1",
            "index_out_of_range",
        ),
        (
            "K-FORM-006",
            "let xs = [1] set xs[0.5] = 1",
            "index_out_of_range",
        ),
        ("K-FORM-006", "let xs = [1] set xs[\"0\"] = 1", "type_error"),
        ("K-FORM-006", "let s = set{} set s[1] = 1", "type_error"),
        ("K-FORM-006", "let t = (1, 2) set t[0] = 1", "type_error"),
        (
            "K-FORM-007",
            "let xs = [1] remove xs[1]",
            "index_out_of_range",
        ),
        ("K-FORM-007", "let m = map{} remove m.a", "type_error"),
        ("K-FORM-009", "let t = (1, 2) return t.first", "type_error"),
        (
            "K-FORM-010",
            "let xs = [1] return xs[1]",
            "index_out_of_range",
        ),
        (
            "K-FORM-010",
            "let xs = [1] return xs[-1]",
            "index_out_of_range",
        ),
        ("K-FORM-010", "let m = map{} return m[\"k\"]", "key_missing"),
        ("K-FORM-010", "return \"text\"[0]", "type_error"),
        ("K-FORM-013", "if 1 { }", "type_error"),
        ("K-FORM-013", "while null { }", "type_error"),
        ("K-ITER-001", "for x in \"text\" { }", "type_error"),
        ("K-KEY-001", "return map{[1]: 1}", "invalid_key"),
        ("K-KEY-001", "return set{absent}", "invalid_key"),
        (
            "K-KEY-001",
            "let m = map{} return m[(1, {})]",
            "invalid_key",
        ),
        ("K-KEY-004", "return deref(1)", "type_error"),
        ("K-FN-004", "let f = fn(a) { } do apply f(1, 2)", "arity"),
        ("K-FN-005", "let f = 1 do apply f()", "type_error"),
        ("K-FN-007", "return num.add(1, \"1\")", "type_error"),
        ("K-TASK-017", "do cancel 1", "type_error"),
        ("K-EFF-002", "let xs = [] set xs[0] = xs print xs", "cycle"),
        ("K-EFF-002", "let f = fn() { } return [f]", "not_data"),
        ("K-EFF-007", "do perform 2(\"1.5\") as Int", "effect_result"),
        (
            "K-EFF-007",
            "do perform 2(\"1e999\") as Float",
            "effect_result",
        ),
        (
            "K-EFF-007",
            "let v = {} do perform 0(v) as Record{status: Int}",
            "effect_result",
        ),
        (
            "K-EFF-007",
            "let v = {a: 1} do perform 0(v) as Record{}",
            "effect_result",
        ),
        (
            "K-EFF-007",
            "do perform 0(\"c\") as Enum(\"a\", \"b\")",
            "effect_result",
        ),
        (
            "K-VAL-034",
            "let t = () let i = 0 while num.lt(i, 200) { set t = (t,) set i = num.add(i, 1) }",
            "too_deep",
        ),
        (
            "K-VAL-034",
            "let x = [] let i = 0 while num.lt(i, 200) { set x = [x] set i = num.add(i, 1) } print x",
            "too_deep",
        ),
    ];
    for (rule, body, kind) in cases {
        assert_eq!(uncaught(body), kind, "{rule}: {body}");
    }
}

/// `K-EFF-006`: under the `float` policy every bare number is a float.
#[test]
fn the_float_policy_decodes_every_bare_number_as_a_float() {
    let mut embedder = Embedder::with(
        "main { let a = perform 2(\"2\") as Number let b = perform 2(\"3\") as Int return (a, b) }",
        Setup {
            numbers: "float",
            ..Setup::default()
        },
    );
    assert_eq!(
        result(embedder.run_to_end(&[])),
        tuple([Datum::Float(Float::new(2.0)), int(3)])
    );
}

/// `K-EVAL-001`, `K-EVAL-003`, `K-EVAL-005`, `K-HOST-001` to `K-HOST-004`:
/// evaluation is left to right, an assignment evaluates its right-hand
/// side before its place, and host reads are answered inside the
/// statement that asks.
#[test]
fn evaluation_order_is_what_the_host_sees() {
    let handle = |kind: &str| {
        Datum::Handle(Handle {
            kind: kind.to_string(),
            id: "t1".to_string(),
        })
    };
    let mut embedder = Embedder::with(
        r#"
definition "function go(h: Handle(\"table\"), broken: Handle(\"broken\")) -> Any\nkernel 1\nmetering statements\nbody { \n  let box = [null]\n  set box[read(h, \"index\")] = read(h, \"value\")\n  let pair = (read(h, \"left\"), read(h, \"right\"))\n  let m = map{read(h, \"key 1\"): read(h, \"value 1\"), read(h, \"key 2\"): read(h, \"value 2\")}\n  let answer = read(h, {path: [\"a\", 0]})\n  let failure = null\n  try { let x = read(broken, \"gone\") } catch e { set failure = e.kind }\n  let wrong = null\n  try { let x = read(\"no handle\", 1) } catch e { set wrong = e.kind }\n  return (box, pair, answer, failure, wrong)\n }"

main { }"#,
        Setup {
            start: CellStart {
                target: Target::Entry(Name::new("go")),
                args: vec![handle("table"), handle("broken")],
                session: None,
                owner: lash_kernel_doc::heap::PageId([9; 32]),
                modes: crate::EffectModes::new(),
                host: Vec::new(),
            },
            ..Setup::default()
        },
    );
    let end = embedder.run_to_end(&[]);
    assert_eq!(
        embedder.world.reads[..8],
        [
            "value", "index", "left", "right", "key 1", "value 1", "key 2", "value 2"
        ]
        .map(text)
    );
    assert_eq!(
        result(end),
        tuple([
            list([text("value")]),
            tuple([text("left"), text("right")]),
            record([("path", list([text("a"), int(0)]))]),
            text("host"),
            text("type_error"),
        ])
    );
}

/// `K-CHG-002`, `K-CHG-003`, `K-CHG-005`: the cost table and a library
/// function's charge formula, to the unit.
#[test]
fn charges_follow_the_cost_table() {
    let charged = |body: &str| {
        let mut embedder = Embedder::new(&format!("main {{ {body} }}"));
        embedder.run_to_end(&[]);
        embedder.machine.meters().charged.expect("exact charge")
    };
    // An empty `main` executes nothing.
    assert_eq!(charged(""), 0);
    // `let`: 1 statement, 1 list node, 2 literal nodes, 2 members written.
    assert_eq!(charged("let x = [1, 2]"), 6);
    // `return`: 1 statement, 1 variable node, and the result copied out at
    // its deep size: the list (1 + 2) and two one-word integers (2 each).
    assert_eq!(charged("let x = [1, 2] return x"), 6 + 2 + 7);
    // A loop: the statement, the iterable's nodes, and one test per
    // iteration and one that ends it.
    assert_eq!(charged("for x in (1, 2) { }"), 1 + (1 + 2 + 2) + 3);
    // Map/set literals charge every written entry, including equivalent
    // keys; copying out and iteration charge only the surviving entries.
    assert_eq!(charged("let m = map{1: 2, 1.0: 3} return m"), 8 + 2 + 7);
    assert_eq!(charged("let s = set{1, 1.0} for k in s { }"), 6 + 2 + 2);
    // A library call: its node, its arguments' nodes, and its formula,
    // `size(result)`: 1 + 4 bytes.
    assert_eq!(charged("let t = text.concat(\"ab\", \"cd\")"), 1 + 3 + 5);
    // An action: 1 per atom; the callee's body is charged as it runs.
    assert_eq!(
        charged("let f = fn(a, b) { return a } let r = apply f(1, 2)"),
        (1 + 1) + (1 + 2) + (1 + 1)
    );
}

/// K-SES-001: finishing without a park publishes the dirty binding tree.
#[test]
fn a_cell_carries_unsealed_bindings_at_its_end() {
    let mut embedder = Embedder::new("main { let x = 7 }");
    let Step::Ended(End::Finished(finished)) = embedder.run() else {
        panic!("cell finishes")
    };
    let carry = finished.carry.unwrap();
    assert_eq!(
        lash_kernel_state::kernel_internals::session_bindings(&carry).len(),
        1
    );
    assert!(
        lash_kernel_state::kernel_internals::session_bindings(&carry)
            .page()
            .is_some(),
        "a nonempty carry must name its sealed tree"
    );
    let mut heap = lash_kernel_state::kernel_internals::PagedHeap::open_header(
        lash_kernel_state::kernel_internals::session_heap(&carry),
        Box::new(embedder.source.clone()),
        lash_kernel_state::paged::EditEpoch(2),
    )
    .unwrap();
    lash_kernel_state::paged::HeapLifecycle::adopt(&mut heap, &carry).unwrap();
    let binding = heap
        .session_root()
        .get(
            &lash_kernel_state::kernel_internals::metadata::metadata_key(&Name::new("x")),
            heap.pages(),
        )
        .unwrap()
        .unwrap();
    assert!(matches!(
        binding.value,
        lash_kernel_state::paged::BoundRecord::Value(lash_kernel_doc::heap::PageValue::Int(_))
    ));
}
