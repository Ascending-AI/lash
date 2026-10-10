//! Laws of K-COL and K-ITER, using the public VM as an in-process embedder.
use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionRegistry, Handle, Integer, Timestamp, parse_document,
};
use lash_kernel_vm::{
    Bounds, End, Host, KernelMachine, Machine, Outcome, Program, Request, RunError, Start, Step,
    Target, register_machine_functions,
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
    charge: 10_000_000,
    memory: 64 << 20,
    call_depth: 100,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};
fn program(source: &str) -> Program {
    let registry = Arc::new(registry());
    let mut text = String::from("kernel 1\nnumbers by_spelling\neffect echo(x: Any) -> Any\n");
    for (id, function) in registry.iter() {
        text.push_str(&format!("use {} = @{id}\n", function.definition.name));
    }
    text.push_str(source);
    Program {
        document: Arc::new(parse_document(&text).unwrap()),
        library: lash_kernel_vm::PreparedLibrary::new(registry),
    }
}
fn machine(source: &str) -> KernelMachine {
    KernelMachine::start(
        program(source),
        BOUNDS,
        Start {
            target: Target::Main,
            args: vec![],
            bindings: Default::default(),
        },
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
    // KPARK owns persisted export/import; this law pins ordinary activation
    // waits, resuming the machine through its public delivery interface.
    let uninterrupted = source.replace("let y = perform echo(x) as Any", "let y = x");
    assert_eq!(result(&uninterrupted), expected);
    let program = program(source);
    let mut machine = KernelMachine::start(
        program.clone(),
        BOUNDS,
        Start {
            target: Target::Main,
            args: vec![],
            bindings: Default::default(),
        },
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
        "fn f(a, b) { let x = num.sub(a, b) let y = perform echo(x) as Any return y } main { let xs = [3, 2, 1] do invoke list.sort(xs, &f) return xs }",
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

#[test]
fn k_col_014_native_charges_count_the_sizes_and_are_repeatable() {
    // K-CHG-004: size(list) = 1 + length. copy's charge is
    // 1 + size(xs) + size(result), independently of previous allocations.
    let registry = registry();
    let (_, copy) = registry
        .iter()
        .find(|(_, f)| f.definition.name.as_str() == "list.copy")
        .unwrap();
    let mut heap = crate::tests::Heap::default();
    use lash_kernel_doc::{NativeCall, NativeHeap, Object, Operand, Value, WorkCounter};
    let xs = Value::List(heap.allocate(Object::List(vec![Value::Null; 3])).unwrap());
    for _ in 0..2 {
        let result = copy
            .native
            .as_ref()
            .unwrap()
            .call(NativeCall {
                args: std::slice::from_ref(&xs),
                heap: &mut heap,
                counter: &mut WorkCounter::new(None),
            })
            .unwrap();
        let charge = copy.definition.charge.evaluate(&mut |operand, _| {
            let value = match operand {
                Operand::Result => &result,
                Operand::Param(_) => &xs,
            };
            1 + heap.len(value.object().unwrap()) as u64
        });
        assert_eq!(charge, 9);
        assert_ne!(xs, result);
    }
}

fn recorded_iteration(shard: &str) {
    let shard: serde_json::Value = serde_json::from_str(shard).unwrap();
    for case in shard["cases"].as_array().unwrap() {
        let document = parse_document(case["document"].as_str().unwrap()).unwrap();
        let program = Program {
            document: Arc::new(document),
            library: lash_kernel_vm::PreparedLibrary::new(Arc::new(FunctionRegistry::new())),
        };
        let mut machine = KernelMachine::start(
            program,
            BOUNDS,
            Start {
                target: Target::Main,
                args: vec![],
                bindings: Default::default(),
            },
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
