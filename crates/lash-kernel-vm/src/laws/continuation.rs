//! Continuation laws (FIG-6417): a task parked at an action nested in
//! its statement's expression keeps the operands it already evaluated as
//! named roles, resumes through the layout's resume entry, and leaves the
//! same trace as a run that never parked (`K-EVAL-006`).

use super::embedder::{self, Embedder, Setup, label};
use crate::{Bounds, End, Layout, Machine, Step};
use lash_kernel_doc::{Datum, TaskId, Value};
use lash_kernel_state::paged::AllocationCensus;
use lash_kernel_state::paged::directory::ordinal_key;
use lash_kernel_state::paged::tree::Tree;
use lash_kernel_state::paged::{
    IncomingRecord, PendingRecord, PhaseRecord, RoleRecord, TaskStateRecord,
};

/// What a run leaves observable: its end, what it printed, every request
/// it handed out, its charged total, and every statement it ran.
#[derive(Debug, PartialEq)]
struct Trace {
    end: String,
    printed: Vec<String>,
    requests: Vec<String>,
    charged: u64,
    statements: Vec<String>,
}

fn trace_of(embedder: &Embedder, end: &End, statements: &[String]) -> Trace {
    Trace {
        end: format!("{end:?}"),
        printed: embedder
            .world
            .printed
            .iter()
            .map(|datum| format!("{datum:?}"))
            .collect(),
        requests: embedder
            .requests
            .iter()
            .map(|request| format!("{request:?}"))
            .collect(),
        charged: embedder.machine.meters().charged,
        statements: statements.to_vec(),
    }
}

/// A run that never parks its machine: the reference every reopened run
/// must match.
fn uninterrupted(text: &str, order: &[&str]) -> Trace {
    run_with(text, order, u64::MAX, false).0
}

/// Runs `text` in `slice`-charge segments; `reopens` says whether the
/// machine is checkpointed and reopened at every slice and park, under
/// the next layout and alternating `native_twice`. Gives the trace and
/// the continuation phases seen just before each checkpoint.
fn run_with(text: &str, order: &[&str], slice: u64, reopens: bool) -> (Trace, Vec<PhaseRecord>) {
    let mut embedder = Embedder::with(
        text,
        Setup {
            slice,
            ..Setup::default()
        },
    );
    embedder.machine.trace_statements();
    let mut statements = Vec::new();
    let mut phases = Vec::new();
    let mut order = order.iter();
    let mut hops = 0_u64;
    let end = loop {
        let step = embedder.run();
        statements.extend(
            embedder
                .machine
                .take_statement_trace()
                .iter()
                .map(|site| site.to_string()),
        );
        match step {
            Step::Ended(end) => break end,
            Step::Slice | Step::Parked(_) => {
                if reopens {
                    hops += 1;
                    phases.extend(
                        embedder
                            .machine
                            .pending_phases()
                            .iter()
                            .map(|(_, _, phase)| *phase),
                    );
                    // Odd layouts most hops, an even one every other,
                    // and `pair.twice` alternating native and body.
                    let layout = if hops.is_multiple_of(2) {
                        Layout(hops.wrapping_mul(0x9e37_79b9) & !1)
                    } else {
                        Layout(hops.wrapping_mul(0x9e37_79b9) | 1)
                    };
                    embedder.reopen(layout, hops.is_multiple_of(2));
                    embedder.machine.trace_statements();
                }
            }
        }
        if matches!(step, Step::Parked(_)) {
            let next = match order.next() {
                Some(name) => (*name).to_string(),
                None => label(
                    embedder
                        .pending
                        .first()
                        .expect("a park with nothing pending"),
                ),
            };
            embedder.deliver(&next);
        }
    };
    (trace_of(&embedder, &end, &statements), phases)
}

/// The exactness check of one case: a run that checkpoints and reopens
/// at every safe point, under another layout and a fresh library each
/// hop, must leave the run the uninterrupted one leaves. The slice-0
/// run must see a parked call and a parked wait.
fn case(text: &str, order: &[&str], expected: &Datum, printed: &[Datum], want: &[PhaseRecord]) {
    let reference = uninterrupted(text, order);
    let (end, _) = embedder::run(text);
    assert_eq!(
        embedder::result(end),
        *expected,
        "the case finished with its expected value"
    );
    assert_eq!(
        reference.printed,
        printed
            .iter()
            .map(|datum| format!("{datum:?}"))
            .collect::<Vec<_>>()
    );
    let mut phases = Vec::new();
    for slice in [u64::MAX, 1, 0] {
        let (trace, seen) = run_with(text, order, slice, true);
        assert_eq!(trace, reference, "reopened at slice {slice}");
        phases.extend(seen);
    }
    for phase in want {
        assert!(phases.contains(phase), "checkpoints saw {phases:?}");
    }
}

fn text(datum: &str) -> Datum {
    Datum::Text(datum.to_string())
}

fn int(datum: i64) -> Datum {
    embedder::int(datum)
}

