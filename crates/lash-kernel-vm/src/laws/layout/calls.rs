//! Call-owned frame entry preserves source operands and continuations.

use super::*;

const SAVED_MEMBER: &str = "let container = [7] let change = fn() { set container[0] = 9 return 2 } let pair = fn(a, b) { return [a, b] } let answer = apply pair(container[0], apply change()) return (answer, container)";
const SAVED_MEMBER_PARK: &str = "let container = [7] let change = fn() { set container[0] = 9 do perform 0(\"changed\") as Any return 2 } let pair = fn(a, b) { return [a, b] } let answer = apply pair(invoke helper.element(container, 0), apply change()) return (answer, container)";
const SUPERSEDED_RETURN: &str = "let cleanup = fn() { try { return [7] } finally { print \"cleanup\" return [9] } } return apply cleanup()";

const NESTED_RETURN: &str = "let cleanup = fn() { try { return [7] } finally { try { return [8] } finally { print \"nested cleanup\" return [9] } } } return apply cleanup()";

const READ_ADD_WRITE: &str = "let run = fn(obj) { let total = 1 set total = flat.add(total, invoke helper.element_map_any(obj, \"a\")) return total } return apply run(map{\"a\": 255})";
const READ_ADD_MISS: &str = "let run = fn(obj) { let total = 1 set total = flat.add(total, invoke helper.element_map_any(obj, \"missing\")) return total } return apply run(map{\"a\": 255})";
const READ_ADD_FAIL: &str = "let run = fn(obj) { let total = false set total = flat.add(total, invoke helper.element_map_any(obj, \"a\")) return total } return apply run(map{\"a\": 255})";

const FORMATTED_APPEND: &str = "let build = fn(n) { let text = \"\" let i = 0 while flat.lt(i, n) { set text = text.concat(text, text.join([\"Tool \", num.to_text(i), \" value=\", num.to_text(flat.mul(i, 3)), \"; status=ready\\n\"], \"\")) set i = flat.add(i, 1) } return text } let answer = apply build(3) do perform 0(answer) as Any return answer";

const FORMATTED_MISS: &str = "let build = fn() { let text = \"old\" set text = text.concat(text, text.join([\"field=\", 1, \"; suffix\"], \"\")) return text } return apply build()";

