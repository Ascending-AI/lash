//! Call-owned frame entry preserves source operands and continuations.
mod construction;

const SOURCE_SHARED_ENTRY: &str = "let makers = (call maker(3, 9), call maker(4)) let first = makers[0] let second = makers[1] return (apply first(), apply second())";
use super::*;

const SAVED_MEMBER: &str = "let container = [7] let change = fn() { set container[0] = 9 return 2 } let pair = fn(a, b) { return [a, b] } let answer = apply pair(container[0], apply change()) return (answer, container)";
const SAVED_MEMBER_PARK: &str = "let container = [7] let change = fn() { set container[0] = 9 do perform 0(\"changed\") as Any return 2 } let pair = fn(a, b) { return [a, b] } let answer = apply pair(invoke helper.element(container, 0), apply change()) return (answer, container)";
const SUPERSEDED_RETURN: &str = "let cleanup = fn() { try { return [7] } finally { print \"cleanup\" return [9] } } return apply cleanup()";

const NESTED_RETURN: &str = "let cleanup = fn() { try { return [7] } finally { try { return [8] } finally { print \"nested cleanup\" return [9] } } } return apply cleanup()";

const ROOT_CALL_STORAGE: &str = "let cleanup = fn(xs) { try { return [7] } finally { let first = call pair(xs[0], xs[1]) let second = call pause(first) return [second, xs[2], xs[3], xs[4], xs[5], xs[6], xs[7], xs[8], xs[9]] } } return apply cleanup([7, 8, 9, 10, 11, 12, 13, 14, 15, 16])";
// K-EVAL-008: nested calls retain a pre-call operand and pending return
// while reusing a wide caller's dead tail, then restore it for later writes.
const NESTED_CALL_STORAGE: &str = "let cleanup = fn(xs) { try { return [7] } finally { let first = flat.add(xs[0], call pause(xs[1])) return [call pair(first, xs[2]), xs[3], xs[4], xs[5], xs[6], xs[7], xs[8], xs[9]] } } return apply cleanup([7, 8, 9, 10, 11, 12, 13, 14, 15, 16])";

