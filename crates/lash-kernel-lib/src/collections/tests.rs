//! Laws of K-COL and K-ITER, using the public VM as an in-process embedder.
use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionRegistry, Handle, Integer, Timestamp, parse_document,
};
use lash_kernel_vm::{
    Bounds, End, Host, KernelMachine, Machine, Outcome, Program, Request, RunError, Step,
    register_machine_functions,
};

use super::register_collections;

fn registry() -> FunctionRegistry {
    let mut registry = FunctionRegistry::new();
    register_machine_functions(&mut registry).unwrap();
    crate::register_numbers(&mut registry).unwrap();
    register_collections(&mut registry).unwrap();
    registry
}
#[derive(Default)]
struct World {
    prints: Vec<Datum>,
}
impl Host for World {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        Ok(Datum::Null)
    }
    fn print(&mut self, value: &Datum) {
        self.prints.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
const BOUNDS: Bounds = Bounds {
    memory_limit: lash_kernel_vm::MemoryLimit::Recount,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    call_depth: 100,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
    durability: lash_kernel_vm::Durability::Durable,
};
fn program(source: &str) -> Program {
    let registry = Arc::new(registry());
    let mut text = String::from("kernel 1\nnumbers by_spelling\neffect echo(x: Any) -> Any\n");
    for (id, function) in registry.iter() {
        text.push_str(&format!("use {} = @{id}\n", function.definition().name));
    }
    text.push_str(source);
    Program {
        release: lash_kernel_doc::heap::PageId([8; 32]),
        document: Arc::new(parse_document(&text).unwrap()),
        library: lash_kernel_vm::PreparedLibrary::new(registry),
    }
}
fn machine(source: &str) -> KernelMachine {
    crate::testsupport::start(
        program(source),
        BOUNDS,
        lash_kernel_vm::kernel_internals::Layout::default(),
    )
    .unwrap()
}
fn run(source: &str) -> End {
    let mut machine = machine(source);
    loop {
        match machine.run(&mut World::default(), u64::MAX).unwrap() {
            Step::Ended(end) => return end,
            Step::Slice => {}
            Step::Parked(park) => {
                for request in park.requests {
                    match request {
                        Request::Effect(effect) => {
                            machine
                                .deliver(effect.wait, Outcome::Completed(effect.args[0].clone()))
                                .unwrap();
                        }
                        Request::Sleep(sleep) => {
                            machine.deliver(sleep.wait, Outcome::Elapsed).unwrap();
                        }
                    }
                }
            }
        }
    }
}
fn result(source: &str) -> Datum {
    match run(source) {
        End::Finished(done) => done.result,
        end => panic!("{end:?}"),
    }
}
fn body(source: &str) -> Datum {
    result(&format!("main {{ {source} }}"))
}
fn int(n: i64) -> Datum {
    Datum::Int(Integer::from(n))
}
fn text(s: &str) -> Datum {
    Datum::Text(s.to_owned())
}
fn list(items: &[i64]) -> Datum {
    Datum::List(items.iter().copied().map(int).collect())
}
fn tuple(items: Vec<Datum>) -> Datum {
    Datum::Tuple(items)
}
fn error(source: &str) -> String {
    match run(&format!("main {{ {source} }}")) {
        End::Error(RunError::Uncaught(Datum::Error(error))) => error.kind,
        other => panic!("expected uncaught error: {other:?}"),
    }
}

#[test]
fn k_col_001_lengths_are_strict_and_integral() {
    assert_eq!(
        body(
            "return (list.len([1, 2]), tuple.len((1,)), map.len(map{1: 2}), set.len(set{1}), record.len({a: 1}))"
        ),
        tuple(vec![int(2), int(1), int(1), int(1), int(1)])
    );
    for source in [
        "return list.len(map{})",
        "return tuple.len([])",
        "return map.len(set{})",
        "return set.len({})",
        "return record.len([])",
    ] {
        assert_eq!(error(source), "type_error");
    }
}
#[test]
fn k_col_002_get_is_strict_and_at_counts_from_the_end() {
    assert_eq!(
        body("return (list.get([3, 4], 1.0), list.at([3, 4], -1), tuple.at((3, 4), -2))"),
        tuple(vec![int(4), int(4), int(3)])
    );
    for index in ["-1", "0.5", "nan", "inf", "2", "99999999999999999999999999"] {
        assert_eq!(
            error(&format!("return list.get([1, 2], {index})")),
            "index_out_of_range"
        );
    }
    assert_eq!(error("return list.get([1], true)"), "type_error");
    assert_eq!(error("return list.get((1,), 0)"), "type_error");
    for index in [
        "-170141183460469231731687303715884105728",
        "9223372036854775808.0",
        "1e100",
    ] {
        assert_eq!(
            error(&format!("return tuple.at((1,), {index})")),
            "index_out_of_range"
        );
    }
    assert_eq!(body("return tuple.get((3,), -0.0)"), int(3));
}
#[test]
fn k_col_003_slices_translate_clamp_and_never_coerce() {
    assert_eq!(
        body(
            "return (list.slice([1, 2, 3], -2), list.slice([1, 2], -99, 99), list.slice([1, 2], 2, 0), tuple.slice((1, 2, 3), 0, -1))"
        ),
        tuple(vec![
            list(&[2, 3]),
            list(&[1, 2]),
            list(&[]),
            tuple(vec![int(1), int(2)])
        ])
    );
    assert_eq!(error("return list.slice([1], 0.5)"), "index_out_of_range");
    assert_eq!(error("return list.slice([1], null)"), "type_error");
    assert_eq!(
        body("return list.slice([1, 2], -999999999999999999999999999999999999999999, 1e100)"),
        list(&[1, 2])
    );
    assert_eq!(
        body(
            "let inner = [1] let xs = [inner, 2] let out = list.slice(xs, 0, 1) set out[0][0] = 3 set out[1] = 4 return (xs, out)"
        ),
        tuple(vec![
            Datum::List(vec![list(&[3]), int(2)]),
            Datum::List(vec![list(&[3]), int(4)])
        ])
    );
}
#[test]
fn k_col_004_concat_preserves_kind_and_order() {
    assert_eq!(
        body("return (list.concat([1], [2, 3]), tuple.concat((1,), (2,)))"),
        tuple(vec![list(&[1, 2, 3]), tuple(vec![int(1), int(2)])])
    );
    assert_eq!(error("return list.concat([], ())"), "type_error");
}
#[test]
fn k_col_005_membership_and_missing_values_are_distinct() {
    assert_eq!(
        body(
            "let m = map{nan: absent} let r = {x: absent} return (list.index_of([2, 1, 2], 2), list.index_of([], 0), list.contains([nan], nan), map.contains(m, nan), record.contains(r, \"x\"), record.contains(r, \"y\"), record.get(r, \"y\"))"
        ),
        tuple(vec![
            int(0),
            int(-1),
            Datum::Bool(false),
            Datum::Bool(true),
            Datum::Bool(true),
            Datum::Bool(false),
            Datum::Absent
        ])
    );
    assert_eq!(error("return map.get(map{}, 0)"), "key_missing");
    assert_eq!(error("return set.contains(set{}, [])"), "invalid_key");
    assert_eq!(
        body(
            "return (list.contains([[1]], [1.0]), tuple.index_of((1, 2, 1), 1.0), tuple.contains((), 1))"
        ),
        tuple(vec![Datum::Bool(true), int(0), Datum::Bool(false)])
    );
}
#[test]
fn k_col_006_views_follow_insertion_order() {
    assert_eq!(
        body(
            "return (list.keys([4, 5]), map.entries(map{2: \"b\", 1: \"a\"}), set.entries(set{2, 1}), record.keys({b: 2, a: 1}), tuple.values((4, 5)))"
        ),
        tuple(vec![
            list(&[0, 1]),
            Datum::List(vec![
                tuple(vec![int(2), text("b")]),
                tuple(vec![int(1), text("a")])
            ]),
            Datum::List(vec![
                tuple(vec![int(2), int(2)]),
                tuple(vec![int(1), int(1)])
            ]),
            Datum::List(vec![text("b"), text("a")]),
            list(&[4, 5])
        ])
    );
    for (kind, source, keys, values) in [
        ("list", "[4, 5]", list(&[0, 1]), list(&[4, 5])),
        ("tuple", "(4, 5)", list(&[0, 1]), list(&[4, 5])),
        ("map", "map{2: 4, 1: 3}", list(&[2, 1]), list(&[4, 3])),
        ("set", "set{2, 1}", list(&[2, 1]), list(&[2, 1])),
        (
            "record",
            "{b: 2, a: 1}",
            Datum::List(vec![text("b"), text("a")]),
            list(&[2, 1]),
        ),
    ] {
        let (Datum::List(k), Datum::List(v)) = (&keys, &values) else {
            unreachable!()
        };
        let entries = Datum::List(
            k.iter()
                .zip(v)
                .map(|(k, v)| tuple(vec![k.clone(), v.clone()]))
                .collect(),
        );
        assert_eq!(
            body(&format!(
                "let xs = {source} return ({kind}.keys(xs), {kind}.values(xs), {kind}.entries(xs))"
            )),
            tuple(vec![keys, values, entries]),
            "{kind}"
        );
    }
}
#[test]
fn k_col_007_copies_preserve_sharing_and_refuse_cycles() {
    assert_eq!(
        body(
            "let inner = [1] let xs = [inner, inner] let shallow = list.copy(xs) let deep = list.copy_deep(xs) set inner[0] = 2 set deep[0][0] = 3 return (xs, shallow, deep)"
        ),
        tuple(vec![
            Datum::List(vec![list(&[2]), list(&[2])]),
            Datum::List(vec![list(&[2]), list(&[2])]),
            Datum::List(vec![list(&[3]), list(&[3])])
        ])
    );
    assert_eq!(
        error("let xs = [] set xs[0] = xs return list.copy_deep(xs)"),
        "cyclic_value"
    );
    assert_eq!(
        error("let r = {} set r.self = r return record.copy_deep(r)"),
        "cyclic_value"
    );
}
/// `K-COL-016`: `holds_none` reads what a deep copy reads, at its depths
/// and each collection once: a value it passes copies without raising, and
/// a cycle or one level past the copy's 128 fails it, also where the order
/// a shared collection is first reached decides its depth. A refused kind
/// anywhere a copy reads fails it. Its charge counts each collection once,
/// so 2^60 paths to one list cost a few hundred units.
#[test]
fn k_col_016_holds_none_reads_what_a_deep_copy_reads() {
    let verdict = |build: &str, kinds: &str| {
        let source = format!(
            "fn nest(n) {{ let d = \"leaf\" let i = 0 while num.lt(i, n) {{ set d = [d] set i = num.add(i, 1) }} return d }} \
             main {{ let x = absent {build} let held = [x] let passed = collection.holds_none(held, {kinds}) \
             let copied = \"copied\" try {{ let c = list.copy_deep(held) }} catch e {{ set copied = e.kind }} \
             return (passed, copied) }}"
        );
        match result(&source) {
            Datum::Tuple(pair) => match &pair[..] {
                [Datum::Bool(passed), Datum::Text(copied)] => (*passed, copied.clone()),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    };
    let plain = "set{\"absent\", \"tuple\"}";
    for (build, kinds, expected) in [
        (
            "set x = {a: [1, 2.5, \"t\", null, true], b: map{\"k\": [1]}}",
            plain,
            (true, "copied"),
        ),
        ("set x = [1, absent]", plain, (false, "copied")),
        ("set x = {a: absent}", "set{}", (true, "copied")),
        (
            "set x = [(1, absent)]",
            "set{\"absent\"}",
            (false, "copied"),
        ),
        (
            "set x = [error.new(\"k\", \"m\", [()])]",
            "set{\"tuple\"}",
            (false, "copied"),
        ),
        (
            "set x = map{(1, ()): 1}",
            "set{\"tuple\"}",
            (false, "copied"),
        ),
        ("set x = set{(), 1}", "set{\"tuple\"}", (false, "copied")),
        ("set x = {f: fn() {}}", plain, (true, "copied")),
        (
            "set x = {f: fn() {}}",
            "set{\"closure\"}",
            (false, "copied"),
        ),
        ("set x = [] set x[0] = x", plain, (false, "cyclic_value")),
        (
            "let r = {} set r.self = [r] set x = r",
            "set{}",
            (false, "cyclic_value"),
        ),
        ("set x = call nest(127)", plain, (true, "copied")),
        ("set x = call nest(128)", plain, (false, "too_deep")),
        (
            "let d = call nest(126) set x = {a: d, b: [[d]]}",
            plain,
            (true, "copied"),
        ),
        (
            "let d = call nest(126) set x = {b: [[d]], a: d}",
            plain,
            (false, "too_deep"),
        ),
    ] {
        let (passed, copied) = verdict(build, kinds);
        assert_eq!((passed, copied.as_str()), expected, "{build} {kinds}");
    }
    assert_eq!(
        error("return collection.holds_none(1, set{\"number\"})"),
        "type_error"
    );
    assert_eq!(
        error("return collection.holds_none(1, [\"tuple\"])"),
        "type_error"
    );
    let charged = |answer: &str| {
        let shared = format!(
            "main {{ let d = [1] let i = 0 while num.lt(i, 60) {{ set d = [d, d] set i = num.add(i, 1) }} \
             let kinds = set{{\"tuple\"}} return {answer} }}"
        );
        let mut machine = machine(&shared);
        let Step::Ended(End::Finished(done)) =
            machine.run(&mut World::default(), u64::MAX).unwrap()
        else {
            panic!("{shared}")
        };
        assert_eq!(done.result, Datum::Bool(true));
        machine.meters().charged.expect("exact charge")
    };
    // 1 + size(kinds) + 61 lists of sizes 3 and 2 and their one integer.
    let held = charged("collection.holds_none(d, kinds)") - charged("true");
    assert!(held < 200, "charged {held}");
}
#[test]
fn k_col_008_mutations_preserve_object_identity_and_order() {
    assert_eq!(
        body(
            "let xs = [1, 3] let alias = xs do invoke list.insert(xs, 1, 2) let old = invoke list.remove(xs, 0) let n = invoke list.push(xs, 4) let last = invoke list.pop(xs) do invoke list.set(xs, 0, 9) return (alias, old, n, last)"
        ),
        tuple(vec![list(&[9, 3]), int(1), int(3), int(4)])
    );
    assert_eq!(
        body(
            "let m = map{1: 2} let s = set{1} do invoke map.set(m, 1, 3) do invoke map.insert(m, 2, 4) do invoke set.insert(s, 2) let rm = invoke map.remove(m, 1) let rs = invoke set.remove(s, 9) do invoke map.clear(m) do invoke set.clear(s) let xs = [] let empty = invoke list.pop(xs) do invoke list.push(xs, 1) do invoke list.clear(xs) return (m, s, rm, rs, empty, xs)"
        ),
        tuple(vec![
            Datum::Map(vec![]),
            Datum::Set(vec![]),
            Datum::Bool(true),
            Datum::Bool(false),
            Datum::Absent,
            list(&[])
        ])
    );
    assert_eq!(
        error("let xs = [1] do invoke list.insert(xs, -1, 2)"),
        "index_out_of_range"
    );
}
#[test]
fn k_col_009_record_copies_and_mutations_keep_field_order() {
    assert_eq!(
        body(
            "let r = {b: 2, a: 1} let changed = record.with(r, \"b\", 3) let removed = record.without(changed, \"a\") do invoke record.set(r, \"c\", 4) let had = invoke record.remove(r, \"b\") let keys = record.keys(r) do invoke record.clear(r) return (changed, removed, keys, had, r)"
        ),
        tuple(vec![
            Datum::Record(vec![("b".into(), int(3)), ("a".into(), int(1))]),
            Datum::Record(vec![("b".into(), int(3))]),
            Datum::List(vec![text("a"), text("c")]),
            Datum::Bool(true),
            Datum::Record(vec![])
        ])
    );
}
#[test]
fn k_col_010_callbacks_fold_group_and_short_circuit() {
    assert_eq!(
        body(
            "let twice = fn(x) { return num.add(x, x) } let small = fn(x) { return num.lt(x, 3) } let add = fn(a, x) { return num.add(a, x) } let same = fn(x) { return x } let arg1 = [1, 2] let m = invoke collection.map(arg1, twice) let arg2 = [1, 3, 2] let f = invoke collection.filter(arg2, small) let arg3 = [1, 2, 3] let r = invoke collection.reduce(arg3, add, 10) let arg4 = [3, 2, 1] let found = invoke collection.find(arg4, small) let arg5 = [] let has_any = invoke collection.any(arg5, small) let arg6 = [] let has_all = invoke collection.all(arg6, small) let arg7 = [2, 1, 2] let groups = invoke collection.group_by(arg7, same) let visits = [] let visit = fn(x) { set visits[list.len(visits)] = x } let arg8 = [1, 2] do invoke collection.for_each(arg8, visit) return (m, f, r, found, has_any, has_all, groups, visits)"
        ),
        tuple(vec![
            list(&[2, 4]),
            list(&[1, 2]),
            int(16),
            int(2),
            Datum::Bool(false),
            Datum::Bool(true),
            Datum::Map(vec![(int(2), list(&[2, 2])), (int(1), list(&[1]))]),
            list(&[1, 2])
        ])
    );
    assert_eq!(
        error("let f = fn(x) { return 1 } let arg9 = [1] do invoke collection.filter(arg9, f)"),
        "type_error"
    );
    assert_eq!(
        error(
            "let f = fn(x) { return [] } let arg10 = [1] do invoke collection.group_by(arg10, f)"
        ),
        "invalid_key"
    );
    assert_eq!(
        body(
            "let f = fn(x) { if num.lt(x, 2) { return true } throw \"visited after match\" } let xs = [1, 2] let found = invoke collection.find(xs, f) let exists = invoke collection.any(xs, f) let no = fn(x) { if num.lt(x, 2) { return false } throw \"visited after failure\" } let every = invoke collection.all(xs, no) return (found, exists, every)"
        ),
        tuple(vec![int(1), Datum::Bool(true), Datum::Bool(false)])
    );
    assert_eq!(
        error("let xs = [] do invoke collection.map(xs, 1)"),
        "type_error"
    );
}
#[test]
fn k_col_011_sort_is_stable_and_failure_never_publishes_partial_sort() {
    assert_eq!(
        body(
            "let xs = [(2, \"a\"), (1, \"b\"), (2, \"c\")] let compare = fn(a, b) { return num.sub(a[0], b[0]) } do invoke list.sort(xs, compare) return xs"
        ),
        Datum::List(vec![
            tuple(vec![int(1), text("b")]),
            tuple(vec![int(2), text("a")]),
            tuple(vec![int(2), text("c")])
        ])
    );
    assert_eq!(
        body(
            "let xs = [3, 2, 1] let calls = 0 let compare = fn(a, b) { set calls = num.add(calls, 1) if eq(calls, 2) { throw \"stop\" } return num.sub(a, b) } try { do invoke list.sort(xs, compare) } catch e { return (xs, calls, e) }"
        ),
        tuple(vec![list(&[3, 2, 1]), int(2), text("stop")])
    );
    assert_eq!(
        body(
            "let xs = [3, 2, 1] let compare = fn(a, b) { set xs[3] = 4 throw \"stop\" } try { do invoke list.sort(xs, compare) } catch e { return xs }"
        ),
        list(&[3, 2, 1, 4])
    );
    assert_eq!(
        error(
            "let xs = [2, 1] let compare = fn(a, b) { return nan } do invoke list.sort(xs, compare)"
        ),
        "unordered"
    );
    assert_eq!(
        error(
            "let xs = [2, 1] let compare = fn(a, b) { return false } do invoke list.sort(xs, compare)"
        ),
        "type_error"
    );
}
/// FIG-6528: stable sorting must use at most ceil(n log2 n) callbacks,
/// including adversarial orderings and ties. Counts and total VM charge are
/// printed at the trace's page sizes so the complexity claim is measurable.
#[test]
fn sort_comparisons_are_bounded_by_n_log_n() {
    let mut violations = Vec::new();
    for n in [30usize, 150, 750, 1000] {
        for shape in ["reversed", "seeded", "sorted", "equal"] {
            let mut seed = 6528u32;
            let ranks: Vec<usize> = (0..n)
                .map(|i| match shape {
                    "reversed" => n - i,
                    "seeded" => {
                        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        seed as usize % 31
                    }
                    "sorted" => i,
                    _ => 0,
                })
                .collect();
            let input = ranks
                .iter()
                .enumerate()
                .map(|(i, rank)| format!("({rank}, {i})"))
                .collect::<Vec<_>>()
                .join(",");
            let source = format!(
                "main {{ let xs = [{input}] let calls = 0 \
                 let compare = fn(a, b) {{ set calls = num.add(calls, 1) return num.sub(a[0], b[0]) }} \
                 do invoke list.sort(xs, compare) return (xs, calls) }}"
            );
            let mut machine = crate::testsupport::start(
                program(&source),
                Bounds {
                    charge_limit: 1_000_000_000,
                    ..BOUNDS
                },
                lash_kernel_vm::kernel_internals::Layout::default(),
            )
            .unwrap();
            let Step::Ended(End::Finished(done)) =
                machine.run(&mut World::default(), u64::MAX).unwrap()
            else {
                panic!("sort did not finish: {shape} n={n}")
            };
            let Datum::Tuple(parts) = done.result else {
                panic!("sort result")
            };
            let mut expected: Vec<_> = ranks.iter().copied().enumerate().collect();
            expected.sort_by_key(|(_, rank)| *rank);
            assert_eq!(
                parts[0],
                Datum::List(
                    expected
                        .into_iter()
                        .map(|(i, rank)| tuple(vec![int(rank as i64), int(i as i64)]))
                        .collect()
                )
            );
            let Datum::Int(calls) = &parts[1] else {
                panic!("callback count")
            };
            let calls: usize = calls.to_string().parse().unwrap();
            let bound = (n as f64 * (n as f64).log2()).ceil() as usize;
            eprintln!(
                "sort shape={shape} n={n} comparisons={calls} bound={bound} charge={}",
                machine.meters().charged.expect("exact charge")
            );
            if calls > bound {
                violations.push((shape, n, calls, bound));
            }
            // K-CHG-007 bills the completion formula plus metered loop
            // tests and ordinary callbacks; total charge is not a bound on
            // the sort formula alone. K-COL-014's schedule is covered by
            // operations::every_helper_bills_its_formula_including_callbacks_and_raises.
        }
    }
    assert!(
        violations.is_empty(),
        "quadratic comparisons: {violations:?}"
    );
}

#[test]
fn k_col_012_sum_zip_enumerate_and_range_have_kernel_bodies() {
    assert_eq!(
        body(
            "let arg11 = [1, 2, 3] let s = invoke collection.sum(arg11) let arg12 = [1, 2] let arg13 = [3] let z = invoke collection.zip(arg12,arg13) let arg14 = (4, 5) let e = invoke collection.enumerate(arg14) let r = invoke collection.range(3, -1, -2) let empty = invoke collection.range(3, 0) return (s, z, e, r, empty)"
        ),
        tuple(vec![
            int(6),
            Datum::List(vec![tuple(vec![int(1), int(3)])]),
            Datum::List(vec![
                tuple(vec![int(0), int(4)]),
                tuple(vec![int(1), int(5)])
            ]),
            list(&[3, 1]),
            list(&[])
        ])
    );
    assert_eq!(error("do invoke collection.range(0, 2, 0)"), "zero_step");
    assert_eq!(error("do invoke collection.range(0.0, 2)"), "type_error");
}
#[test]
fn k_col_013_task_helpers_join_work_and_cancellation_cleanup() {
    assert_eq!(
        result(
            "fn worker(x) { let v = perform echo(x) as Any return v } main { let a = spawn call worker(1) let b = spawn call worker(2) let out = invoke tasks.wait_all() return out }"
        ),
        list(&[1, 2])
    );
    assert_eq!(
        result(
            "fn worker() { try { let v = perform echo(1) as Any } finally { let c = perform echo(2) as Any } } main { let h = spawn call worker() let out = invoke tasks.cancel_all() return list.len(out) }"
        ),
        int(1)
    );
    assert_eq!(
        result(
            "fn child() { let x = perform echo(2) as Any return x } fn parent() { let x = perform echo(1) as Any let h = spawn call child() return x } main { let h = spawn call parent() let out = invoke tasks.wait_all() return out }"
        ),
        list(&[1, 2])
    );
    assert_eq!(
        result(
            "fn child() { try { let x = perform echo(3) as Any } finally { let x = perform echo(4) as Any } } fn parent() { try { let x = perform echo(1) as Any } finally { let h = spawn call child() } } main { let h = spawn call parent() let out = invoke tasks.cancel_all() return list.len(out) }"
        ),
        int(2)
    );
}
#[test]
fn k_iter_001_iterables_and_callback_order_are_kernel_rules() {
    recorded_iteration(include_str!(
        "../../corpus/collections/iteration-kinds.json"
    ));
    assert_eq!(
        body(
            "let id = fn(x) { return x } let arg15 = map{2: 4, 1: 3} let m = invoke collection.map(arg15, id) let arg16 = set{2, 1} let s = invoke collection.map(arg16, id) let arg17 = (3, 4) let t = invoke collection.map(arg17, id) return (m, s, t)"
        ),
        tuple(vec![list(&[2, 1]), list(&[2, 1]), list(&[3, 4])])
    );
    assert_eq!(
        error("let id = fn(x) { return x } let arg18 = {} do invoke collection.map(arg18, id)"),
        "type_error"
    );
}
#[test]
fn k_iter_002_lists_are_live_even_through_callback_mutation() {
    recorded_iteration(include_str!("../../corpus/collections/iteration-list.json"));
    assert_eq!(
        body(
            "let xs = [1, 2] let f = fn(x) { if eq(x, 1) { set xs[2] = 3 } if eq(x, 2) { remove xs[0] } return x } let out = invoke collection.map(xs, f) return (out, xs)"
        ),
        tuple(vec![list(&[1, 2]), list(&[2, 3])])
    );
}
#[test]
fn k_iter_003_maps_and_sets_visit_additions_and_skip_removals() {
    recorded_iteration(include_str!(
        "../../corpus/collections/iteration-tables.json"
    ));
    assert_eq!(
        body(
            "let m = map{1: 1, 2: 2} let s = set{1, 2} let mf = fn(x) { if eq(x, 1) { remove m[2] set m[3] = 3 set m[1] = 9 } return x } let sf = fn(x) { if eq(x, 1) { remove s[2] set s[2] = true set s[3] = true } return x } let a = invoke collection.map(m, mf) let b = invoke collection.map(s, sf) return (a, b)"
        ),
        tuple(vec![list(&[1, 3]), list(&[1, 2, 3])])
    );
}

fn callback_law(source: &str, expected: Datum, expected_effects: usize) {
    // The uninterrupted witness has the same callback body without its effect.
    // S4 owns checkpoint sealing; this law pins ordinary activation
    // waits, resuming the machine through its public delivery interface.
    let uninterrupted = source.replace("let y = perform echo(x) as Any", "let y = x");
    assert_eq!(result(&uninterrupted), expected);
    let program = program(source);
    let mut machine = crate::testsupport::start(
        program.clone(),
        BOUNDS,
        lash_kernel_vm::kernel_internals::Layout::default(),
    )
    .unwrap();
    let mut effects = 0;
    loop {
        match machine.run(&mut World::default(), u64::MAX).unwrap() {
            Step::Ended(End::Finished(done)) => {
                assert_eq!(done.result, expected);
                break;
            }
            Step::Ended(other) => panic!("{other:?}"),
            Step::Slice => {}
            Step::Parked(park) => {
                for request in park.requests {
                    let Request::Effect(effect) = request else {
                        panic!("callback requested a sleep")
                    };
                    effects += 1;
                    machine
                        .deliver(effect.wait, Outcome::Completed(effect.args[0].clone()))
                        .unwrap();
                }
            }
        }
    }
    assert_eq!(effects, expected_effects);
}
#[test]
fn map_callback_wait_resumes_the_taken_element() {
    callback_law(
        "fn f(x) { let y = perform echo(x) as Any return num.add(y, y) } main { let arg19 = [1, 2, 3] let out = invoke collection.map(arg19, &f) return out }",
        list(&[2, 4, 6]),
        3,
    );
}
#[test]
fn filter_callback_wait_resumes_the_predicate_and_element() {
    callback_law(
        "fn f(x) { let y = perform echo(x) as Any return num.lt(y, 3) } main { let arg20 = [1, 3, 2] let out = invoke collection.filter(arg20, &f) return out }",
        list(&[1, 2]),
        3,
    );
}
#[test]
fn reduce_callback_wait_resumes_the_accumulator() {
    callback_law(
        "fn f(a, x) { let y = perform echo(x) as Any return num.add(a, y) } main { let arg21 = [1, 2, 3] let out = invoke collection.reduce(arg21, &f, 10) return out }",
        int(16),
        3,
    );
}
#[test]
fn sort_callback_wait_resumes_the_snapshot_and_comparator_operands() {
    callback_law(
        "fn f(a, b) { let x = num.sub(a, b) let y = perform echo(x) as Any return y } main { let xs = [3, 1, 2] do invoke list.sort(xs, &f) return xs }",
        list(&[1, 2, 3]),
        3,
    );
}

#[test]
fn k_bool_001_not_is_strict_without_truthiness() {
    assert_eq!(
        body("return (bool.not(true), bool.not(false))"),
        tuple(vec![Datum::Bool(false), Datum::Bool(true)])
    );
    for value in ["0", "null", "absent", "\"\"", "[]"] {
        assert_eq!(error(&format!("return bool.not({value})")), "type_error");
    }
}
#[test]
fn k_col_015_error_new_has_strict_text_and_retains_data_identity() {
    assert_eq!(
        body(
            "let xs = [1] let e = error.new(\"kind\", \"message\", xs) set xs[0] = 2 let empty = error.new(\"kind\", \"message\") return (e.kind, e.message, e.data, empty.data)"
        ),
        tuple(vec![
            text("kind"),
            text("message"),
            list(&[2]),
            Datum::Absent
        ])
    );
    assert_eq!(error("return error.new(1, \"message\")"), "type_error");
    assert_eq!(error("return error.new(\"kind\", false)"), "type_error");
}

/// `K-CHG-003`, `K-CHG-005`: `same` compares two immutable values member by
/// member, so it is charged the immutable values it walks: two equal texts
/// in separate nested tuples, or in two errors' data, cost at least their
/// bytes (FIG-5825).
#[test]
fn k_chg_005_same_is_charged_the_immutable_values_nested_in_its_operands() {
    const BYTES: u64 = 100_000;
    let long = format!("\"{}\"", "x".repeat(BYTES as usize));
    for compared in [
        format!("let a = (({long},),) let b = (({long},),) return same(a, b)"),
        format!(
            "let a = error.new(\"k\", \"m\", ({long},)) let b = error.new(\"k\", \"m\", ({long},)) \
             return same(a, b)"
        ),
    ] {
        let mut machine = machine(&format!("main {{ {compared} }}"));
        let Step::Ended(End::Finished(done)) =
            machine.run(&mut World::default(), u64::MAX).unwrap()
        else {
            panic!("{compared}")
        };
        assert_eq!(done.result, Datum::Bool(true));
        let charged = machine.meters().charged.expect("exact charge");
        assert!(charged >= 2 * BYTES, "charged {charged} for {compared:.40}");
    }
}

#[test]
fn k_col_014_native_charges_count_the_sizes_and_are_repeatable() {
    // K-CHG-004: size(list) = 1 + length. copy's charge is
    // 1 + size(xs) + size(result), independently of previous allocations.
    let registry = registry();
    let (_, copy) = registry
        .iter()
        .find(|(_, f)| f.definition().name.as_str() == "list.copy")
        .unwrap();
    let mut heap = crate::testsupport::heap();
    use lash_kernel_doc::{Operand, WorkCounter};
    let xs = crate::testsupport::decode(&mut heap, Datum::List(vec![Datum::Null; 3]));
    for _ in 0..2 {
        let result = crate::testsupport::call(
            &mut heap,
            &**copy.native().unwrap(),
            std::slice::from_ref(&xs),
            u64::MAX,
            &mut WorkCounter::new(None),
        )
        .unwrap();
        let charge = copy.definition().charge.evaluate(&mut |operand, _| {
            let value = match operand {
                Operand::Result => &result,
                Operand::Param(_) => &xs,
            };
            1 + heap.arena().len(value.handle().unwrap()).unwrap()
        });
        assert_eq!(charge, 9);
        assert_ne!(xs.handle(), result.handle());
    }
}

fn recorded_iteration(shard: &str) {
    let shard: serde_json::Value = serde_json::from_str(shard).unwrap();
    for case in shard["cases"].as_array().unwrap() {
        let document = parse_document(case["document"].as_str().unwrap()).unwrap();
        let program = Program {
            release: lash_kernel_doc::heap::PageId([8; 32]),
            document: Arc::new(document),
            library: lash_kernel_vm::PreparedLibrary::new(Arc::new(FunctionRegistry::new())),
        };
        let mut machine = crate::testsupport::start(
            program,
            BOUNDS,
            lash_kernel_vm::kernel_internals::Layout::default(),
        )
        .unwrap();
        let mut world = World::default();
        let Step::Ended(End::Finished(done)) = machine.run(&mut world, u64::MAX).unwrap() else {
            panic!("self-contained iteration case did not finish")
        };
        let expected: Datum =
            serde_json::from_value(case["expected"]["end"]["finished"].clone()).unwrap();
        let prints: Vec<Datum> =
            serde_json::from_value(case["expected"]["prints"].clone()).unwrap();
        assert_eq!(done.result, expected, "{}", case["name"]);
        assert_eq!(world.prints, prints, "{}", case["name"]);
    }
}

/// FIG-5874 / K-CHG-005: a range search pays for the input graph and
/// at most length comparisons against the needle, plus index validation.
#[test]
fn index_of_range_keeps_numeric_equivalence_and_rejects_bad_bounds() {
    assert_eq!(
        body(
            "return (list.index_of_range([nan, -0.0, 0.0, nan], nan, 0, 4, false, true), list.index_of_range([nan, -0.0, 0.0, nan], nan, 0, 4, true, true), list.index_of_range([nan], nan, 0, 1, false, false), list.index_of_range([-0.0, 0.0], 0.0, 0, 2, true, false))"
        ),
        tuple(vec![int(0), int(3), int(-1), int(1)])
    );
    for bounds in ["-1, 1", "0, 2", "1, 0", "nan, 1", "0.5, 1", "inf, 1"] {
        assert_eq!(
            error(&format!(
                "return list.index_of_range([0.0], 0.0, {bounds}, false, false)"
            )),
            "index_out_of_range"
        );
    }
    // Kernel equality remains mathematical across numeric kinds.
    assert_eq!(
        body("return list.index_of_range([0], -0.0, 0, 1, false, false)"),
        int(0)
    );
}

/// K-LIB-006/007: each collection native has a named, hand-derived case;
/// a second run with the same prepared native implementations agrees.
#[test]
fn every_collection_native_has_a_named_cold_and_warm_case() {
    let inputs = [
        ("list", "[7]", Datum::List(vec![int(7)])),
        ("tuple", "(7,)", tuple(vec![int(7)])),
        ("map", "map{7: 9}", Datum::Map(vec![(int(7), int(9))])),
        ("set", "set{7}", Datum::Set(vec![int(7)])),
        (
            "record",
            "{a: 7}",
            Datum::Record(vec![("a".into(), int(7))]),
        ),
    ];
    let mut cases = Vec::new();
    for (kind, input, copied) in inputs {
        cases.push((format!("{kind}.len"), input.to_owned(), Ok(int(1))));
        for op in ["copy", "copy_deep", "check"] {
            if kind == "tuple" && op == "check" {
                continue;
            }
            cases.push((format!("{kind}.{op}"), input.to_owned(), Ok(copied.clone())));
        }
        let (keys, values, entries) = match kind {
            "list" | "tuple" => (
                list(&[0]),
                list(&[7]),
                Datum::List(vec![tuple(vec![int(0), int(7)])]),
            ),
            "map" => (
                list(&[7]),
                list(&[9]),
                Datum::List(vec![tuple(vec![int(7), int(9)])]),
            ),
            "set" => (
                list(&[7]),
                list(&[7]),
                Datum::List(vec![tuple(vec![int(7), int(7)])]),
            ),
            "record" => (
                Datum::List(vec![text("a")]),
                list(&[7]),
                Datum::List(vec![tuple(vec![text("a"), int(7)])]),
            ),
            _ => unreachable!(),
        };
        for (op, expected) in [("keys", keys), ("values", values), ("entries", entries)] {
            cases.push((format!("{kind}.{op}"), input.to_owned(), Ok(expected)));
        }
        match kind {
            "list" | "tuple" => {
                for op in ["get", "at"] {
                    cases.push((format!("{kind}.{op}"), format!("{input}, 0"), Ok(int(7))));
                }
                if kind == "list" {
                    cases.push((
                        "list.insert_index".into(),
                        format!("{input}, 1"),
                        Ok(int(1)),
                    ));
                }
                cases.push((
                    format!("{kind}.slice"),
                    format!("{input}, 0, 1"),
                    Ok(copied.clone()),
                ));
                let twice = if kind == "list" {
                    list(&[7, 7])
                } else {
                    tuple(vec![int(7), int(7)])
                };
                cases.push((
                    format!("{kind}.concat"),
                    format!("{input}, {input}"),
                    Ok(twice),
                ));
                cases.push((
                    format!("{kind}.contains"),
                    format!("{input}, 7"),
                    Ok(Datum::Bool(true)),
                ));
                cases.push((
                    format!("{kind}.index_of"),
                    format!("{input}, 7"),
                    Ok(int(0)),
                ));
                if kind == "list" {
                    cases.push((
                        "list.index_of_range".into(),
                        format!("{input}, 7, 0, 1, false, false"),
                        Ok(int(0)),
                    ));
                }
            }
            "map" => {
                cases.push(("map.get".into(), format!("{input}, 7"), Ok(int(9))));
                cases.push((
                    "map.contains".into(),
                    format!("{input}, 7"),
                    Ok(Datum::Bool(true)),
                ));
            }
            "set" => {
                cases.push((
                    "set.contains".into(),
                    format!("{input}, 7"),
                    Ok(Datum::Bool(true)),
                ));
            }
            "record" => {
                cases.push(("record.get".into(), format!("{input}, \"a\""), Ok(int(7))));
                cases.push((
                    "record.contains".into(),
                    format!("{input}, \"a\""),
                    Ok(Datum::Bool(true)),
                ));
                cases.push((
                    "record.with".into(),
                    format!("{input}, \"b\", 9"),
                    Ok(Datum::Record(vec![
                        ("a".into(), int(7)),
                        ("b".into(), int(9)),
                    ])),
                ));
                cases.push((
                    "record.without".into(),
                    format!("{input}, \"a\""),
                    Ok(Datum::Record(vec![])),
                ));
            }
            _ => unreachable!(),
        }
    }
    cases.extend([
        (
            "key.identity".into(),
            "absent, 7, \"error\"".into(),
            Ok(int(7)),
        ),
        ("list.get_or".into(), "[7], 2, 9".into(), Ok(int(9))),
        ("bool.not".into(), "true".into(), Ok(Datum::Bool(false))),
        (
            "error.new".into(),
            "\"k\", \"m\", 7".into(),
            Ok(Datum::Error(Box::new(ErrorDatum {
                kind: "k".into(),
                message: "m".into(),
                data: int(7),
            }))),
        ),
        (
            "collection.unordered".into(),
            "null".into(),
            Err("unordered"),
        ),
        ("collection.zero_step".into(), "0".into(), Err("zero_step")),
        ("collection.int_check".into(), "7".into(), Ok(int(7))),
        (
            "collection.function_check".into(),
            "null".into(),
            Err("type_error"),
        ),
        (
            "collection.holds_none".into(),
            "[7], set{\"text\"}".into(),
            Ok(Datum::Bool(true)),
        ),
    ]);
    let mut registered = FunctionRegistry::new();
    register_machine_functions(&mut registered).unwrap();
    crate::register_numbers(&mut registered).unwrap();
    let collection_ids = register_collections(&mut registered).unwrap();
    let natives: std::collections::BTreeSet<_> = collection_ids
        .into_values()
        .filter(|id| registered.get(id).unwrap().native().is_some())
        .collect();
    let mut exercised = std::collections::BTreeSet::new();
    for (name, args, expected) in cases {
        let (id, _) = registered
            .iter()
            .find(|(_, function)| function.definition().name.as_str() == name)
            .unwrap();
        exercised.insert(*id);
        let program = program(&format!("main {{ return {name}({args}) }}"));
        for temperature in ["cold", "warm"] {
            let mut machine = crate::testsupport::start(
                program.clone(),
                BOUNDS,
                lash_kernel_vm::kernel_internals::Layout::default(),
            )
            .unwrap();
            let Step::Ended(end) = machine.run(&mut World::default(), u64::MAX).unwrap() else {
                panic!("{name} must end")
            };
            match (end, &expected) {
                (End::Finished(finished), Ok(value)) => {
                    assert_eq!(&finished.result, value, "{name} {temperature}")
                }
                (End::Error(RunError::Uncaught(Datum::Error(error))), Err(kind)) => {
                    assert_eq!(&error.kind, kind, "{name} {temperature}")
                }
                (end, expected) => panic!("{name} {temperature}: {end:?} != {expected:?}"),
            }
        }
    }
    assert_eq!(
        exercised, natives,
        "every collection native needs a named corpus case"
    );
}

/// K-COL-017: identity projection never reads the objects it keys. Its
/// formula pays for each visited tuple slot and the immutable result only.
#[test]
fn identity_key_projection_preserves_distinct_error_data_identities() {
    assert_eq!(
        body(
            "let a = {} let b = {} let ea = error.new(\"k\", \"m\", a) let eb = error.new(\"k\", \"m\", b) return (same(key.identity(ea, 0, \"fault\"), (\"fault\", ref(a), \"k\", \"m\")), same(key.identity(ea, 0, \"fault\"), key.identity(eb, 0, \"fault\")))"
        ),
        tuple(vec![Datum::Bool(true), Datum::Bool(false)])
    );
}

/// K-COL-018: only a nonnegative integral position outside the list uses
/// the supplied default; a present absent is not replaced by that default.
#[test]
fn list_get_or_distinguishes_missing_positions_from_present_absent() {
    assert_eq!(
        body(
            "return (list.get_or([absent, 7], 0, 9), list.get_or([absent, 7], 1, 9), list.get_or([absent, 7], 2, 9), list.get_or([], 0), list.get_or([], 99999999999999999999999999, 8))"
        ),
        tuple(vec![Datum::Absent, int(7), int(9), Datum::Absent, int(8)])
    );
    for index in ["-1", "0.5", "nan", "inf"] {
        assert_eq!(
            error(&format!("return list.get_or([], {index}, 9)")),
            "index_out_of_range"
        );
    }
    assert_eq!(error("return list.get_or([], \"0\", 9)"), "type_error");
}

/// K-BND-003: expansion refuses against the actual S7 census while its
/// input remains rooted; no reservation spy decides this result.
#[test]
fn collection_expansion_refuses_under_the_real_census() {
    use lash_kernel_doc::WorkCounter;
    let registry = registry();
    for name in ["tuple.concat", "list.concat", "list.entries"] {
        let mut heap = crate::testsupport::heap();
        let xs = crate::testsupport::decode(
            &mut heap,
            if name == "tuple.concat" {
                Datum::Tuple(vec![Datum::Null])
            } else {
                Datum::List(vec![Datum::Null])
            },
        );
        let args = [xs, xs];
        let args = if name == "list.entries" {
            &args[..1]
        } else {
            &args[..]
        };
        let (_, function) = registry
            .iter()
            .find(|(_, f)| f.definition().name.as_str() == name)
            .unwrap();
        let result = crate::testsupport::call(
            &mut heap,
            &**function.native().unwrap(),
            args,
            0,
            &mut WorkCounter::new(None),
        );
        assert!(crate::testsupport::exhausted(&result), "{name}: {result:?}");
        assert_eq!(heap.arena().len(args[0].handle().unwrap()).unwrap(), 1);
    }
}

/// FIG-5874 / K-CHG-005: a range search pays for the input graph and
/// at most length comparisons against the needle, plus index validation,
/// and its charge bounds the work it did on the paged heap.
#[test]
fn index_of_range_charge_bounds_search_and_numeric_equivalence() {
    use lash_kernel_doc::{Measure, Operand, WorkCounter};
    let registry = registry();
    let (_, search) = registry
        .iter()
        .find(|(_, f)| f.definition().name.as_str() == "list.index_of_range")
        .unwrap();
    for length in [0, 1, 8, 64] {
        for bytes in [0, 1, 64] {
            let mut heap = crate::testsupport::heap();
            let needle = Datum::Text("x".repeat(bytes));
            let args: Vec<_> = [
                Datum::List(vec![needle.clone(); length]),
                needle,
                int(0),
                int(length as i64),
                Datum::Bool(false),
                Datum::Bool(false),
            ]
            .into_iter()
            .map(|arg| crate::testsupport::decode(&mut heap, arg))
            .collect();
            let mut counter = WorkCounter::new(None);
            let answer = crate::testsupport::call(
                &mut heap,
                &**search.native().unwrap(),
                &args,
                u64::MAX,
                &mut counter,
            )
            .unwrap();
            assert_eq!(
                crate::testsupport::datum(&heap, &answer),
                int(if length == 0 { -1 } else { 0 })
            );
            let charge = search
                .definition()
                .charge
                .evaluate(&mut |operand, measure| {
                    let Operand::Param(name) = operand else {
                        panic!("formula only measures arguments");
                    };
                    match (name.as_str(), measure) {
                        ("xs", Measure::Size) => 1 + length as u64,
                        ("xs", Measure::DeepSize) => 1 + length as u64 * (2 + bytes as u64),
                        ("value", Measure::DeepSize) => 1 + bytes as u64,
                        ("start", Measure::Size) => 1,
                        ("end", Measure::Size) => {
                            if length == 0 {
                                1
                            } else {
                                2
                            }
                        }
                        other => panic!("unexpected measure: {other:?}"),
                    }
                });
            assert_eq!(
                charge,
                12 + u64::from(length != 0) + length as u64 * (3 + 2 * bytes as u64) + bytes as u64
            );
            assert!(charge >= length as u64 * (1 + bytes as u64));
            assert!(
                charge >= counter.spent(),
                "length {length}, bytes {bytes}: charged {charge} for work {}",
                counter.spent()
            );
        }
    }
}

/// K-COL-017: identity projection keys objects by identity without
/// reading them. Its formula pays for each visited tuple slot and the
/// immutable result only, and bounds the work it did on the paged heap.
#[test]
fn identity_key_projection_preserves_identity_and_bounds_its_charge() {
    use lash_kernel_doc::{Float, Operand, Value, WorkCounter};
    let registry = registry();
    let (_, function) = registry
        .iter()
        .find(|(_, f)| f.definition().name.as_str() == "key.identity")
        .unwrap();
    let native = function.native().unwrap();
    let mut heap = crate::testsupport::heap();
    let Value::List(object) =
        crate::testsupport::decode(&mut heap, Datum::List(vec![Datum::Null; 4096]))
    else {
        panic!("a list decodes to a list")
    };
    let missing = crate::testsupport::decode(&mut heap, tuple(vec![text("missing")]));
    let fault = crate::testsupport::decode(&mut heap, text("fault"));
    for length in [0, 1, 8, 64] {
        let keys = heap
            .arena_mut()
            .tuple(vec![Value::List(object); length], usize::MAX)
            .unwrap();
        let args = [keys, missing, fault];
        let mut work = WorkCounter::new(None);
        let out =
            crate::testsupport::call(&mut heap, &**native, &args, u64::MAX, &mut work).unwrap();
        let Value::Tuple(out) = out else {
            panic!("a projection of a tuple is a tuple")
        };
        let members = heap.arena().semantics().sequence(out).unwrap().to_vec();
        assert_eq!(members.len(), length);
        assert!(
            members
                .iter()
                .all(|member| matches!(member, Value::ObjectRef(id) if *id == object)),
            "every slot keys the one object by its identity"
        );
        let charged = function.definition().charge.evaluate(&mut |operand, _| {
            assert_eq!(*operand, Operand::Result);
            1 + length as u64
        });
        assert_eq!(charged, 2 + length as u64);
        assert_eq!(work.spent(), 1 + length as u64);
        assert!(charged >= work.spent());
    }
    for (value, expected) in [
        (Value::Absent, crate::testsupport::datum(&heap, &missing)),
        (
            Value::Float(Float::new(-0.0)),
            Datum::Float(Float::new(0.0)),
        ),
    ] {
        let args = [value, missing, fault];
        let out = crate::testsupport::call(
            &mut heap,
            &**native,
            &args,
            u64::MAX,
            &mut WorkCounter::new(None),
        )
        .unwrap();
        if let Value::Float(zero) = &out {
            assert_eq!(zero.get().to_bits(), 0.0_f64.to_bits());
        }
        assert_eq!(crate::testsupport::datum(&heap, &out), expected);
    }
}