thread_local! {
    static CHECKPOINT_EXITS: std::cell::RefCell<crate::machine::islands::witness::Counts> = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
    // PreparedLibrary is an immutable, shareable fixture. Cold reopen still
    // constructs its own library in Embedder::reopen.
    static CALL_LIBRARIES: std::cell::RefCell<std::collections::BTreeMap<u64, crate::PreparedLibrary>> = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

use super::ExecutionMode as Mode;

#[derive(Debug, PartialEq)]
struct Observed {
    end: String,
    printed: Vec<Datum>,
    requests: Vec<Request>,
    stops: Vec<(&'static str, crate::Meters)>,
    checkpoints: Vec<lash_kernel_state::semantic::Checkpoint>,
    events: Vec<lash_kernel_exec::Weight>,
}

fn observe(source: &str, layout: Layout, slice: u64, bounds: Bounds, taped: bool) -> Observed {
    observe_reopening(source, layout, slice, bounds, taped, false)
}

fn observe_reopening(
    source: &str,
    layout: Layout,
    slice: u64,
    bounds: Bounds,
    taped: bool,
    cold_slices: bool,
) -> Observed {
    observe_source_or_register(
        source,
        layout,
        slice,
        bounds,
        taped,
        cold_slices,
        Mode::Specialized,
    )
}

fn observe_source_or_register(
    source: &str,
    layout: Layout,
    slice: u64,
    bounds: Bounds,
    taped: bool,
    cold_slices: bool,
    mode: Mode,
) -> Observed {
    crate::machine::islands::witness::begin();
    let prepared = CALL_LIBRARIES.with_borrow_mut(|libraries| {
        libraries
            .entry(layout.0)
            .or_insert_with(|| {
                let fixture = super::super::embedder::library_with(true, Some(helper_probes));
                crate::PreparedLibrary::with_layout(
                    std::sync::Arc::clone(&fixture.registry),
                    layout,
                )
            })
            .clone()
    });
    let mut embedder = Embedder::try_over(
        source,
        Setup {
            layout,
            slice,
            bounds,
            probes: Some(helper_probes),
            ..Setup::default()
        },
        Some(prepared),
    )
    .expect("the call fixture starts");
    if source.contains("helper.element") && layout == Layout(1) {
        embedder.machine.count_work();
    }
    let tape = crate::ChargeTape::new();
    if taped {
        embedder.machine.record_charges(tape.clone());
    }
    let (mut stops, mut checkpoints) = (Vec::new(), Vec::new());
    let mut register_statements = 0;
    let mut slab_calls = 0;
    let (mut packets, mut specialized) = (0, 0);
    let end = loop {
        if !taped {
            embedder.machine.count_execution();
        }
        match mode {
            Mode::Source => embedder.machine.force_source_execution(),
            Mode::Generic => embedder.machine.force_generic_packets(),
            Mode::Specialized => {}
        }
        let step = embedder.run();
        let (dispatched, fused) = embedder.machine.take_packet_counts();
        packets += dispatched;
        specialized += fused;
        slab_calls += embedder.machine.take_slab_calls();
        assert!(
            embedder.machine.execution_storage_released(),
            "K-BND-003: a source boundary retains no physical slab holdings: {source}"
        );
        if !taped {
            register_statements += embedder
                .machine
                .take_execution_census()
                .statements
                .get(&crate::perf_witness::execution::Owner::RegisterProgram)
                .copied()
                .unwrap_or(0);
        }
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
                    embedder.reopen(layout, true);
                    if taped {
                        embedder.machine.record_charges(tape.clone());
                    }
                }
            }
            Step::Parked(_) => {
                checkpoints.push(embedder.machine.semantic_checkpoint());
                embedder.reopen(layout, true);
                if source.contains("helper.element") && layout == Layout(1) {
                    embedder.machine.count_work();
                }
                if taped {
                    embedder.machine.record_charges(tape.clone());
                }
                embedder.deliver_next();
            }
        }
    };
    if mode == Mode::Source {
        assert_eq!(packets, 0, "the source oracle cannot dispatch packets");
    } else if !taped && bounds == super::super::embedder::ROOMY {
        assert!(
            packets > 0,
            "the call law must exercise packet dispatch: {source}"
        );
    }
    if mode == Mode::Generic {
        assert_eq!(
            specialized, 0,
            "forced generic execution entered a specialized packet: {source}"
        );
    }
    if !taped && mode == Mode::Source {
        assert_eq!(slab_calls, 0, "the source oracle cannot place slab calls");
        assert_eq!(
            register_statements, 0,
            "the source oracle cannot enter register code"
        );
    } else if !taped && bounds == super::super::embedder::ROOMY {
        assert!(
            register_statements > 0,
            "paired call cases must run register statements: {source}"
        );
        if slice == u64::MAX
            && (source.contains(SAVED_MEMBER)
                || source.contains(SAVED_MEMBER_PARK)
                || source.contains(SUPERSEDED_RETURN)
                || source.contains(NESTED_RETURN))
        {
            assert!(
                slab_calls > 0,
                "the recovery examples must exercise direct slab placement: {source}"
            );
        }
    }
    let witnesses = crate::machine::islands::witness::take();
    if mode != Mode::Specialized {
        assert!(witnesses.is_empty());
    }
    if mode == Mode::Specialized
        && slice == u64::MAX
        && !taped
        && layout == Layout(0)
        && bounds == super::super::embedder::ROOMY
        && source.contains(READ_ADD_WRITE)
    {
        use crate::machine::islands::witness::{Exit, Family};
        assert!(
            witnesses
                .get(&(Family::ReadUpdate, Exit::Commit))
                .copied()
                .unwrap_or(0)
                > 0,
            "the read/add/write checkpoint must commit its complete family: {witnesses:?}"
        );
        println!("read/add/write checkpoint exits: {witnesses:?}");
    }
    if mode == Mode::Specialized
        && layout == Layout(0)
        && slice == u64::MAX
        && !taped
        && bounds == super::super::embedder::ROOMY
        && source.contains(FORMATTED_APPEND)
    {
        use crate::machine::islands::witness::{Exit, Family};
        assert!(
            witnesses
                .get(&(Family::FormatAppend, Exit::Commit))
                .copied()
                .unwrap_or(0)
                > 0,
            "the formatting checkpoint must execute its allocating family: {witnesses:?}"
        );
        println!("format/append checkpoint exits: {witnesses:?}");
    }
    if mode == Mode::Specialized
        && layout == Layout(0)
        && [
            READ_ADD_WRITE,
            READ_ADD_MISS,
            READ_ADD_FAIL,
            FORMATTED_APPEND,
            FORMATTED_MISS,
        ]
        .iter()
        .any(|body| source.contains(body))
    {
        CHECKPOINT_EXITS.with_borrow_mut(|counts| {
            for (event, count) in witnesses {
                *counts.entry(event).or_default() += count;
            }
        });
    }
    Observed {
        end,
        printed: embedder.world.printed,
        requests: embedder.requests,
        stops,
        checkpoints,
        events: lash_kernel_exec::Weight::expand(tape.steps()),
    }
}