const READ_ADD_WRITE: &str = "let run = fn(obj) { let total = 1 set total = flat.add(total, invoke helper.element_map_any(obj, \"a\")) return total } return apply run(map{\"a\": 255})";
const SOURCE_SESSION: &str = "let saved = [7] set saved = [saved, saved] for ignored in [0, 1] { set saved = saved } let active = true while active { set saved = saved set active = false } return saved";
const SOURCE_CONSTRUCTION: &str = "let f = fn(value) { let empty_a = () let empty_b = () if same(empty_a, empty_b) { } else { return empty_b } let large = resident.large_add(9223372036854775807, value) let again = resident.negate(large) let negative = resident.negate(value) let restored = resident.add(negative, 14) let minimum = resident.minimum(restored, 9) let pair = [[minimum], value] let other = [pair, value] set pair = other let count = resident.len(other) let first = list.get(other, 0) let deep = resident.deep_copy(other) if same(deep[0][0][0], value) { } else { return deep } set deep[0][0][0] = 9 if same(other[0][0][0], value) { } else { return deep } let copied = resident.copy(other) set copied[0] = value let temporary = resident.copy([[value], value]) if same(temporary[0][0], value) { } else { return temporary } let made_tuple = ((pair, pair), value) if same(made_tuple[0][1], pair) { } else { return made_tuple } let made_map = map{\"a\": value, \"a\": pair} let copied_map = resident.copy_map(made_map) if same(copied_map[\"a\"], pair) { } else { return copied_map } let made_set = set{value, value} let copied_set = resident.copy_set(made_set) if copied_set[value] { } else { return copied_set } let made_record = {b: value, a: pair} let copied_record = resident.copy_record(made_record) if same(copied_record.a, pair) { } else { return copied_record } let joined = text.join([\"a canonical long joined value\", \"with an allocated native output\"], \";\") let repeated = text.repeat(joined, 2) let trimmed = text.slice(repeated, 0, 5) if same(trimmed, \"a can\") { } else { return trimmed } if true { let alias = [other, other] } return other } return apply f(7)";
const SOURCE_BAD_MEMBER: &str = "let xs = [1] try { do xs } catch unused { print unused.kind } finally { do xs } try { set xs[3] = 2 } catch e { print e.kind } try { set xs[2] = 2 } catch e { print e.kind } return xs";
// K-CHG-001: an unpaid source region cannot publish a captured closure.
const SOURCE_CLOSURE_PREFIX: &str = "let run = fn(value) { let captured = value do let(wrapper = fn() { return captured }, call literal(wrapper)) return captured } return apply run(7)";
const SOURCE_SCOPED_BINDINGS: &str = "let run = fn(value) { if true { do value } else { do value } if false { do value } else { do value } do \"a deferred source payload beyond the inline representation\" do 1000000000000000000000000 do b\"0011ff\" let callback = &literal do apply callback(value) let ordinary = let(alias = if(true, value, value), alias) let captured = let(shared = [if(false, value, ordinary)], let(get = fn() { return shared }, apply get())) do {value: [value]}.value do [[value]][0] for ignored in [0, 1] { let scoped = let(alias = [value], alias) let capture = fn() { return scoped } set scoped = [value] } else { do value } let active = true while active { let scoped = let(alias = value, alias) set active = false } return [ordinary, captured] } return apply run(7)";
const SOURCE_CELL: &str = "let run = fn(value) { let saved = value let getter = fn() { return saved } let setter = fn(next) { set saved = next return saved } set saved = [value, value] let alias = apply getter() let changed = apply setter([alias, alias]) return (alias, changed, apply getter()) } return apply run([7])";
const SOURCE_MEMBER: &str = "let nested = [7] let f = fn(xs, value) { let table = map{\"keep\": value} set table[\"keep\"] = 9 set table[\"a dynamic key beyond the inline representation\"] = value remove table[\"keep\"] remove table[\"absent\"] let record = {keep: value} set record[\"added\"] = value remove record[\"keep\"] set record.short = value if same(record.short, value) { } else { return record } set record.a_static_field_name_beyond_the_inline_representation = value if same(record.a_static_field_name_beyond_the_inline_representation, value) { } else { return record } set record.a_static_field_name_beyond_the_inline_representation = 9 if same(record.a_static_field_name_beyond_the_inline_representation, 9) { } else { return record } remove record.short remove record.a_static_field_name_beyond_the_inline_representation remove record.an_absent_static_field_name_beyond_the_inline_representation if same(record.len(record), 1) { } else { return record } let members = set{7} set members[8] = true set members[7] = false remove members[8] if members[7] { return members } if members[8] { return members } if same(record[\"added\"], value) { } else { return record } let numeric = map{1: value} set numeric[1.0] = 9 if same(numeric[1], 9) { } else { return numeric } let transient = [value, 0, 1] remove transient[1] remove transient[1] remove transient[0] set xs[0] = value set xs[1] = value set xs[2] = value set xs[3] = 9 set value[0] = 8 return xs } return apply f([], nested)";
const SOURCE_UNIT: &str = "let nested = [7] let f = fn(value) { if same(value, ()) { return 0 } return value } return apply f(nested)";
const SOURCE_PENDING_UNIT: &str = "let nested = [7] let f = fn(value) { try { return [9] } finally { if same(value, ()) { return 0 } } } return apply f(nested)";
const READ_ADD_MISS: &str = "let run = fn(obj) { let total = 1 set total = flat.add(total, invoke helper.element_map_any(obj, \"missing\")) return total } return apply run(map{\"a\": 255})";
const READ_ADD_FAIL: &str = "let run = fn(obj) { let total = false set total = flat.add(total, invoke helper.element_map_any(obj, \"a\")) return total } return apply run(map{\"a\": 255})";

const FORMATTED_APPEND: &str = "let build = fn(n) { let text = \"\" let i = 0 while flat.lt(i, n) { set text = text.concat(text, text.join([\"Tool \", num.to_text(i), \" value=\", num.to_text(flat.mul(i, 3)), \"; status=ready\\n\"], \"\")) set i = flat.add(i, 1) } return text } let answer = apply build(3) do perform 0(answer) as Any return answer";