fn list(datum: &[Datum]) -> Datum {
    Datum::List(datum.to_vec())
}

fn tuple(datum: &[Datum]) -> Datum {
    Datum::Tuple(datum.to_vec())
}

/// Case 1: calls and a wait nested inside one expression's arguments.
const NESTED_AWAIT: &str = "fn a() { print \"a\" return 1 }\n\
    fn c() { let v = perform echo(\"c\") as Any print \"c\" return v }\n\
    fn observe(p, q, r) { print (p, q, r) return [p, q, r] }\n\
    main { let x = \"tool\"\n\
    let r = call observe(call a(), perform echo(x) as Any, call c())\n\
    return r }\n";

const NESTED_AWAIT_HOISTED: &str = "fn a() { print \"a\" return 1 }\n\
    fn c() { let v = perform echo(\"c\") as Any print \"c\" return v }\n\
    fn observe(p, q, r) { print (p, q, r) return [p, q, r] }\n\
    main { let x = \"tool\"\n\
    let t0 = call a()\n\
    let t1 = perform echo(x) as Any\n\
    let t2 = call c()\n\
    let r = call observe(t0, t1, t2)\n\
    return r }\n";

#[test]
fn nested_await_resumes_exactly() {
    let expected = list(&[int(1), text("tool"), text("c")]);
    let printed = [
        text("a"),
        text("c"),
        tuple(&[int(1), text("tool"), text("c")]),
    ];
    case(
        NESTED_AWAIT,
        &["tool", "c"],
        &expected,
        &printed,
        &[PhaseRecord::Called, PhaseRecord::Waiting],
    );
    case(
        NESTED_AWAIT_HOISTED,
        &["tool", "c"],
        &expected,
        &printed,
        &[],
    );
}

/// Case 2: a role holds the value its operand produced when it was
/// evaluated, while another task mutates the receiver during the park.
const MUTATED_RECEIVER: &str = "fn writer(o) { do perform echo(\"w\") as Any set o.x = 2 return null }\n\
    main { let o = {x: 1}\n\
    let h = spawn call writer(o)\n\
    let r = [o.x, perform echo(\"m\") as Any]\n\
    let done = join h\n\
    return (r, o.x) }\n";

const MUTATED_RECEIVER_HOISTED: &str = "fn writer(o) { do perform echo(\"w\") as Any set o.x = 2 return null }\n\
    main { let o = {x: 1}\n\
    let h = spawn call writer(o)\n\
    let t0 = o.x\n\
    let t1 = perform echo(\"m\") as Any\n\
    let r = [t0, t1]\n\
    let done = join h\n\
    return (r, o.x) }\n";

#[test]
fn parked_role_keeps_the_evaluated_value() {
    let r = list(&[int(1), text("m")]);
    let expected = tuple(&[r, int(2)]);
    case(
        MUTATED_RECEIVER,
        &["w", "m"],
        &expected,
        &[],
        &[PhaseRecord::Waiting],
    );
    case(MUTATED_RECEIVER_HOISTED, &["w", "m"], &expected, &[], &[]);
}

/// Case 3: an async callback inside a sync array op, inside an
/// expression: spawns under a nested call of a nested call.
const CALLBACK: &str = "fn work(x) { let v = [perform echo(x) as Any] return v[0] }\n\
    fn collect(f, xs) { let out = [] for x in xs { let y = apply f(x) set out[list.len(out)] = y } return out }\n\
    fn gather(hs) { let rs = join all hs return rs }\n\
    fn tag(t) { print t return t }\n\
    main { let r = (call tag(\"a\"),\n\
      call gather(call collect(fn(x) { let h = spawn call work(x) return h }, [1, 2])),\n\
      call tag(\"b\"))\n\
    return r }\n";

const CALLBACK_HOISTED: &str = "fn work(x) { let v = perform echo(x) as Any return v }\n\
    fn collect(f, xs) { let out = [] for x in xs { let y = apply f(x) set out[list.len(out)] = y } return out }\n\
    fn gather(hs) { let rs = join all hs return rs }\n\
    fn tag(t) { print t return t }\n\
    main { let t0 = call tag(\"a\")\n\
    let f = fn(x) { let h = spawn call work(x) return h }\n\
    let xs = [1, 2]\n\
    let t1 = call collect(f, xs)\n\
    let t2 = call gather(t1)\n\
    let t3 = call tag(\"b\")\n\
    let r = (t0, t2, t3)\n\
    return r }\n";

#[test]
fn callback_inside_expression_resumes_exactly() {
    let expected = tuple(&[text("a"), list(&[int(1), int(2)]), text("b")]);
    case(
        CALLBACK,
        &["echo", "echo"],
        &expected,
        &[text("a"), text("b")],
        &[PhaseRecord::Called, PhaseRecord::Waiting],
    );
    case(
        CALLBACK_HOISTED,
        &["echo", "echo"],
        &expected,
        &[text("a"), text("b")],
        &[],
    );
}