/// K-EVAL-008 / K-CHG-001 / K-MACH-008: pushing a frame inside its call
/// instruction preserves every completed operand across an admission
/// refusal or parked callee. Effectful operands keep their source evaluation
/// order; alternating closures retain their own cells. Values, charge events,
/// finite stop points, held bytes and parked images agree with the general
/// layout, including arity, type, depth, memory and missing-binding exits.
#[test]
fn direct_call_entry_agrees() {
    CHECKPOINT_EXITS.with_borrow_mut(|counts| counts.clear());
    let definition = |signature: &str, body: &str| {
        let function = format!(
            "function {signature} -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}"
        );
        format!("definition {}\n", serde_json::to_string(&function).unwrap())
    };
    let definitions = format!(
        "{}{}{}{}",
        definition("literal(x: Any)", "return 1"),
        definition("pair(a: Any, b: Any)", "return (a, b)"),
        definition("pause(x: Any)", "do perform 0(x) as Any return x"),
        definition("typed(x: Int)", "return x"),
    );
    // K-CHG-001 / K-BND-003: a refused statement unit retains updated locals.
    {
        let source = format!("{definitions}main {{ {FORMATTED_APPEND} }}");
        let bounds = Bounds {
            charge_limit: 157,
            ..super::super::embedder::ROOMY
        };
        let reference = observe_source_or_register(
            &source,
            Layout(0),
            u64::MAX,
            bounds,
            false,
            false,
            Mode::Source,
        );
        let generic = observe_source_or_register(
            &source,
            Layout(0),
            u64::MAX,
            bounds,
            false,
            false,
            Mode::Generic,
        );
        assert_eq!(
            generic, reference,
            "a refused statement unit keeps the completed text local"
        );
        println!(
            "refused statement retains updated source locals: {:?}",
            generic.stops
        );
    }
    for body in [SAVED_MEMBER, SUPERSEDED_RETURN, NESTED_RETURN] {
        placement_witness(&format!("{definitions}main {{ {body} }}"), body);
    }
    let mut comparisons = 0;
    for body in [
        SAVED_MEMBER,
        SAVED_MEMBER_PARK,
        SUPERSEDED_RETURN,
        NESTED_RETURN,
        READ_ADD_WRITE,
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
        if [READ_ADD_WRITE, FORMATTED_APPEND].contains(&body) {
            verify_checkpoint_maps(&source);
        }
        let bounds = super::super::embedder::ROOMY;
        let reference = observe(&source, Layout(1), u64::MAX, bounds, true);
        let natural = observe(&source, Layout(0), u64::MAX, bounds, true);
        assert_eq!(natural, reference, "{body}");
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
        if body == SUPERSEDED_RETURN {
            assert_eq!(
                natural.end,
                format!("finished {:?}", Datum::List(vec![int(9)]))
            );
            assert_eq!(natural.printed, vec![text("cleanup")]);
        }
        let plain = observe(&source, Layout(0), u64::MAX, bounds, false);
        assert_eq!(
            plain,
            observe_source_or_register(
                &source,
                Layout(0),
                u64::MAX,
                bounds,
                false,
                false,
                Mode::Generic,
            ),
            "generic packet calls: {body}"
        );
        if body == SAVED_MEMBER
            || body == SAVED_MEMBER_PARK
            || body == SUPERSEDED_RETURN
            || body == NESTED_RETURN
            || body == READ_ADD_WRITE
            || body == READ_ADD_MISS
            || body == READ_ADD_FAIL
            || body == FORMATTED_APPEND
            || body == FORMATTED_MISS
        {
            let source_only = observe_source_or_register(
                &source,
                Layout(0),
                u64::MAX,
                bounds,
                false,
                false,
                Mode::Source,
            );
            assert_eq!(plain, source_only, "source oracle: {body}");
        }
        assert_eq!(plain.end, reference.end, "untaped result: {body}");
        assert_eq!(
            plain,
            observe(&source, Layout(1), u64::MAX, bounds, false),
            "untaped: {body}"
        );
        for slice in [1, 2, 3, 5, 11, 31] {
            assert_eq!(
                observe(&source, Layout(0), slice, bounds, true),
                observe(&source, Layout(1), slice, bounds, true),
                "slice {slice}: {body}"
            );
            comparisons += 1;
        }
        let charged = reference.stops.last().unwrap().1.charged.unwrap();
        for limit in 0..charged {
            let bounds = Bounds {
                charge_limit: limit,
                ..bounds
            };
            assert_eq!(
                observe(&source, Layout(0), u64::MAX, bounds, true),
                observe(&source, Layout(1), u64::MAX, bounds, true),
                "limit {limit}: {body}"
            );
            comparisons += 1;
        }
        if [READ_ADD_WRITE, FORMATTED_APPEND, FORMATTED_MISS].contains(&body) {
            let mut refused = false;
            for memory in [512, 1024, 2048, 4096, 8192] {
                let limits = Bounds {
                    memory,
                    ..super::super::embedder::ROOMY
                };
                let source_only = observe_source_or_register(
                    &source,
                    Layout(0),
                    u64::MAX,
                    limits,
                    true,
                    false,
                    Mode::Source,
                );
                refused |= source_only.end.contains("Memory");
                for mode in [Mode::Generic, Mode::Specialized] {
                    assert_eq!(
                        observe_source_or_register(
                            &source,
                            Layout(0),
                            u64::MAX,
                            limits,
                            true,
                            false,
                            mode
                        ),
                        source_only,
                        "whole path memory {memory}: {body}"
                    );
                }
                comparisons += 1;
            }
            assert!(
                refused,
                "the checkpoint must exercise an actual source memory refusal"
            );
        }
        if [READ_ADD_WRITE, FORMATTED_APPEND].contains(&body) {
            for limit in 0..charged {
                let limits = Bounds {
                    charge_limit: limit,
                    ..super::super::embedder::ROOMY
                };
                let source_only = observe_source_or_register(
                    &source,
                    Layout(0),
                    u64::MAX,
                    limits,
                    false,
                    false,
                    Mode::Source,
                );
                for mode in [Mode::Generic, Mode::Specialized] {
                    assert_eq!(
                        observe_source_or_register(
                            &source,
                            Layout(0),
                            u64::MAX,
                            limits,
                            false,
                            false,
                            mode
                        ),
                        source_only,
                        "quiet whole path {mode:?} bound {limit}: {body}"
                    );
                }
                comparisons += 1;
            }
        }
        for memory in [8192, 16384] {
            let bounds = Bounds { memory, ..bounds };
            assert_eq!(
                observe(&source, Layout(0), u64::MAX, bounds, true),
                observe(&source, Layout(1), u64::MAX, bounds, true),
                "memory {memory}: {body}"
            );
            comparisons += 1;
        }
        let bounds = Bounds {
            call_depth: 3,
            ..bounds
        };
        assert_eq!(
            observe(&source, Layout(0), u64::MAX, bounds, true),
            observe(&source, Layout(1), u64::MAX, bounds, true),
            "depth: {body}"
        );
    }
    // K-MACH-008 / K-CHG-001: stop after every charged boundary in a
    // callback/capture/return sequence, then rebuild its source frames cold.
    for body in [
        SAVED_MEMBER,
        SAVED_MEMBER_PARK,
        SUPERSEDED_RETURN,
        NESTED_RETURN,
        READ_ADD_WRITE,
        READ_ADD_MISS,
        READ_ADD_FAIL,
        FORMATTED_APPEND,
        FORMATTED_MISS,
        "let n = 0 let f = fn(x) { set n = flat.add(n, 1) let saved = call pause(x) return flat.add(saved, n) } return invoke each.map(f, [1, 3])",
    ] {
        let source = format!("{definitions}main {{ {body} }}");
        for slice in [1, 2, 3, 5, 11, 31] {
            let natural = observe_reopening(
                &source,
                Layout(0),
                slice,
                super::super::embedder::ROOMY,
                true,
                true,
            );
            assert_eq!(
                natural,
                observe_reopening(
                    &source,
                    Layout(1),
                    slice,
                    super::super::embedder::ROOMY,
                    true,
                    true
                ),
                "cold source frames at slice {slice}: {body}"
            );
            if body == SAVED_MEMBER
                || body == SAVED_MEMBER_PARK
                || body == SUPERSEDED_RETURN
                || body == NESTED_RETURN
                || body == READ_ADD_WRITE
                || body == READ_ADD_MISS
                || body == READ_ADD_FAIL
                || body == FORMATTED_APPEND
                || body == FORMATTED_MISS
            {
                assert_eq!(
                    natural,
                    observe_source_or_register(
                        &source,
                        Layout(0),
                        slice,
                        super::super::embedder::ROOMY,
                        true,
                        true,
                        Mode::Generic,
                    ),
                    "cold generic packets at slice {slice}: {body}"
                );
                assert_eq!(
                    natural,
                    observe_source_or_register(
                        &source,
                        Layout(0),
                        slice,
                        super::super::embedder::ROOMY,
                        true,
                        true,
                        Mode::Source,
                    ),
                    "cold source oracle at slice {slice}: {body}"
                );
            }
            comparisons += 1;
        }
    }
    println!("direct call entry: {comparisons} paired runs");
    CHECKPOINT_EXITS.with_borrow(|counts| {
        use crate::machine::islands::witness::{Exit, Family};
        for (family, exits) in [
            (
                Family::ReadUpdate,
                &[Exit::Commit, Exit::Guard, Exit::Partial][..],
            ),
            (
                Family::FormatAppend,
                &[Exit::Commit, Exit::Prefix, Exit::Partial][..],
            ),
        ] {
            for exit in exits {
                assert!(
                    counts.get(&(family, *exit)).copied().unwrap_or(0) > 0,
                    "the checkpoint must reach {family:?}/{exit:?}: {counts:?}"
                );
            }
        }
        println!("whole-path checkpoint exits: {counts:?}");
    });
}