const FORMATTED_MISS: &str = "let build = fn() { let text = \"old\" set text = text.concat(text, text.join([\"field=\", 1, \"; suffix\"], \"\")) return text } return apply build()";

thread_local! {
    // PreparedLibrary is an immutable, shareable fixture. Cold reopen still
    // constructs its own library in Embedder::reopen.
    static CALL_LIBRARY: crate::PreparedLibrary = {
        let fixture = super::super::embedder::library_with(true, Some(helper_probes));
        crate::PreparedLibrary::new(std::sync::Arc::clone(&fixture.registry))
    };
}

#[derive(Debug, PartialEq)]
struct Observed {
    end: String,
    printed: Vec<Datum>,
    requests: Vec<Request>,
    stops: Vec<(&'static str, crate::Meters)>,
    checkpoints: Vec<lash_kernel_state::semantic::Checkpoint>,
    events: Vec<crate::Weight>,
}

impl Observed {
    /// What a run shows its embedder, whatever its slices: result,
    /// prints, effects and the charge events in order.
    fn outcome(&self) -> (&str, &[Datum], &[Request], &[crate::Weight]) {
        (&self.end, &self.printed, &self.requests, &self.events)
    }
}

fn observe(source: &str, slice: u64, bounds: Bounds, taped: bool) -> Observed {
    observe_reopening(source, slice, bounds, taped, false)
}

/// Runs `source` with every park reopened cold, and every finite slice
/// too when `cold_slices`.
fn observe_reopening(
    source: &str,
    slice: u64,
    bounds: Bounds,
    taped: bool,
    cold_slices: bool,
) -> Observed {
    struct Context<'a>(&'a str, u64, Bounds, bool);
    impl Drop for Context<'_> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                eprintln!(
                    "call observation: source={}, slice={}, bounds={:?}, taped={}",
                    self.0, self.1, self.2, self.3
                );
            }
        }
    }
    let _context = Context(source, slice, bounds, taped);
    let prepared = CALL_LIBRARY.with(Clone::clone);
    let mut embedder = Embedder::try_over(
        source,
        Setup {
            slice,
            bounds,
            probes: Some(helper_probes),
            ..Setup::default()
        },
        Some(prepared),
    )
    .expect("the call fixture starts");
    let tape = crate::ChargeTape::new();
    if taped {
        embedder.machine.record_charges(tape.clone());
    }
    let (mut stops, mut checkpoints) = (Vec::new(), Vec::new());

    let end = loop {
        let step = embedder.run();
        stops.push((
            match &step {
                Step::Ended(_) => "ended",
                Step::Slice => "slice",
                Step::Parked(_) => "parked",
            },
            embedder.machine.meters(),
        ));
        match step {
            Step::Ended(End::Finished(finished)) => {
                break format!("finished {:?}", finished.result);
            }
            Step::Ended(other) => break format!("{other:?}"),
            Step::Slice => {
                if cold_slices {
                    checkpoints.push(embedder.machine.semantic_checkpoint());
                    embedder.reopen(true);
                    if taped {
                        embedder.machine.record_charges(tape.clone());
                    }
                }
            }
            Step::Parked(_) => {
                checkpoints.push(embedder.machine.semantic_checkpoint());
                embedder.reopen(true);
                if taped {
                    embedder.machine.record_charges(tape.clone());
                }
                embedder.deliver_next();
            }
        }
    };
    Observed {
        end,
        printed: embedder.world.printed,
        requests: embedder.requests,
        stops,
        checkpoints,
        events: crate::Weight::expand(tape.steps()),
    }
}