/// The smallest memory bound under which the run that checkpoints and
/// reopens at every slice still finishes, found by bisection over
/// [low, high]: the bound the reopened runs are tested against. Sealing
/// at each hop allocates inside the same bound, so this bound is found
/// on the reopened run itself.
fn least_memory(text: &str, order: &[&str]) -> u64 {
    let mut low = 1;
    let mut high = 64 << 20;
    let finishes = |memory: u64| {
        let bounds = Bounds {
            memory,
            ..embedder::ROOMY
        };
        let (end, _) = run_with_bounds(text, order, 0, true, bounds);
        end.contains("Finished")
    };
    assert!(!finishes(low), "the low bound already finishes");
    assert!(finishes(high), "the high bound cannot finish");
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if finishes(mid) { high = mid } else { low = mid }
    }
    high
}

#[test]
fn memory_refusal_is_identical_across_reopens() {
    for (source, order) in [
        (NESTED_AWAIT, &["tool", "c"][..]),
        (NESTED_AWAIT_HOISTED, &["tool", "c"][..]),
        (MUTATED_RECEIVER, &["w", "m"][..]),
        (MUTATED_RECEIVER_HOISTED, &["w", "m"][..]),
        (CALLBACK, &["echo", "echo"][..]),
        (CALLBACK_HOISTED, &["echo", "echo"][..]),
    ] {
        let memory = least_memory(source, order);
        let bounds = Bounds {
            memory,
            ..embedder::ROOMY
        };
        let expected = embedder::result(embedder::run(source).0);
        // One byte under the bound the run must refuse, and the same
        // reopened run must refuse identically.
        let under = Bounds {
            memory: memory - 1,
            ..embedder::ROOMY
        };
        let (under_trace, _) = run_with_bounds(source, order, 0, true, under);
        assert!(
            under_trace.contains("Bound") || under_trace.contains("start refused"),
            "the reopened run at {memory} - 1 finished: {under_trace}"
        );
        let (at_trace, _) = run_with_bounds(source, order, 0, true, bounds);
        assert_eq!(
            at_trace,
            format!("Finished({expected:?})"),
            "the reopened run at {memory}"
        );
    }
}

/// P0 measurement, not a law: the least memory bound under which each
/// source finishes uninterrupted (slice u64::MAX, never reopened), sliced
/// at 0 but never reopened, and reopened at every slice; then, one byte
/// under the smallest of the three, what each refusing run leaves: its
/// end, charge, and how many requests and prints it made.
#[test]
#[ignore = "P0 measurement; run with --include-ignored --nocapture"]
fn p0_memory_refusal_points() {
    let least = |text: &str, order: &[&str], slice: u64, reopens: bool| {
        let finishes = |memory: u64| {
            bounded(text, order, slice, reopens, memory)
                .0
                .contains("Finished")
        };
        let (mut low, mut high) = (1_u64, 64_u64 << 20);
        assert!(!finishes(low) && finishes(high));
        while low + 1 < high {
            let mid = low + (high - low) / 2;
            if finishes(mid) { high = mid } else { low = mid }
        }
        high
    };
    for (name, source, order) in [
        ("nested_await", NESTED_AWAIT, &["tool", "c"][..]),
        (
            "nested_await_hoisted",
            NESTED_AWAIT_HOISTED,
            &["tool", "c"][..],
        ),
        ("mutated_receiver", MUTATED_RECEIVER, &["w", "m"][..]),
        (
            "mutated_receiver_hoisted",
            MUTATED_RECEIVER_HOISTED,
            &["w", "m"][..],
        ),
        ("callback", CALLBACK, &["echo", "echo"][..]),
        ("callback_hoisted", CALLBACK_HOISTED, &["echo", "echo"][..]),
    ] {
        let warm = least(source, order, u64::MAX, false);
        let sliced = least(source, order, 0, false);
        let reopened = least(source, order, 0, true);
        let under = warm.min(sliced).min(reopened) - 1;
        let refusals = [
            ("warm", bounded(source, order, u64::MAX, false, under)),
            ("sliced", bounded(source, order, 0, false, under)),
            ("reopened", bounded(source, order, 0, true, under)),
        ];
        println!(
            "P0-MEMORY {name} least_warm={warm} least_sliced={sliced} least_reopened={reopened} under={under}"
        );
        for (mode, (end, charged, requests, printed)) in refusals {
            println!(
                "P0-MEMORY {name} {mode} end={end} charged={charged} requests={requests} printed={printed}"
            );
        }
    }
}