fn placement_witness(source: &str, body: &str) {
    let mut embedder = Embedder::with(
        source,
        Setup {
            probes: Some(helper_probes),
            ..Setup::default()
        },
    );
    embedder.machine.trace_slab_placements();
    let mut placements = Vec::new();
    loop {
        let step = embedder.run();
        placements.extend(embedder.machine.take_slab_placements());
        assert!(embedder.machine.execution_storage_released());
        match step {
            Step::Ended(End::Finished(_)) => break,
            Step::Ended(other) => panic!("placement example failed: {other:?}"),
            Step::Slice => {}
            Step::Parked(_) => embedder.deliver_next(),
        }
    }
    assert!(!placements.is_empty(), "the slab must execute");
    for stack in &placements {
        for pair in stack.windows(2) {
            assert!(
                pair[0].pending.end <= pair[1].bindings.start,
                "live frames cannot overlap: {stack:?}"
            );
        }
        for frame in stack {
            assert_eq!(frame.bindings.end, frame.operands.start);
            assert_eq!(frame.operands.end, frame.pending.start);
            for live in &frame.retained {
                assert_eq!(live.at, frame.operands.start + live.role as usize);
                assert!(frame.operands.contains(&live.at));
            }
            for departure in &frame.departures {
                assert!(frame.pending.contains(&departure.at));
            }
        }
    }
    let saved = placements.iter().find(|stack| {
        stack.iter().any(|frame| {
            frame.waiting && frame.retained.iter().any(|live| live.value == Some(int(7)))
        })
    });
    if body == SAVED_MEMBER {
        let saved = saved.expect("a suspended caller retains argument 7 in its own slab range");
        println!("saved argument slab placement: {saved:?}");
        let completed = placements
            .iter()
            .rfind(|stack| {
                stack.iter().any(|frame| {
                    frame.waiting
                        && frame.retained.iter().any(|live| live.value == Some(int(7)))
                        && frame.retained.iter().any(|live| live.value == Some(int(2)))
                })
            })
            .unwrap();
        println!("placed call after mutation: {completed:?}");
        println!(
            "source/packet coordinates: {:?}",
            completed
                .iter()
                .map(|frame| (frame.code, frame.packet))
                .collect::<Vec<_>>()
        );
    } else {
        let simultaneous = placements.iter().find(|stack| {
            stack.iter().any(|frame| {
                frame
                    .departures
                    .iter()
                    .any(|held| held.value == Some(Datum::List(vec![int(7)])))
                    && frame
                        .retained
                        .iter()
                        .any(|live| live.value == Some(Datum::List(vec![int(9)])))
            })
        });
        let simultaneous =
            simultaneous.expect("the pending [7] outlives evaluation of replacement [9]");
        println!("superseded return slab placement: {simultaneous:?}");
        if body == NESTED_RETURN {
            assert!(
                placements.iter().any(|stack| stack.iter().any(|frame| {
                    let a = frame
                        .departures
                        .iter()
                        .find(|held| held.value == Some(Datum::List(vec![int(7)])));
                    let b = frame
                        .departures
                        .iter()
                        .find(|held| held.value == Some(Datum::List(vec![int(8)])));
                    matches!((a, b), (Some(a), Some(b)) if a.at != b.at)
                })),
                "nested pending departures have distinct owner lifetimes"
            );
        }
    }
}

fn verify_checkpoint_maps(source: &str) {
    let (program, _) = Embedder::with(
        source,
        Setup {
            probes: Some(helper_probes),
            ..Setup::default()
        },
    )
    .program(true);
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("the checkpoint source is admitted"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("the checkpoint source compiles"));
    let mut checked = 0;
    for plan in &exe.islands.plans {
        let Some(mut code) = crate::machine::islands::Compiled::of(plan, &exe) else {
            continue;
        };
        assert_eq!(code.verify_family_projections(plan, &exe), Ok(()));
        if code.corrupt_family_roles() {
            assert_eq!(
                code.verify_family_projections(plan, &exe),
                Err(crate::compile::islands::Refused::Calls),
                "a family cannot omit a committed prefix's source roles"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "the checkpoint must form a verified family");
}