/// K-EVAL-008 / K-CHG-001 / K-MACH-008: pushing a frame inside its call
/// instruction preserves every completed operand across an admission
/// refusal or parked callee. Effectful operands keep their source evaluation
/// order; alternating closures retain their own cells. Values, effects and
/// charge events agree whole and sealed cold at every finite slice,
/// including arity, type and missing-binding exits.
#[test]
fn direct_call_entry_agrees_whole_and_cold() {
    let definition = |signature: &str, body: &str| {
        let function = format!(
            "function {signature} -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}"
        );
        format!("definition {}\n", serde_json::to_string(&function).unwrap())
    };
    let definitions = format!(
        "{}{}{}{}{}",
        definition("literal(x: Any)", "return 1"),
        definition("pair(a: Any, b: Any)", "return (a, b)"),
        definition("pause(x: Any)", "do perform 0(x) as Any return x"),
        definition("typed(x: Int)", "return x"),
        definition("maker(a: Any, b?: Any)", "return fn() { return (a, b) }"),
    );

    let mut comparisons = 0;
    for body in [
        "let f = fn(x) { if flat.lt(x, 1) { throw x } return x } let i = 0 while flat.lt(i, 2) { try { print flat.add(7, apply f(i)) } catch err { print err } set i = flat.add(i, 1) } return i",
        SAVED_MEMBER,
        SAVED_MEMBER_PARK,
        SUPERSEDED_RETURN,
        NESTED_RETURN,
        ROOT_CALL_STORAGE,
        NESTED_CALL_STORAGE,
        READ_ADD_WRITE,
        SOURCE_MEMBER,
        SOURCE_CELL,
        SOURCE_SCOPED_BINDINGS,
        SOURCE_CLOSURE_PREFIX,
        SOURCE_BAD_MEMBER,
        SOURCE_CONSTRUCTION,
        SOURCE_SESSION,
        READ_ADD_MISS,
        READ_ADD_FAIL,
        FORMATTED_APPEND,
        FORMATTED_MISS,
        "return invoke helper.element([7], 0)",
        "return invoke helper.element_list_any([7, \"text\"], 1)",
        "return invoke helper.element_list_int([\"wrong element type\"], 0)",
        "return invoke helper.element_map_any(map{\"value\": [7]}, \"value\")",
        "return invoke helper.element_map_int(map{\"value\": \"wrong value type\"}, \"value\")",
        "return invoke helper.element({value: 7}, \"value\")",
        "return invoke helper.element([7], 2)",
        "let f = fn(x) { return 1 } return apply f(7)",
        SOURCE_UNIT,
        SOURCE_PENDING_UNIT,
        "let nested = [7] let f = fn(value) { if same(kind(value), \"list\") { let alias = value } return value } return apply f(nested)",
        "let n = 0 while flat.lt(n, 3) { set n = flat.add(n, call literal(n)) } return n",
        "let n = 7 let f = fn() { return false } try { set n = flat.add(n, apply f()) } catch err { print n } return n",
        "let n = 0 let f = fn() { return 1 } while flat.lt(n, 3) { set n = flat.add(n, apply f()) } return n",
        "let n = 0 let f = fn(x) { set n = flat.add(n, 1) let g = fn(y) { return call pair(y, n) } return apply g(x) } return invoke each.map(f, [1, 2, 3])",
        "let n = 0 let f = fn(x) { set n = flat.add(n, 1) let answer = call pause(x) return call pair(answer, n) } return invoke each.map(f, [1, 2])",
        "let f = fn(x) { try { return call pause(x) } finally { do flat.add(x, 1) } } return invoke each.map(f, [1, 2])",
        // A delayed left cell read must not observe the call's later write.
        "let n = 3 let f = fn() { set n = 20 return 2 } set n = flat.add(n, apply f()) return n",
        // Refusal of a post-call conversion must not replay the call.
        "let n = 0 let f = fn() { set n = flat.add(n, 1) return false } try { set n = flat.add(n, flat.identity(apply f())) } catch err { print n } return n",
        // Conversion and increment keep the captured cell across cold reopen.
        "let n = 0 let f = fn() { let number = flat.identity(n) set n = flat.add(number, 1) do perform 0(n) as Any return n } let first = apply f() let second = apply f() return (first, second, n)",
        "let x = 3 return call pair(x, 4)",
        "let f = fn(a, b) { return (a, b) } return apply f(3, 4)",
        "let f = fn(a, b) { return (a, b) } return apply f(3)",
        // K-EVAL-008: the same physical window binds every missing argument
        // afresh, even after another call supplied it or captured its cell.
        "let f = fn(a, b) { return (a, b) } let first = apply f(3, 9) let second = apply f(4) return (first, second)",
        "let make = fn(a, b) { return fn() { return (a, b) } } let first = apply make(3, 9) let second = apply make(4) return (apply first(), apply second())",
        SOURCE_SHARED_ENTRY,
        // An early return leaves a waiting caller and cleanup state to restore.
        "let n = 0 let f = fn(x) { try { if flat.lt(x, 2) { return call pause(x) } return flat.add(x, 10) } finally { set n = flat.add(n, 1) } } let first = apply f(1) let second = apply f(3) return (first, second, n)",
        "let f = fn(a, b) { return (a, b) } return apply f(1, 2, 3)",
        "let bad = 3 return apply bad(4)",
        "let f = fn(x) { return call typed(x) } return apply f(false)",
        "return call pair(1, call pause(2))",
        "return call pair(call pause(1), 2)",
        "let f = fn(a, b) { return (a, b) } let g = fn() { set f = fn(x) { return 1 } return 8 } return apply f(apply g(), 4)",
        "let f = fn(a, b) { return (a, b) } return apply f(1, missing)",
        "let recurse = fn(self, n) { if flat.lt(n, 1) { return call pause(n) } return flat.add(n, apply self(self, flat.add(n, -1))) } return apply recurse(recurse, 4)",
        "let a = 0 let b = 100 let f = fn(x) { set a = flat.add(a, x) do perform 0(a) as Any return a } let g = fn(x) { set b = flat.add(b, x) return b } let first = apply f(2) let second = apply g(3) return (first, second, apply f(4))",
    ] {
        let source = format!("{definitions}main {{ {body} }}");

        let bounds = super::super::embedder::ROOMY;
        let natural = observe(&source, u64::MAX, bounds, true);
        if body == "let f = fn(x) { return 1 } return apply f(7)" {
            assert_eq!(natural.end, format!("finished {:?}", int(1)));
        }
        if body == SAVED_MEMBER || body == SAVED_MEMBER_PARK {
            assert_eq!(
                natural.end,
                format!(
                    "finished {:?}",
                    Datum::Tuple(vec![
                        Datum::List(vec![int(7), int(2)]),
                        Datum::List(vec![int(9)])
                    ])
                )
            );
        }
        if body == READ_ADD_WRITE {
            assert_eq!(natural.end, format!("finished {:?}", int(256)));
        }
        if body == SOURCE_CLOSURE_PREFIX {
            assert_eq!(natural.end, format!("finished {:?}", int(7)));
        }
        if body == SUPERSEDED_RETURN {
            assert_eq!(
                natural.end,
                format!("finished {:?}", Datum::List(vec![int(9)]))
            );
            assert_eq!(natural.printed, vec![text("cleanup")]);
        }
        if body == SOURCE_BAD_MEMBER {
            assert_eq!(
                natural.end,
                format!("finished {:?}", Datum::List(vec![int(1)]))
            );
            assert_eq!(
                natural.printed,
                vec![Datum::Text("index_out_of_range".into()); 2]
            );
        }
        if body == SOURCE_SESSION {
            assert_eq!(
                natural.end,
                format!(
                    "finished {:?}",
                    Datum::List(vec![Datum::List(vec![int(7)]); 2])
                )
            );
        }
        if body == SOURCE_CONSTRUCTION {
            assert_eq!(
                natural.end,
                format!(
                    "finished {:?}",
                    Datum::List(vec![
                        Datum::List(vec![Datum::List(vec![int(7)]), int(7)]),
                        int(7),
                    ])
                )
            );
        }
        if body == SOURCE_SCOPED_BINDINGS {
            assert_eq!(
                natural.end,
                format!(
                    "finished {:?}",
                    Datum::List(vec![int(7), Datum::List(vec![int(7)])])
                )
            );
        }
        // The charge tape only watches: a run without one ends alike.
        let untaped = observe(&source, u64::MAX, bounds, false);
        assert_eq!(
            (&untaped.end, &untaped.printed, &untaped.requests),
            (&natural.end, &natural.printed, &natural.requests),
            "untaped: {body}"
        );
        comparisons += 1;
    }
    // K-MACH-008 / K-CHG-001: stop after every charged boundary in a
    // callback/capture/return sequence, then rebuild its source frames cold.
    for body in [
        SAVED_MEMBER,
        SAVED_MEMBER_PARK,
        SUPERSEDED_RETURN,
        NESTED_RETURN,
        ROOT_CALL_STORAGE,
        NESTED_CALL_STORAGE,
        READ_ADD_WRITE,
        SOURCE_MEMBER,
        SOURCE_CELL,
        SOURCE_SCOPED_BINDINGS,
        SOURCE_BAD_MEMBER,
        SOURCE_CONSTRUCTION,
        SOURCE_SESSION,
        SOURCE_UNIT,
        SOURCE_PENDING_UNIT,
        READ_ADD_MISS,
        READ_ADD_FAIL,
        FORMATTED_APPEND,
        FORMATTED_MISS,
        "let n = 0 let f = fn(x) { set n = flat.add(n, 1) let saved = call pause(x) return flat.add(saved, n) } return invoke each.map(f, [1, 3])",
    ] {
        let source = format!("{definitions}main {{ {body} }}");
        let whole = observe(&source, u64::MAX, super::super::embedder::ROOMY, true);
        for slice in [1, 2, 3, 5, 11, 31] {
            assert_eq!(
                observe_reopening(&source, slice, super::super::embedder::ROOMY, true, true)
                    .outcome(),
                whole.outcome(),
                "cold source frames at slice {slice}: {body}"
            );
            comparisons += 1;
        }
    }
    println!("direct call entry: {comparisons} paired runs");
}