/// One run under `memory`: its end, charge, requests and prints.
fn bounded(
    text: &str,
    order: &[&str],
    slice: u64,
    reopens: bool,
    memory: u64,
) -> (String, u64, usize, usize) {
    let setup = Setup {
        bounds: Bounds {
            memory,
            ..embedder::ROOMY
        },
        slice,
        ..Setup::default()
    };
    let text = format!(
        "numbers {}\n{}{text}",
        setup.numbers,
        embedder::library(setup.native_twice).header
    );
    let Ok(mut embedder) = Embedder::try_with(&text, setup) else {
        return ("start refused".to_string(), 0, 0, 0);
    };
    let mut order = order.iter();
    let mut hops = 0_u64;
    let end = loop {
        let step = match embedder.machine.run(&mut embedder.world, slice) {
            Ok(step) => {
                if let Step::Parked(park) = &step {
                    embedder.requests.extend(park.requests.iter().cloned());
                    embedder.pending.extend(park.requests.iter().cloned());
                }
                step
            }
            Err(error) => break format!("machine refused: {error:?}"),
        };
        match step {
            Step::Ended(End::Finished(finished)) => {
                break format!("Finished({:?})", finished.result);
            }
            Step::Ended(other) => break format!("{other:?}"),
            Step::Slice | Step::Parked(_) => {
                if reopens {
                    hops += 1;
                    let layout = if hops.is_multiple_of(2) {
                        Layout(hops.wrapping_mul(0x9e37_79b9) & !1)
                    } else {
                        Layout(hops.wrapping_mul(0x9e37_79b9) | 1)
                    };
                    if let Err(error) = embedder.try_reopen(layout, hops.is_multiple_of(2)) {
                        break format!("reopen refused: {error:?}");
                    }
                }
            }
        }
        if matches!(step, Step::Parked(_)) {
            let next = match order.next() {
                Some(name) => (*name).to_string(),
                None => label(
                    embedder
                        .pending
                        .first()
                        .expect("a park with nothing pending"),
                ),
            };
            embedder.deliver(&next);
        }
    };
    let charged = embedder.machine.meters().charged;
    (
        end,
        charged,
        embedder.requests.len(),
        embedder.world.printed.len(),
    )
}

/// [`run_with`] under a chosen memory bound.
fn run_with_bounds(
    text: &str,
    order: &[&str],
    slice: u64,
    reopens: bool,
    bounds: Bounds,
) -> (String, Vec<PhaseRecord>) {
    let setup = Setup {
        bounds,
        slice,
        ..Setup::default()
    };
    let text = format!(
        "numbers {}\n{}{text}",
        setup.numbers,
        embedder::library(setup.native_twice).header
    );
    let Ok(mut embedder) = Embedder::try_with(&text, setup) else {
        return ("start refused".to_string(), Vec::new());
    };
    let mut phases = Vec::new();
    let mut order = order.iter();
    let mut hops = 0_u64;
    let end = loop {
        // `machine.run` gives the bound as a machine fault, not an `End`;
        // a refusal under the bound is that fault's `Bound`.
        let step = match embedder.machine.run(&mut embedder.world, slice) {
            Ok(step) => {
                if let Step::Parked(park) = &step {
                    embedder.requests.extend(park.requests.iter().cloned());
                    embedder.pending.extend(park.requests.iter().cloned());
                    embedder.withdrawn.extend(park.withdrawn.iter().copied());
                }
                step
            }
            Err(error) => break format!("machine refused: {error:?}"),
        };
        match step {
            Step::Ended(end) => {
                // A finished run compares by its result: the carried
                // root's page ids differ under a different bound and say
                // nothing.
                break match &end {
                    End::Finished(finished) => {
                        format!("Finished({:?})", finished.result)
                    }
                    other => format!("{other:?}"),
                };
            }
            Step::Slice | Step::Parked(_) => {
                if reopens {
                    hops += 1;
                    phases.extend(
                        embedder
                            .machine
                            .pending_phases()
                            .iter()
                            .map(|(_, _, phase)| *phase),
                    );
                    let layout = if hops.is_multiple_of(2) {
                        Layout(hops.wrapping_mul(0x9e37_79b9) & !1)
                    } else {
                        Layout(hops.wrapping_mul(0x9e37_79b9) | 1)
                    };
                    if let Err(error) = embedder.try_reopen(layout, hops.is_multiple_of(2)) {
                        break format!("machine refused: {error:?}");
                    }
                }
            }
        }
        if matches!(step, Step::Parked(_)) {
            let next = match order.next() {
                Some(name) => (*name).to_string(),
                None => label(
                    embedder
                        .pending
                        .first()
                        .expect("a park with nothing pending"),
                ),
            };
            embedder.deliver(&next);
        }
    };
    (end, phases)
}

/// Runs `embedder` to its end or a machine fault, answering every park
/// with its oldest pending request: the fault text a refused
/// continuation gives.
fn outcome(embedder: &mut Embedder, slice: u64) -> String {
    loop {
        let step = match embedder.machine.run(&mut embedder.world, slice) {
            Ok(step) => step,
            Err(error) => return format!("{error:?}"),
        };
        if let Step::Parked(park) = &step {
            embedder.pending.extend(park.requests.iter().cloned());
        }
        match step {
            Step::Ended(end) => return format!("{end:?}"),
            Step::Slice => {}
            Step::Parked(_) => {
                // The answer can surface the refusal: delivering resumes
                // the parked task, which decodes its saved continuation.
                let request = embedder
                    .pending
                    .first()
                    .expect("a park with nothing pending")
                    .clone();
                match embedder
                    .machine
                    .deliver(embedder::wait_of(&request), embedder::answer(&request))
                {
                    Ok(_) => embedder.pending.remove(0),
                    Err(error) => return format!("{error:?}"),
                };
            }
        }
    }
}

/// Runs `source` a step at a time until `want` accepts the live pending
/// continuations (`None` once a step had to come), then leaves the
/// embedder at that point.
fn park_where(source: &str, want: impl Fn(&[(TaskId, usize, PhaseRecord)]) -> bool) -> Embedder {
    let mut embedder = Embedder::with(
        source,
        Setup {
            slice: 0,
            ..Setup::default()
        },
    );
    for _ in 0..100 {
        let step = embedder.run();
        let phases = embedder.machine.pending_phases();
        if want(&phases) {
            return embedder;
        }
        match step {
            Step::Ended(_) => panic!("the run ended before {phases:?} matched"),
            Step::Slice => {}
            Step::Parked(_) => embedder.deliver_next(),
        }
    }
    panic!("the run never reached a matching pending")
}

/// The continuation phases of one task's frame, as `pending_phases`
/// reports them.
fn only(phase: PhaseRecord) -> Vec<(TaskId, usize, PhaseRecord)> {
    vec![(TaskId::MAIN, 0, phase)]
}

/// `source` parked with a `Waiting` continuation on `main`'s only frame.
fn waiting() -> Embedder {
    park_where(NESTED_AWAIT, |phases| phases == only(PhaseRecord::Waiting))
}

/// `main` parked mid-`call a()`: the callee frame is on top and the
/// caller's saved `Called` phase is only decoded when the call returns.
fn called() -> Embedder {
    park_where(NESTED_AWAIT, |phases| phases == only(PhaseRecord::Called))
}

/// Reopens `embedder` cold under an odd layout, then runs to the
/// refusal: an open that decodes a refused continuation fails the open
/// itself.
fn refused(embedder: &mut Embedder) -> String {
    match embedder.try_reopen(Layout(0x9e37_79b9 | 1), true) {
        Err(error) => error,
        Ok(()) => outcome(embedder, 0),
    }
}

#[test]
fn unknown_site_is_refused() {
    let mut embedder = waiting();
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, _| {
            pending.as_mut().unwrap().site = vec![9, 9];
        })
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(problem.contains("unknown site"), "{problem}");
}

#[test]
fn a_called_phase_on_a_wake_is_refused() {
    let mut embedder = waiting();
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, _| {
            pending.as_mut().unwrap().phase = PhaseRecord::Called;
        })
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(problem.contains("illegal phase"), "{problem}");
}

#[test]
fn a_waiting_phase_on_a_return_is_refused() {
    let mut embedder = called();
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, _| {
            pending.as_mut().unwrap().phase = PhaseRecord::Waiting;
        })
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(problem.contains("illegal phase"), "{problem}");
}

#[test]
fn a_pending_continuation_on_a_fresh_frame_is_refused() {
    // At the slice after `let x`, the frame's statement is the `let r`
    // it is about to run: its first nested action's path suffix is
    // `[0, 0]`. A pending there was never suspended.
    let mut embedder = park_where(NESTED_AWAIT, |phases| phases.is_empty());
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, _| {
            *pending = Some(PendingRecord {
                site: vec![0, 0],
                phase: PhaseRecord::Called,
                roles: Tree::new(true),
            });
        })
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(problem.contains("illegal phase"), "{problem}");
}

#[test]
fn a_missing_role_is_refused() {
    let mut embedder = waiting();
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, access| {
            pending
                .as_mut()
                .unwrap()
                .roles
                .remove(&ordinal_key(0), access)
                .unwrap();
        })
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(problem.contains("missing role"), "{problem}");
}

#[test]
fn an_extra_role_is_refused() {
    let mut embedder = waiting();
    embedder
        .machine
        .tamper_pending(TaskId::MAIN, 0, |pending, access| {
            let pending = pending.as_mut().unwrap();
            let value = pending
                .roles
                .after(None, access)
                .unwrap()
                .unwrap()
                .1
                .value
                .clone();
            pending
                .roles
                .write(
                    ordinal_key(9),
                    RoleRecord {
                        site: vec![9],
                        value,
                    },
                    access,
                )
                .unwrap();
        })
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(problem.contains("extra role"), "{problem}");
}

/// A task cancelled while it waits on a nested `perform` resumes as a
/// raise at its action's site (`K-TASK-017`): an incoming outcome is a
/// legal thing to deliver to a `Waiting` phase. The same run, reopened
/// at every safe point, leaves what the uninterrupted run leaves.
#[test]
fn cancel_at_a_nested_wait_resumes_exactly() {
    const SOURCE: &str = "fn work(x) { let v = [perform echo(x) as Any] return v[0] }\n\
        main { let h = spawn call work(0)\n\
        do cancel h\n\
        let r = \"none\"\n\
        try { let v = join h\n\
        set r = v } catch e { set r = e.kind }\n\
        return r }\n";
    case(
        SOURCE,
        &[],
        &text("cancelled"),
        &[],
        &[PhaseRecord::Waiting],
    );
}