/// K-CLO-001 / K-FN-004: a repeated value-call site resolves a rebound
/// closure's own body and arity. Closures of one body still use their
/// distinct cells; a different body must not inherit the memo's entry.
#[test]
fn a_direct_memo_resolves_rebound_closures_and_their_captures() {
    let cases = [
        (
            "let cells = [0, 100] let f = fn(x) { set cells[0] = flat.add(cells[0], x) return cells[0] } let other = fn(x) { set cells[1] = flat.add(cells[1], flat.add(x, 10)) return cells[1] } let answers = [] let i = 0 while flat.lt(i, 3) { if same(i, 1) { set f = other } set answers[list.len(answers)] = apply f(1) set i = flat.add(i, 1) } return (answers, cells)",
            Datum::Tuple(vec![
                Datum::List(vec![int(1), int(111), int(122)]),
                Datum::List(vec![int(1), int(122)]),
            ]),
        ),
        (
            "let make = fn(n) { return fn(x) { set n = flat.add(n, x) return n } } let left = apply make(1) let right = apply make(100) let f = left let answers = [] let i = 0 while flat.lt(i, 4) { if same(i, 1) { set f = right } if same(i, 2) { set f = left } set answers[list.len(answers)] = apply f(1) set i = flat.add(i, 1) } return answers",
            Datum::List(vec![int(2), int(101), int(3), int(4)]),
        ),
    ];
    for (body, expected) in cases {
        let source = format!("main {{ {body} }}");
        let before = crate::vm::calls::hits();
        let fast = observe(&source, u64::MAX, super::super::embedder::ROOMY, false);
        assert!(
            crate::vm::calls::hits() > before,
            "a repeated closure was entered"
        );
        assert_eq!(fast.end, format!("finished {expected:?}"));
        let plain = crate::vm::calls::without_memo(|| {
            observe(&source, u64::MAX, super::super::embedder::ROOMY, false)
        });
        assert_eq!(fast.outcome(), plain.outcome());
        assert_eq!(fast.stops, plain.stops);
        for limit in 0..=fast.stops.last().unwrap().1.charged.unwrap() {
            let bounds = Bounds {
                charge_limit: limit,
                ..super::super::embedder::ROOMY
            };
            let fast = observe(&source, u64::MAX, bounds, false);
            let plain =
                crate::vm::calls::without_memo(|| observe(&source, u64::MAX, bounds, false));
            assert_eq!(
                fast.outcome(),
                plain.outcome(),
                "charge limit {limit}: {body}"
            );
            assert_eq!(fast.stops, plain.stops, "charge limit {limit}: {body}");
        }
    }

    // The second trip refuses before the new closure's body can write.
    let source = "main { let cells = [0, 100] let f = fn(x) { set cells[0] = x return x } let other = fn() { set cells[1] = 999 return 999 } let i = 0 while flat.lt(i, 2) { if same(i, 1) { set f = other } print cells print apply f(7) set i = flat.add(i, 1) } return cells }";
    let fast = observe(source, u64::MAX, super::super::embedder::ROOMY, false);
    assert_eq!(
        fast.printed,
        vec![
            Datum::List(vec![int(0), int(100)]),
            int(7),
            Datum::List(vec![int(7), int(100)])
        ]
    );
    let plain = crate::vm::calls::without_memo(|| {
        observe(source, u64::MAX, super::super::embedder::ROOMY, false)
    });
    assert_eq!(fast.outcome(), plain.outcome());
    assert_eq!(fast.stops, plain.stops);
    assert!(fast.end.contains("arity"), "{}", fast.end);
    assert!(
        fast.end.contains("Main"),
        "the caller owns the refusal: {}",
        fast.end
    );
}