/// A saved `Waiting` phase on a task resuming a delivered value is a
/// machine fault: a request's wait cannot be answered by a return's.
#[test]
fn a_task_state_refusal_is_typed() {
    let mut embedder = waiting();
    embedder
        .machine
        .tamper_task_state(
            TaskId::MAIN,
            TaskStateRecord::Resuming(IncomingRecord::Value(Value::Null)),
        )
        .unwrap();
    let problem = refused(&mut embedder);
    assert!(problem.contains("task state"), "{problem}");
}

/// R-8: for a nested action under every expression form and every
/// admitted statement position, `live_roles` names exactly the sites
/// the compiler's resume entry saves, under layout 0 and an odd layout.
#[test]
fn live_roles_match_every_resume_entry() {
    const POSITIONS: &str = "fn inner(p) { return [p] }\n\
        main {\n\
        let a = (perform echo(\"a\") as Any, 0)\n\
        let b = [perform echo(\"b\") as Any, 1]\n\
        let m = map{perform echo(\"mk\") as Any: perform echo(\"mv\") as Any}\n\
        let s = set{perform echo(\"s\") as Any}\n\
        let r = {f: perform echo(\"r\") as Any}\n\
        let i = b[perform echo(\"i\") as Any]\n\
        let n = call inner(perform echo(\"n\") as Any)\n\
        let p = perform echo([perform echo(\"p\") as Any]) as Any\n\
        let lib = invoke list.len([perform echo(\"lib\") as Any])\n\
        let cl = fn(x) { return [perform echo(\"cl\") as Any, x] }\n\
        let t = 0\n\
        set t = [perform echo(\"set\") as Any]\n\
        if [perform echo(\"if\") as Any][0] { let c = 0 }\n\
        for q in [perform echo(\"for\") as Any] { let d = q }\n\
        try { let u = [perform echo(\"try\") as Any] } catch e { let c = 0 }\n\
        return (a, perform echo(\"ret\") as Any)\n\
        }\n";
    for layout in [Layout::default(), Layout(0x9e37_79b9 | 1)] {
        let embedder = Embedder::with(
            POSITIONS,
            Setup {
                layout,
                ..Setup::default()
            },
        );
        let exe = embedder.machine.executable();
        let document = &embedder.program(true).0.document;
        let mut found = 0;
        for block in exe.blocks() {
            for &stmt in &block.stmts {
                let (stream, row, _) = exe.statement(stmt);
                for act in row.nested(stream) {
                    let entry = &stream.nested[act.ix()];
                    // The entry belongs to this statement: it resumes
                    // inside the statement's ops and the op it resumes
                    // after is the `Nested` that names it.
                    assert!(entry.resume < row.ops.len, "{}", entry.site);
                    let nested_op = &stream.ops[row.ops.start as usize + entry.resume as usize - 1];
                    assert!(
                        matches!(
                            nested_op,
                            lash_kernel_exec::Op::Nested { act: named, .. } if *named == act
                        ),
                        "{}: {nested_op:?}",
                        entry.site
                    );
                    let roles = lash_kernel_doc::live_roles(document, &entry.site)
                        .unwrap_or_else(|error| panic!("{}: {error}", entry.site));
                    let expected: Vec<lash_kernel_doc::Site> =
                        entry.roles.iter().map(|(site, _)| site.clone()).collect();
                    assert_eq!(roles, expected, "at {}", entry.site);
                    found += 1;
                }
            }
        }
        assert!(found >= 15, "only {found} nested actions compiled");
        assert_eq!(
            lash_kernel_doc::live_roles(
                document,
                &lash_kernel_doc::Site::new(lash_kernel_doc::Unit::Main, vec![0]),
            ),
            Err(lash_kernel_doc::LiveRolesError::NotAction)
        );
        assert_eq!(
            lash_kernel_doc::live_roles(
                document,
                &lash_kernel_doc::Site::new(lash_kernel_doc::Unit::Main, vec![999]),
            ),
            Err(lash_kernel_doc::LiveRolesError::NoSuchNode)
        );
    }
}

/// Positions a nested action may not stand refuse with a typed reason:
/// a `while` condition and both halves of an assignment place.
#[test]
fn refused_positions_are_typed() {
    let library = embedder::library(true);
    for text in [
        "main { let x = 1\n  while [perform echo(\"w\") as Any][0] { let c = x } }",
        "main { let t = [0]\n  set t[perform echo(\"i\") as Any] = 9 }",
        "main { let t = [0]\n  set [perform echo(\"t\") as Any][0] = 9 }",
    ] {
        let text = format!("numbers by_spelling\n{}{text}", library.header);
        let document = lash_kernel_doc::parse_document(&text)
            .unwrap_or_else(|error| panic!("{error}\n{text}"));
        let problem =
            lash_kernel_doc::validate_document(&document, library.registry.as_ref()).unwrap_err();
        let problem = format!("{problem}");
        assert!(
            problem.contains("cannot be nested"),
            "`{text}` refused with {problem}"
        );
    }
}