/// K-CHG-001 / K-MACH-002: failed prepayment resumes the real callee
/// before its next unpaid op, preserving captures and the exact refusal.
#[test]
fn a_leaf_that_cannot_prepay_keeps_plain_results_refusals_and_charge() {
    for (body, answers) in [
        (
            "set last = x return x",
            Datum::List((0..6).map(int).collect()),
        ),
        ("set last = x", Datum::List(vec![Datum::Null; 6])),
    ] {
        let source = format!(
            "main {{ let last = 0 let f = fn(x) {{ {body} }} let answers = [] let i = 0 while flat.lt(i, 6) {{ set answers[list.len(answers)] = apply f(i) set i = flat.add(i, 1) }} return (answers, last) }}"
        );
        let roomy = super::super::embedder::ROOMY;
        let before = crate::vm::calls::leaves();
        let fast = observe(&source, u64::MAX, roomy, false);
        assert!(
            crate::vm::calls::leaves() > before,
            "the authored f returned through its real-frame leaf path"
        );
        let expected = Datum::Tuple(vec![answers, int(5)]);
        assert_eq!(fast.end, format!("finished {expected:?}"));
        let plain = crate::vm::calls::without_memo(|| observe(&source, u64::MAX, roomy, false));
        assert_eq!(fast.outcome(), plain.outcome());
        assert_eq!(fast.stops, plain.stops);
        let charge = plain.stops.last().unwrap().1.charged.unwrap();
        for limit in 0..=charge {
            let bounds = Bounds {
                charge_limit: limit,
                ..roomy
            };
            let fast = observe(&source, u64::MAX, bounds, false);
            let plain =
                crate::vm::calls::without_memo(|| observe(&source, u64::MAX, bounds, false));
            assert_eq!(fast.outcome(), plain.outcome(), "charge limit {limit}");
            assert_eq!(fast.stops, plain.stops, "charge limit {limit}");
        }
        for charge in [crate::Charge::Exact, crate::Charge::Off] {
            let bounds = Bounds { charge, ..roomy };
            for slice in [1, 3, 17, 101] {
                let fast = observe_reopening(&source, slice, bounds, false, true);
                let plain = crate::vm::calls::without_memo(|| {
                    observe_reopening(&source, slice, bounds, false, true)
                });
                assert_eq!(fast.outcome(), plain.outcome(), "{charge:?} slice {slice}");
                assert_eq!(fast.stops, plain.stops, "{charge:?} slice {slice}");
                assert_eq!(
                    fast.checkpoints, plain.checkpoints,
                    "{charge:?} slice {slice}"
                );
            }
        }
    }
}