/// A page source that counts what a cold resume reads: page reads, the
/// distinct pages among them, and their logical bytes.
struct CountReads {
    source: embedder::Source,
    reads: std::rc::Rc<std::cell::RefCell<Reads>>,
}

#[derive(Default, Debug)]
struct Reads {
    reads: u64,
    pages: std::collections::BTreeSet<lash_kernel_doc::heap::PageId>,
    bytes: u64,
}

impl lash_kernel_state::paged::PageSource for CountReads {
    fn read(
        &mut self,
        pages: &[lash_kernel_doc::heap::PageRef],
    ) -> Result<Vec<lash_kernel_state::paged::PageAnswer>, lash_kernel_doc::heap::HeapFault> {
        let answers = self.source.read(pages)?;
        let mut reads = self.reads.borrow_mut();
        for answer in &answers {
            reads.reads += 1;
            if reads.pages.insert(answer.page.id) {
                reads.bytes += answer.tokens.len() as u64;
            }
        }
        Ok(answers)
    }
    fn seal(
        &mut self,
        page: lash_kernel_state::paged::PageBody,
    ) -> Result<lash_kernel_doc::heap::PageRef, lash_kernel_doc::heap::HeapFault> {
        self.source.seal(page)
    }
}

/// The P0 cost program: `big`, `n` elements, stays reachable; a first
/// park seals the whole run, and the measured park comes at `echo(x)`,
/// nested beside two evaluated operands (`big` itself and `call a()`'s
/// result) or hoisted into statements of its own.
fn retained(n: usize, extra: usize, nested: bool) -> String {
    // `extra` more evaluated operands, small integers, before the call.
    let more: String = (0..extra).map(|i| format!("{i}, ")).collect();
    let tail = if nested {
        format!("let r = [big, {more}call a(), perform echo(x) as Any]\n")
    } else {
        format!("let t0 = call a()\nlet t1 = perform echo(x) as Any\nlet r = [big, {more}t0, t1]\n")
    };
    format!(
        "fn a() {{ return 1 }}\n\
         main {{ let big = work.fill({n})\n\
         do perform echo(\"warm\") as Any\n\
         let x = \"tool\"\n\
         {tail}\
         do perform echo(\"after\") as Any\n\
         return list.len(r) }}\n"
    )
}

/// P0 measurement, not a law: at the measured park, the parked frame's
/// encoded size and its continuation's metadata and role payload; the
/// pages and bytes the park's seal writes beyond the first seal; and the
/// pages a cold resume under another layout reads, with its CPU time, for
/// a nested and a hoisted program over retained heaps of growing size.
#[test]
#[ignore = "P0 measurement; run with --include-ignored --nocapture"]
fn p0_continuation_costs() {
    use lash_kernel_doc::graph_tokens::encode_body;
    let reps: u32 = 31;
    for (n, extra) in [
        (0_usize, 0_usize),
        (10_000, 0),
        (1_000_000, 0),
        (0, 2),
        (0, 6),
        (0, 14),
        (0, 30),
        (0, 126),
    ] {
        for nested in [false, true] {
            let shape = if nested { "nested" } else { "hoisted" };
            let mut embedder = Embedder::new(&retained(n, extra, nested));
            assert!(matches!(embedder.run(), Step::Parked(_)), "the warm park");
            let first_pages = embedder.source.0.borrow().len();
            embedder.checkpoint();
            let sealed_first = embedder.source.0.borrow().len() - first_pages;
            embedder.deliver("warm");
            // Everything the run writes from here to the checkpoint after
            // the measured park: the park itself seals a spilled role tree.
            let before: std::collections::BTreeSet<_> =
                embedder.source.0.borrow().keys().copied().collect();
            assert!(
                matches!(embedder.run(), Step::Parked(_)),
                "the measured park"
            );
            // The parked frame and its continuation, as the seal encodes
            // them.
            let rows = embedder.machine.frame_rows();
            let (_, _, frame, saved) = rows
                .iter()
                .find(|(task, index, _, _)| *task == TaskId::MAIN && *index == 0)
                .expect("main's frame")
                .clone();
            let (root, certificate) = embedder.checkpoint();
            let frame_bytes = encode_body(&frame).len();
            let payload: usize = saved
                .iter()
                .map(|role| encode_body(&role.value).len())
                .sum();
            let role_sites: usize = saved.iter().map(|role| encode_body(&role.site).len()).sum();
            let role_records: usize = saved.iter().map(|role| encode_body(role).len()).sum();
            let roles = saved.len();
            // A role tree too large to stay inline lives in pages of its
            // own: they count toward the continuation.
            let mut spilled = 0;
            let mut role_pages = 0;
            if let Some(page) = frame.pending.as_ref().and_then(|p| p.roles.page()) {
                let mut stack = vec![page];
                let mut seen = std::collections::BTreeSet::new();
                while let Some(page) = stack.pop() {
                    if !seen.insert(page.id) {
                        continue;
                    }
                    let answer = embedder.source.0.borrow().get(&page.id).cloned().unwrap();
                    spilled += answer.tokens.len();
                    role_pages += 1;
                    stack.extend(answer.children.iter().copied());
                }
            }
            let pending_bytes =
                frame.pending.as_ref().map_or(0, |p| encode_body(p).len()) + spilled;
            // The same continuation encoded as one flat sequence (action
            // site, phase, then each role's site and value), with no
            // ordinal-keyed tree: what the tree's keys and nodes cost.
            let flat = frame.pending.as_ref().map_or(0, |p| {
                encode_body(&p.site).len()
                    + encode_body(&p.phase).len()
                    + encode_body(&saved).len()
                    + 1
            });
            let written: Vec<_> = embedder
                .source
                .0
                .borrow()
                .iter()
                .filter(|(id, _)| !before.contains(id))
                .map(|(_, answer)| answer.tokens.len())
                .collect();
            let mut kinds: Vec<_> = embedder
                .source
                .0
                .borrow()
                .iter()
                .filter(|(id, _)| !before.contains(id))
                .map(|(_, answer)| answer.kind.0)
                .collect();
            kinds.sort_unstable();
            println!("P0-PAGES n={n} extra={extra} shape={shape} kinds={kinds:?}");
            println!(
                "P0-PARK n={n} extra={extra} shape={shape} first_seal_pages={sealed_first} \
                 park_pages={} park_bytes={} frame_bytes={frame_bytes} \
                 continuation_bytes={pending_bytes} metadata_bytes={} role_payload_bytes={payload} \
                 roles={roles} role_site_bytes={role_sites} role_record_bytes={role_records} \
                 role_pages={role_pages} flat_bytes={flat} flat_metadata_bytes={}",
                written.len(),
                written.iter().sum::<usize>(),
                pending_bytes - payload,
                flat.saturating_sub(payload),
            );
            // Cold resumes: open under another layout with a fresh
            // library, answer the wait, run to the end.
            let mut samples = Vec::new();
            let mut reads_seen = None;
            for rep in 0..reps {
                let reads = std::rc::Rc::new(std::cell::RefCell::new(Reads::default()));
                let (program, _) = embedder.program(rep % 2 == 0);
                let request = embedder
                    .pending
                    .iter()
                    .find(|request| label(request) == "tool")
                    .expect("the measured wait")
                    .clone();
                let began = std::time::Instant::now();
                let mut machine = crate::KernelMachine::open_with_layout(
                    program,
                    embedder::ROOMY,
                    &root,
                    Vec::new(),
                    Box::new(CountReads {
                        source: embedder.source.clone(),
                        reads: std::rc::Rc::clone(&reads),
                    }),
                    Box::new(
                        lash_kernel_state::paged::census_impl::Census::restore(
                            certificate,
                            lash_kernel_doc::heap::LogicalBytes(embedder::ROOMY.memory),
                        )
                        .unwrap(),
                    ),
                    Layout(0x9e37_79b9 | 1),
                )
                .unwrap();
                machine
                    .deliver(embedder::wait_of(&request), embedder::answer(&request))
                    .unwrap();
                // The resumed statement runs, then the task parks again
                // at `echo("after")`: the cold resume ends there.
                let step = machine.run(&mut embedder.world, u64::MAX).unwrap();
                let elapsed = began.elapsed().as_nanos();
                let Step::Parked(park) = step else {
                    panic!("the resumed run did not park: {step:?}");
                };
                samples.push(elapsed);
                let resumed = {
                    let reads = reads.borrow();
                    (reads.reads, reads.pages.len() as u64, reads.bytes)
                };
                // The run's end, apart: what finishing reads.
                let after = park.requests[0].clone();
                machine
                    .deliver(embedder::wait_of(&after), embedder::answer(&after))
                    .unwrap();
                let step = machine.run(&mut embedder.world, u64::MAX).unwrap();
                let Step::Ended(End::Finished(finished)) = step else {
                    panic!("the resumed run did not finish: {step:?}");
                };
                assert_eq!(finished.result, embedder::int(3 + extra as i64));
                let reads = reads.borrow();
                let seen = (
                    resumed,
                    (
                        reads.reads - resumed.0,
                        reads.pages.len() as u64 - resumed.1,
                    ),
                );
                if let Some(previous) = reads_seen {
                    assert_eq!(previous, seen, "every cold resume reads the same pages");
                }
                reads_seen = Some(seen);
            }
            samples.sort_unstable();
            let ((reads, pages, bytes), (end_reads, end_pages)) = reads_seen.unwrap();
            println!(
                "P0-RESUME n={n} extra={extra} shape={shape} reads={reads} distinct_pages={pages} bytes={bytes} \
                 end_reads={end_reads} end_new_pages={end_pages} \
                 reps={reps} ns_min={} ns_median={} ns_p90={}",
                samples[0],
                samples[samples.len() / 2],
                samples[samples.len() * 9 / 10],
            );
        }
    }
}
