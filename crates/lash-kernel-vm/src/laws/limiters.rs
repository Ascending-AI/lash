//! The limiter laws (FIG-6768): where a run reaches its memory or charge
//! bound, its host's limiter grows the limit or refuses; a grown limit is
//! saved with the run, and a run resumed from before the save asks again.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use super::embedder::{Embedder, ROOMY, Setup, int, result};
use crate::{
    Bound, Bounds, ChargeLimiter, End, LimitDecision, Machine, MachineOptions, MemoryLimiter,
    RunError, Step,
};

/// A host's limiter: it answers from its script, then refuses, and keeps
/// every question it was asked, `(used, limit)`, in order.
#[derive(Clone, Default)]
struct Scripted {
    asked: Rc<RefCell<Vec<(u64, u64)>>>,
    answers: Rc<RefCell<VecDeque<LimitDecision>>>,
}

impl Scripted {
    fn answering(answers: impl IntoIterator<Item = LimitDecision>) -> Self {
        Self {
            asked: Rc::default(),
            answers: Rc::new(RefCell::new(answers.into_iter().collect())),
        }
    }

    fn decide(&self, used: u64, limit: u64) -> LimitDecision {
        self.asked.borrow_mut().push((used, limit));
        self.answers
            .borrow_mut()
            .pop_front()
            .unwrap_or(LimitDecision::Refuse)
    }

    fn asked(&self) -> Vec<(u64, u64)> {
        self.asked.borrow().clone()
    }
}

impl ChargeLimiter for Scripted {
    fn limit_reached(&mut self, used: u64, limit: u64) -> LimitDecision {
        self.decide(used, limit)
    }
}

impl MemoryLimiter for Scripted {
    fn limit_reached(&mut self, used: u64, limit: u64) -> LimitDecision {
        self.decide(used, limit)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Charge,
    Memory,
}

fn limited(embedder: &mut Embedder, which: Which, limiter: Option<&Scripted>) {
    let limiter = limiter.cloned();
    embedder.machine.set_options(match which {
        Which::Charge => MachineOptions {
            charge_limiter: limiter.map(|limiter| Box::new(limiter) as Box<dyn ChargeLimiter>),
            ..MachineOptions::default()
        },
        Which::Memory => MachineOptions {
            memory_limiter: limiter.map(|limiter| Box::new(limiter) as Box<dyn MemoryLimiter>),
            ..MachineOptions::default()
        },
    });
}

/// What a run observably did: how it ended, what it was charged, what it
/// printed and every effect it asked for.
#[derive(Debug, PartialEq)]
struct Trace {
    end: String,
    charged: Option<u64>,
    printed: Vec<lash_kernel_doc::Datum>,
    asked: Vec<String>,
}

fn trace(embedder: &Embedder, end: End) -> Trace {
    Trace {
        end: match end {
            End::Finished(finished) => format!("{:?}", finished.result),
            other => format!("{other:?}"),
        },
        charged: embedder.machine.meters().charged,
        printed: embedder.world.printed.clone(),
        asked: embedder.asked(),
    }
}

/// Runs `text` under `bounds` to its end, sealing at every park and
/// resuming on a fresh machine opened with `bounds` again, `limiter`
/// answering at the bound `which` on every machine. The trace, and the
/// limit the last park saved with how many questions the limiter had been
/// asked by then.
fn sealed_and_resumed(
    text: &str,
    bounds: Bounds,
    which: Which,
    limiter: Option<&Scripted>,
) -> (Trace, Option<(u64, usize)>) {
    let mut embedder = Embedder::with(
        text,
        Setup {
            bounds,
            probes: Some(if text == super::layout::HELPERS {
                super::layout::helper_probes
            } else {
                super::layout::scalar_probes
            }),
            ..Setup::default()
        },
    );
    limited(&mut embedder, which, limiter);
    let mut saved = None;
    loop {
        match embedder.run() {
            Step::Ended(end) => return (trace(&embedder, end), saved),
            Step::Slice => {}
            Step::Parked(_) => {
                let root = embedder.reopen(true);
                let limit = match which {
                    Which::Charge => root.limits.charge,
                    Which::Memory => root.limits.memory.expect("memory is metered"),
                };
                saved = Some((limit, limiter.map_or(0, |limiter| limiter.asked().len())));
                limited(&mut embedder, which, limiter);
                embedder.deliver_next();
            }
        }
    }
}

/// Loops on both sides of a park, building text of `chunk`s that stays
/// live.
fn builds(chunk: &str) -> String {
    format!(
        r#"main {{
  let s = ""
  let i = 0
  while num.lt(i, 60) {{
    set s = text.concat(s, "{chunk}")
    set i = num.add(i, 1)
  }}
  print i
  do perform 0("park") as Any
  let j = 0
  while num.lt(j, 60) {{
    set s = text.concat(s, "{chunk}")
    set j = num.add(j, 1)
  }}
  print j
  return num.add(i, j)
}}"#
    )
}

/// Whose live text passes [`MEMORY`] before its park.
fn large() -> String {
    builds(&"x".repeat(100))
}

/// A memory limit [`large`] passes before its park, exactly counted.
const MEMORY: u64 = 4 << 10;

/// FIG-6768 law 1, charge: a run that grows its charge limit where it
/// reaches it, then seals at its park and resumes on a fresh machine opened
/// with the limit it started with, runs as one started with the grown
/// limit: the same end, charge, prints and effects. Only the decision
/// differs. It holds at every limit the run can reach, so the crossing
/// falls in every kind of charge site the program has, before the park
/// and after it, and a grown limit the run still reaches is asked again.
#[test]
fn a_grown_charge_limit_runs_as_a_run_started_with_it() {
    for text in [
        builds("0123456789"),
        super::layout::scalar_document(
            "let i = 0 let s = 0 while flat.lt(i, 5) { set s = flat.add(s, flat.mul(i, i)) set i = flat.add(i, 1) } print s do perform 0(s) as Any set i = 0 while flat.lt(i, 5) { set s = flat.add(s, flat.mul(i, i)) set i = flat.add(i, 1) } print s return s",
        ),
    ] {
        grown_charge(&text);
    }
}

fn grown_charge(text: &str) {
    let (whole, _) = sealed_and_resumed(text, ROOMY, Which::Charge, None);
    let total = whole.charged.expect("charge is metered");
    let grow = total / 3;
    // Every limit the first statements reach, then limits spread over the
    // whole run.
    let limits = (1..=64).chain((64..total).step_by((total / 29).max(1) as usize));
    for limit in limits {
        let limiter = Scripted::answering([LimitDecision::Grow(grow)]);
        let bounds = Bounds {
            charge_limit: limit,
            ..ROOMY
        };
        let (grown, saved) = sealed_and_resumed(text, bounds, Which::Charge, Some(&limiter));
        let started = Bounds {
            charge_limit: limit + grow,
            ..ROOMY
        };
        let (expected, _) = sealed_and_resumed(text, started, Which::Charge, None);
        assert_eq!(grown, expected, "limit {limit}");
        if text.contains("flat.") {
            let (reference, _) = sealed_and_resumed(text, started, Which::Charge, None);
            assert_eq!(grown, reference, "typed boundary {limit}");
        }
        let asked = limiter.asked();
        assert_eq!(asked[0].1, limit, "limit {limit}: asked at the limit");
        assert!(asked[0].0 > limit, "limit {limit}: asked past it");
        // A run that reaches the grown limit too is asked again there and
        // refused, as the run started with it is.
        let again = limit + grow < total;
        assert_eq!(asked.len(), 1 + usize::from(again), "limit {limit}");
        if again {
            assert_eq!(asked[1].1, limit + grow);
        }
        // The park saves the limit as it stands there: grown once the
        // limiter has answered. A run refused at its grown limit before
        // the park never parks.
        match saved {
            Some((saved, 0)) => assert_eq!(saved, limit, "limit {limit}"),
            Some((saved, _)) => assert_eq!(saved, limit + grow, "limit {limit}"),
            None => assert!(again, "limit {limit}: the run parks"),
        }
    }
}

/// FIG-6768 law 1, memory: a run that grows its memory limit where its
/// exact count no longer fits, then seals at its park and resumes on a
/// fresh machine opened with the limit it started with, runs as one
/// started with the grown limit, apart from the one decision. The park
/// saved the grown limit.
#[test]
fn a_grown_memory_limit_runs_as_a_run_started_with_it() {
    const LIMIT: u64 = MEMORY;
    const GROW: u64 = 1 << 20;
    let limiter = Scripted::answering([LimitDecision::Grow(GROW)]);
    let bounds = Bounds {
        memory: LIMIT,
        ..ROOMY
    };
    let (grown, saved) = sealed_and_resumed(&large(), bounds, Which::Memory, Some(&limiter));
    let started = Bounds {
        memory: LIMIT + GROW,
        ..ROOMY
    };
    let (expected, _) = sealed_and_resumed(&large(), started, Which::Memory, None);
    assert_eq!(grown, expected);
    assert_eq!(grown.end, format!("{:?}", int(120)));
    let asked = limiter.asked();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].1, LIMIT);
    assert!(asked[0].0 > LIMIT);
    assert_eq!(
        saved,
        Some((LIMIT + GROW, 1)),
        "the run grew before its park, which saved the grown limit"
    );
}

/// Parks first, then passes the charge limit it is given, then parks again.
const CUT: &str = r#"main {
  do perform 0("first") as Any
  let i = 0
  while num.lt(i, 40) {
    set i = num.add(i, 1)
  }
  print i
  do perform 0("second") as Any
  return i
}"#;

/// FIG-6768 law 2: a crash cut between the limiter's answer and the save
/// that would keep it asks again. The run is resumed from the seal before
/// the decision, on a fresh machine whose host already knows the grown
/// limit: the checkpoint's limit wins, the run reaches the same point,
/// asks the same question, and its observable trace up to the next park
/// is the one the lost attempt made.
#[test]
fn a_crash_before_a_grown_limit_is_saved_asks_again() {
    let (whole, _) = sealed_and_resumed(CUT, ROOMY, Which::Charge, None);
    let total = whole.charged.expect("charge is metered");
    const GROW: u64 = 1_000;
    let mut embedder = Embedder::with(
        CUT,
        Setup {
            bounds: Bounds {
                charge_limit: total / 2,
                ..ROOMY
            },
            ..Setup::default()
        },
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let root = embedder.checkpoint();
    assert_eq!(root.limits.charge, total / 2);
    let pending = embedder.pending.clone();
    let mut attempts = Vec::new();
    for _ in 0..2 {
        let limiter = Scripted::answering([LimitDecision::Grow(GROW)]);
        embedder.open_at(&root, true);
        limited(&mut embedder, Which::Charge, Some(&limiter));
        embedder.pending = pending.clone();
        embedder.world.printed.clear();
        embedder.deliver("first");
        let Step::Parked(park) = embedder.run() else {
            panic!("the run parks at `second`")
        };
        attempts.push((
            limiter.asked(),
            park.requests.len(),
            embedder.machine.meters().charged,
            embedder.world.printed.clone(),
        ));
        // The host learned the grown limit; the next machine is opened
        // with it.
        embedder.bounds.charge_limit = total / 2 + GROW;
    }
    assert_eq!(attempts[0], attempts[1]);
    assert_eq!(attempts[0].0.len(), 1, "one decision each time");
    assert_eq!(attempts[0].0[0].1, total / 2);
}

fn refused(text: &str, bounds: Bounds, which: Which, limiter: Option<&Scripted>) -> (Trace, End) {
    let mut embedder = Embedder::with(
        text,
        Setup {
            probes: if text == super::layout::ALLOCATING {
                Some(super::layout::allocation_probes)
            } else if text == super::layout::HELPERS {
                Some(super::layout::helper_probes)
            } else {
                None
            },
            bounds,
            ..Setup::default()
        },
    );
    limited(&mut embedder, which, limiter);
    if text == super::layout::ALLOCATING {
        super::observe::fully_observe(&mut embedder.machine);
    }

    let end = embedder.run_to_end(&[]);

    (trace(&embedder, end.clone()), end)
}

/// FIG-6768 law 4, embedded host: a limiter that refuses ends the run with
/// the `BoundExceeded` a run without a limiter ends with, at the same
/// point, charged the same, for both bounds; it was asked once, past the
/// limit.
#[test]
fn a_refusing_limiter_ends_the_run_where_no_limiter_does() {
    let charge = Bounds {
        charge_limit: 1_000,
        ..ROOMY
    };
    let memory = Bounds {
        memory: MEMORY,
        ..ROOMY
    };
    for (which, bounds, bound, limit) in [
        (Which::Charge, charge, Bound::Charge, 1_000),
        (Which::Memory, memory, Bound::Memory, MEMORY),
    ] {
        let limiter = Scripted::default();
        let (asked, end) = refused(&large(), bounds, which, Some(&limiter));
        let (unasked, _) = refused(&large(), bounds, which, None);
        assert_eq!(asked, unasked, "{which:?}");
        let End::Error(RunError::Bound(exceeded)) = end else {
            panic!("{which:?}: the run passes its bound: {end:?}")
        };
        assert_eq!((exceeded.bound, exceeded.limit), (bound, limit));
        let questions = limiter.asked();
        assert_eq!(questions.len(), 1, "{which:?}");
        assert_eq!(questions[0].1, limit, "{which:?}");
        assert!(questions[0].0 > limit, "{which:?}");
    }
}

/// FIG-6791: the memory limiter is asked with what the run holds and the
/// allocation that passes the limit, as the capacity meter counts them:
/// a run holding a 1.5 MiB text that asks for 1 MiB more past a 2 MiB
/// limit is asked with at least 2.5 MiB, and no more than one chunk past
/// it.
#[test]
fn the_memory_limiter_is_asked_with_what_the_run_holds() {
    const HELD: u64 = 3 << 19;
    const ASKED: u64 = 1 << 20;
    let text = format!(
        "main {{ let s = work.repeat({HELD}) let t = work.repeat({ASKED}) return list.len([s, t]) }}"
    );
    let bounds = Bounds {
        memory: 2 << 20,
        ..ROOMY
    };
    let limiter = Scripted::default();
    let (_, end) = refused(&text, bounds, Which::Memory, Some(&limiter));
    let End::Error(RunError::Bound(exceeded)) = end else {
        panic!("the run passes its bound: {end:?}")
    };
    assert_eq!((exceeded.bound, exceeded.limit), (Bound::Memory, 2 << 20));
    let [(used, limit)] = limiter.asked()[..] else {
        panic!("the limiter was asked once: {:?}", limiter.asked())
    };
    assert_eq!(limit, 2 << 20);
    assert!(used >= HELD + ASKED, "asked with {used}");
    assert!(used <= HELD + ASKED + (64 << 10), "asked with {used}");
}

/// K-CHG-002: admission of a literal return never lets an effect after
/// the first refusing event escape. Compare every charge crossing with
/// the default refusal and with the successful run's actual effect prefix.
#[test]
fn literal_return_refusals() {
    let source = r#"
        definition "function constant(x: Any) -> Any\nkernel 1\nmetering statements\nbody { return 1 }"
        main {
            do perform 0("before") as Any
            let value = call constant(7)
            print value
            do perform 0("after") as Any
            return value
        }
    "#;
    let (whole, end) = refused(source, ROOMY, Which::Charge, None);
    assert_eq!(result(end), int(1));
    assert_eq!(whole.asked, ["before", "after"]);
    for limit in 0..whole.charged.unwrap() {
        let bounds = Bounds {
            charge_limit: limit,
            ..ROOMY
        };
        let limiter = Scripted::default();
        let (asked, end) = refused(source, bounds, Which::Charge, Some(&limiter));
        let (default, _) = refused(source, bounds, Which::Charge, None);
        assert_eq!(asked, default, "bound {limit}");
        assert!(matches!(end, End::Error(RunError::Bound(_))));
        assert!(whole.asked.starts_with(&asked.asked));
        assert!(whole.printed.starts_with(&asked.printed));
        assert_eq!(limiter.asked().len(), 1, "bound {limit}");
    }
}

/// K-BND-001 / K-CHG-001: a refused allocating call admits no later
/// effect or heap write, with the same first refusal as source execution.
#[test]
fn allocating_refusals() {
    let source = super::layout::ALLOCATING;
    let (whole, end) = refused(source, ROOMY, Which::Charge, None);
    assert!(matches!(end, End::Finished(_)));
    for limit in 0..whole.charged.unwrap() {
        let bounds = Bounds {
            charge_limit: limit,
            ..ROOMY
        };
        let limiter = Scripted::default();
        let (asked, end) = refused(source, bounds, Which::Charge, Some(&limiter));
        let (reference, _) = refused(source, bounds, Which::Charge, None);
        assert_eq!(asked, reference, "allocating bound {limit}");
        assert!(matches!(end, End::Error(RunError::Bound(_))));
        assert!(whole.asked.starts_with(&asked.asked));
        assert!(whole.printed.starts_with(&asked.printed));
        assert_eq!(limiter.asked().len(), 1);
    }
}

/// K-CHG-001: a loop and closure document refused at every charge, with a
/// limiter, leaves what the default refusal leaves, across its park.
#[test]
fn control_refusals() {
    let source = super::layout::control_document(
        "let bump = call make() let one = apply bump() let value = call loops([1, 2], 3, true) print value return flat.add(one, apply bump())",
    );
    let (whole, _) = sealed_and_resumed(&source, ROOMY, Which::Charge, None);
    for limit in 0..=whole.charged.unwrap() {
        let bounds = Bounds {
            charge_limit: limit,
            ..ROOMY
        };
        let limiter = Scripted::default();
        let actual = sealed_and_resumed(&source, bounds, Which::Charge, Some(&limiter));
        let reference = sealed_and_resumed(&source, bounds, Which::Charge, None);
        assert_eq!(actual.0, reference.0, "control refusal {limit}");
        assert!(whole.asked.starts_with(&actual.0.asked));
        assert!(whole.printed.starts_with(&actual.0.printed));
    }
}

/// K-CHG-001 / K-TASK: refusal at each scheduler event leaves exactly
/// the source request prefix, including cancellation's waiting cleanup.
#[test]
fn scheduler_refusals() {
    for cancel in [false, true] {
        let source = super::scheduler::document(cancel);
        let run = |limit, ask: bool| {
            let mut embedder = Embedder::with(
                &source,
                Setup {
                    bounds: Bounds {
                        charge_limit: limit,
                        ..ROOMY
                    },
                    ..Setup::default()
                },
            );
            let limiter = Scripted::default();
            limited(&mut embedder, Which::Charge, ask.then_some(&limiter));

            let end = embedder.run_to_end(&[]);

            (trace(&embedder, end), limiter.asked())
        };
        let (whole, _) = run(ROOMY.charge_limit, false);
        for limit in 0..=whole.charged.unwrap() + 1 {
            let (actual, questions) = run(limit, true);
            let reference = run(limit, false).0;
            assert_eq!(
                actual, reference,
                "scheduler refusal {limit}, cancel={cancel}"
            );
            if !questions.is_empty() {
                assert_eq!(questions.len(), 1);
                assert_eq!(questions[0].1, limit);
            }
        }
    }
}

#[test]
fn helper_refusals() {
    let source = super::layout::HELPERS;
    let (whole, _) = sealed_and_resumed(source, ROOMY, Which::Charge, None);
    for limit in 0..=whole.charged.unwrap() {
        let bounds = Bounds {
            charge_limit: limit,
            ..ROOMY
        };
        let limiter = Scripted::default();
        let actual = sealed_and_resumed(source, bounds, Which::Charge, Some(&limiter));
        let reference = sealed_and_resumed(source, bounds, Which::Charge, None);
        assert_eq!(actual.0, reference.0, "helper refusal {limit}");
        assert!(whole.asked.starts_with(&actual.0.asked));
        assert!(whole.printed.starts_with(&actual.0.printed));
    }
}

/// How a core run ended, what it was charged and what it printed.
type CoreTrace = (String, Option<u64>, Vec<lash_kernel_doc::Datum>);

/// Runs `text` on the bytecode core under charge limit `limit` in slices
/// of `slice`, `limiter` answering at the bound. `exact` records the
/// charges, which runs every block event by event: the contract's
/// reference path (cutover contract §3).
fn core_run(
    text: &str,
    limit: u64,
    slice: u64,
    exact: bool,
    limiter: Option<&Scripted>,
) -> CoreTrace {
    let seen = drive(
        text,
        Drive {
            limit,
            slice,
            exact,
            limiter,
            ..Drive::default()
        },
    );
    (seen.end, seen.charged, seen.printed)
}

/// How a law drives a core run.
#[derive(Clone, Copy)]
struct Drive<'a> {
    limit: u64,
    slice: u64,
    /// Records the charges: every block runs event by event.
    exact: bool,
    limiter: Option<&'a Scripted>,
    /// A limiter of the run's own that grows the limit once by this much.
    grow: Option<u64>,
    charge: crate::Charge,
    /// Pages every object out at each statement boundary.
    cold: bool,
    /// Cancellation is latched from the start.
    cancel: bool,
    memory: u64,
}

impl Default for Drive<'_> {
    fn default() -> Self {
        Self {
            limit: ROOMY.charge_limit,
            slice: u64::MAX,
            exact: false,
            limiter: None,
            grow: None,
            charge: ROOMY.charge,
            cold: false,
            cancel: false,
            memory: ROOMY.memory,
        }
    }
}

/// What a core run showed its host: its end, charge, output, recorded
/// charges, the charge at each slice's return, the requests it handed
/// out, the questions its limiter was asked and how often its natives ran.
#[derive(Debug, PartialEq)]
struct Observed {
    end: String,
    charged: Option<u64>,
    printed: Vec<lash_kernel_doc::Datum>,
    tape: Option<Vec<lash_kernel_exec::Weight>>,
    slices: Vec<Option<u64>>,
    requests: Vec<Vec<lash_kernel_doc::Datum>>,
    asked: Vec<(u64, u64)>,
    natives: u64,
}

fn drive(text: &str, how: Drive<'_>) -> Observed {
    let bounds = Bounds {
        charge: how.charge,
        charge_limit: how.limit,
        memory: how.memory,
        ..ROOMY
    };
    let mut machine = match super::embedder::core_machine(text, bounds, Some(huge_charges)) {
        Ok(machine) => machine,
        Err(refused) => {
            return Observed {
                end: format!("{refused:?}"),
                charged: None,
                printed: Vec::new(),
                tape: None,
                slices: Vec::new(),
                requests: Vec::new(),
                asked: Vec::new(),
                natives: 0,
            };
        }
    };
    let tape = how.exact.then(|| {
        let tape = crate::ChargeTape::new();
        machine.record_charges(tape.clone());
        tape
    });
    if how.cold {
        machine.force_cold();
    }
    let limiter = how.limiter.cloned().or_else(|| {
        how.grow
            .map(|grow| Scripted::answering([LimitDecision::Grow(grow)]))
    });
    machine.set_options(MachineOptions {
        charge_limiter: limiter
            .clone()
            .map(|limiter| Box::new(limiter) as Box<dyn ChargeLimiter>),
        ..MachineOptions::default()
    });
    let mut world = super::embedder::World {
        cancel: how.cancel,
        ..super::embedder::World::default()
    };
    let (mut slices, mut requests) = (Vec::new(), Vec::new());
    NATIVE_CALLS.set(0);
    let end = loop {
        match machine.run(&mut world, how.slice).unwrap() {
            Step::Ended(End::Finished(finished)) => break format!("{:?}", finished.result),
            Step::Ended(other) => break format!("{other:?}"),
            Step::Slice => slices.push(machine.meters().charged),
            Step::Parked(park) => {
                for request in &park.requests {
                    let crate::Request::Effect(effect) = request;
                    requests.push(effect.args.clone());
                    machine
                        .deliver(
                            super::embedder::wait_of(request),
                            super::embedder::answer(request),
                        )
                        .unwrap();
                }
            }
        }
    };
    Observed {
        end,
        charged: machine.meters().charged,
        printed: world.printed,
        tape: tape.map(|tape| tape.steps()),
        slices,
        requests,
        asked: limiter.map_or_else(Vec::new, |limiter| limiter.asked.borrow().clone()),
        natives: NATIVE_CALLS.get(),
    }
}

/// Documents whose blocks hold every kind of charge site: fixed events,
/// atomic constructions, completion fees of natives and of a library
/// call, a raise in mid-block and its catch. The third calls covered
/// bodies the inline pass expands: `law.deep` returning with a fee that
/// reads its argument deeply and its result, `law.pick` on its guard path
/// and, below 2, off it into a call of `law.raises` whose raise escapes
/// both, `law.oops` returning or raising from inside its region, and
/// `law.at`, whose guard path folds to one list read: past the end of its
/// list the fold declines and its region raises. `law.get`'s folds to a
/// membership answer and one map read: on a key the map lacks its guard
/// does not hold and the region returns 0. `law.str` folds both ways
/// of its numeric guard: an integer's digits below 1000, a constant
/// from 1000.
const EVENT_CONTRACT: [&str; 3] = [
    r#"definition "function sq(x: Any) -> Any\nkernel 1\nmetering statements\nbody { return num.add(x, x) }"
main {
  let s = ""
  let total = 0
  let i = 0
  while num.lt(i, 12) {
    set s = text.concat(s, "ab")
    let q = call sq(i)
    set total = num.add(total, q)
    let held = [i, total, (i, s)]
    set i = num.add(i, 1)
  }
  print total
  return s
}"#,
    r#"main {
  let caught = 0
  let i = 0
  while num.lt(i, 8) {
    try {
      let x = num.add(i, 1)
      let bad = text.concat(x, "x")
      set caught = num.add(caught, 100)
    } catch e {
      set caught = num.add(caught, 1)
    }
    set i = num.add(i, 1)
  }
  print caught
  return caught
}"#,
    r#"main {
  let total = 0
  let i = 0
  while num.lt(i, 4) {
    let d = invoke law.deep(map{"a": [i, i]})
    try {
      set total = num.add(total, invoke law.pick(i))
    } catch e {
      set total = num.add(total, 10)
    }
    try {
      let bad = invoke law.oops(i)
    } catch e {
      set total = num.add(total, 100)
    }
    let s = invoke law.oops("ab")
    set total = num.add(total, list.len(d))
    set total = num.add(total, invoke law.at([5, 6, 7], i))
    set total = num.add(total, invoke law.get(map{1: 20, 2: 30}, i))
    let up = invoke law.str(num.add(i, 998))
    let down = invoke law.str(num.add(i, -2))
    print (up, down)
    try {
      set total = num.add(total, invoke law.at([5], i))
    } catch e {
      set total = num.add(total, 1000)
    }
    set i = num.add(i, 1)
  }
  print total
  return total
}"#,
];

/// `drive` with every covered body called through its frame: the inline
/// pass's reference (`bytecode::inline`).
fn reference(text: &str, how: Drive<'_>) -> Observed {
    crate::bytecode::inline::disabled(|| drive(text, how))
}

/// `drive`, asserting that the inline pass expanded a body of `text`.
fn inlined(text: &str, how: Drive<'_>) -> Observed {
    let before = crate::bytecode::inline::added();
    let seen = drive(text, how);
    assert!(
        crate::bytecode::inline::added() > before,
        "no body of the law's document was expanded"
    );
    seen
}

/// Contract §3: at every charge limit the run can reach, a prepaid run
/// refuses at the same event with the same total as the event-by-event
/// run, whether its fixed costs were reserved, cancelled by a dynamic fee
/// or a raise, or stopped short by a slice; a slice of any size, a fee
/// on top of a reservation included, returns at the charge the
/// event-by-event run's slice does; and a limiter that grows the
/// limit at the refused event resumes it there, as a run started with the
/// grown limit runs. A limit at and next to `u64::MAX` reserves without
/// overflow. Where covered bodies run inline, every limit refuses at the
/// same event, with the same tape, function and site, as the run calling
/// them through their frames, and a growing limiter is asked the same.
#[test]
fn the_prepaid_and_exact_paths_refuse_at_the_same_event_with_the_same_total() {
    let held = crate::vm::inline::held();
    let inline = EVENT_CONTRACT[2];
    for text in EVENT_CONTRACT {
        let whole = core_run(text, ROOMY.charge_limit, u64::MAX, true, None);
        let total = whole.1.expect("charge is metered");
        assert!(
            whole.0.starts_with("Int") || whole.0.starts_with("Text"),
            "{whole:?}"
        );
        for slice in 1..=total {
            let sliced = |exact| {
                let seen = drive(
                    text,
                    Drive {
                        slice,
                        exact,
                        ..Drive::default()
                    },
                );
                (seen.end, seen.charged, seen.printed, seen.slices)
            };
            assert_eq!(sliced(false), sliced(true), "slice {slice}");
        }
        for limit in 0..=total + 1 {
            let exact = core_run(text, limit, u64::MAX, true, None);
            assert_eq!(
                core_run(text, limit, u64::MAX, false, None),
                exact,
                "limit {limit}"
            );
            assert_eq!(
                core_run(text, limit, 7, false, None),
                exact,
                "limit {limit}, sliced"
            );
            let grow = total / 3 + 1;
            let started = core_run(text, limit + grow, u64::MAX, true, None);
            for exact_path in [false, true] {
                let limiter = Scripted::answering([LimitDecision::Grow(grow)]);
                let grown = core_run(text, limit, u64::MAX, exact_path, Some(&limiter));
                assert_eq!(grown, started, "limit {limit} grown, exact {exact_path}");
            }
            if text == inline {
                for how in [
                    Drive {
                        limit,
                        exact: true,
                        ..Drive::default()
                    },
                    Drive {
                        limit,
                        slice: 7,
                        ..Drive::default()
                    },
                    Drive {
                        limit,
                        grow: Some(grow),
                        ..Drive::default()
                    },
                    Drive {
                        limit,
                        exact: true,
                        charge: crate::Charge::Off,
                        slice: 3,
                        ..Drive::default()
                    },
                ] {
                    assert_eq!(
                        inlined(text, how),
                        reference(text, how),
                        "inline, limit {limit}, exact {}, slice {}, {:?}",
                        how.exact,
                        how.slice,
                        how.charge
                    );
                }
            }
        }
        for limit in [u64::MAX - 1, u64::MAX] {
            assert_eq!(
                core_run(text, limit, u64::MAX, false, None),
                whole,
                "limit {limit}"
            );
        }
    }
    assert!(crate::vm::inline::held() > held, "no region's fold held");
}

thread_local! {
    /// How many times `law.wide` and `law.sized` ran on this thread.
    static NATIVE_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// `law.wide`, whose completion formula saturates on any argument of size
/// 3 or more; `law.sized`, a native whose fee reads its argument deeply;
/// and two kernel-bodied library functions billing at completion,
/// `law.deep` returning and `law.fails` raising out of the covered
/// `law.raises` it calls, whose own native calls are covered; `law.pick`,
/// whose guard returns at once or calls `law.raises`; `law.oops`, which
/// raises unless its argument is text, billing its deep size; and
/// `law.at`, reading a list below 3 and billing its result's size; and
/// `law.first`, reading its first argument's first member and billing
/// its second's deep size; `law.holds`, `map.contains` declaring its
/// inline answer; `law.get`, reading a map under a key it holds;
/// `law.digits`, `num.to_text` declaring its inline answer; and
/// `law.str`, spelling an integer below 1000 and billing its result's
/// size.
fn huge_charges(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    ids: &std::collections::BTreeMap<&'static str, lash_kernel_value::FunctionId>,
) -> Vec<(&'static str, lash_kernel_value::FunctionId)> {
    struct Nothing;
    impl lash_kernel_doc::NativeFunction for Nothing {
        fn call(
            &self,
            _: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            NATIVE_CALLS.set(NATIVE_CALLS.get() + 1);
            Ok(lash_kernel_doc::Value::Null)
        }
    }
    /// `map.contains` over the native views, declaring its inline
    /// membership answer as the library's does.
    struct Holds;
    impl lash_kernel_doc::NativeFunction for Holds {
        fn specialization(&self) -> lash_kernel_doc::NativeSpecialization {
            use lash_kernel_doc::arena::native::{InlineNative, InlineOperation};
            lash_kernel_doc::NativeSpecialization {
                inline: Some(InlineNative {
                    native_version: lash_kernel_doc::FIRST_NATIVE_VERSION,
                    operation: InlineOperation::Membership(lash_kernel_doc::plan::Table::Map),
                }),
                ..Default::default()
            }
        }

        fn call(
            &self,
            call: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            let [target, key] = call.args.values() else {
                return Err(lash_kernel_doc::NativeError::Malformed(
                    "law.holds takes two",
                ));
            };
            let Some(table) = call.args.table(target)? else {
                return Err(lash_kernel_doc::NativeError::Malformed(
                    "law.holds takes a map",
                ));
            };
            Ok(lash_kernel_doc::Value::Bool(
                call.args.lookup(table, key)?.is_some(),
            ))
        }
    }
    /// `num.to_text` over words whose digits a short text holds, spelled
    /// by the standard formatter, declaring its inline answer as the
    /// library's does.
    struct Digits;
    impl lash_kernel_doc::NativeFunction for Digits {
        fn specialization(&self) -> lash_kernel_doc::NativeSpecialization {
            use lash_kernel_doc::arena::native::{
                InlineNative, InlineOperation, IntegerTextDomain,
            };
            lash_kernel_doc::NativeSpecialization {
                inline: Some(InlineNative {
                    native_version: lash_kernel_doc::FIRST_NATIVE_VERSION,
                    operation: InlineOperation::IntegerText(IntegerTextDomain::Decimal),
                }),
                ..Default::default()
            }
        }

        fn call(
            &self,
            call: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            let [lash_kernel_doc::Value::Int(word)] = call.args.values() else {
                return Err(lash_kernel_doc::NativeError::Malformed(
                    "law.digits takes a word",
                ));
            };
            lash_kernel_doc::arena::ShortText::new(&word.to_string())
                .map(lash_kernel_doc::Value::Text)
                .ok_or(lash_kernel_doc::NativeError::Malformed(
                    "law.digits spells short words",
                ))
        }
    }
    let mut registered: Vec<(&'static str, lash_kernel_value::FunctionId)> = Vec::new();
    for (name, source, native) in [
        (
            "law.wide",
            "function law.wide(x: Any) -> Any\nkernel 1\ncharge product(size(x), 9223372036854775807)\nnative\n",
            true,
        ),
        (
            "law.sized",
            "function law.sized(xs: Any) -> Any\nkernel 1\ncharge sum(1, deep(xs))\nnative\n",
            true,
        ),
        (
            "law.deep",
            "function law.deep(xs: Any) -> Any\nkernel 1\ncharge sum(3, deep(xs), size(result))\n{uses}body { let n = list.len(xs[\"a\"]) return [n, xs] }",
            false,
        ),
        (
            "law.raises",
            "function law.raises(xs: Any) -> Any\nkernel 1\ncharge 0\n{uses}body { let n = law.sized(1) let bad = text.concat(1, \"x\") return bad }",
            false,
        ),
        (
            "law.fails",
            "function law.fails(xs: Any) -> Any\nkernel 1\ncharge sum(5, deep(xs), size(result))\n{uses}body { let n = list.len(xs) let bad = invoke law.raises(xs) return bad }",
            false,
        ),
        (
            "law.pick",
            "function law.pick(x: Any) -> Any\nkernel 1\ncharge sum(2, size(x), size(result))\n{uses}body { if num.lt(x, 2) { let r = invoke law.raises(x) return r } return num.add(x, 1) }",
            false,
        ),
        (
            "law.oops",
            "function law.oops(x: Any) -> Any\nkernel 1\ncharge sum(4, deep(x))\n{uses}body { let t = text.concat(x, \"!\") return t }",
            false,
        ),
        (
            "law.at",
            "function law.at(xs: Any, i: Any) -> Any\nkernel 1\ncharge sum(2, size(result))\n{uses}body { if num.lt(i, 3) { return xs[i] } return i }",
            false,
        ),
        (
            "law.first",
            "function law.first(xs: Any, ys: Any) -> Any\nkernel 1\ncharge sum(2, deep(ys))\n{uses}body { return xs[0] }",
            false,
        ),
        (
            "law.holds",
            "function law.holds(m: Any, k: Any) -> Bool\nkernel 1\ncharge 1\nnative\n",
            true,
        ),
        (
            "law.digits",
            "function law.digits(x: Any) -> Text\nkernel 1\ncharge size(result)\nnative\n",
            true,
        ),
        (
            "law.str",
            "function law.str(x: Any) -> Any\nkernel 1\ncharge sum(8, deep(x), size(result))\n{uses}body { if num.lt(x, 1000) { return law.digits(x) } return \"big\" }",
            false,
        ),
        (
            "law.get",
            "function law.get(m: Any, k: Any) -> Any\nkernel 1\ncharge sum(6, deep(k), size(result))\n{uses}body { if law.holds(m, k) { return m[k] } return 0 }",
            false,
        ),
    ] {
        let mut uses = format!(
            "use list.len = @{}\nuse text.concat = @{}\nuse num.add = @{}\nuse num.lt = @{}\n",
            ids["list.len"], ids["text.concat"], ids["num.add"], ids["num.lt"]
        );
        for (name, id) in &registered {
            uses.push_str(&format!("use {name} = @{id}\n"));
        }
        let source = source.replace("{uses}", &uses);
        let definition = lash_kernel_doc::parse_definition(&source).unwrap();
        let native =
            native.then(|| match name {
                "law.holds" => std::sync::Arc::new(Holds)
                    as std::sync::Arc<dyn lash_kernel_doc::NativeFunction>,
                "law.digits" => std::sync::Arc::new(Digits)
                    as std::sync::Arc<dyn lash_kernel_doc::NativeFunction>,
                _ => std::sync::Arc::new(Nothing)
                    as std::sync::Arc<dyn lash_kernel_doc::NativeFunction>,
            });
        registered.push((name, registry.register(definition, native).unwrap()));
    }
    registered
}

/// Contract §3 and `K-CHG-003` at the top of `u64`. A `metered u64::MAX`
/// loop test shares its block with one-unit events, so the block's fixed
/// sum passes `u64::MAX` and the block runs event by event: at every limit
/// it refuses where the exact path does, and at `u64::MAX` it refuses the
/// test with only what preceded it charged. `law.wide("abc")`'s formula
/// saturates to `u64::MAX` (a wrapping one is `2^63 - 3`, which fits), so
/// it too refuses at every limit with only what preceded it charged.
#[test]
fn a_block_past_u64_and_a_saturating_fee_refuse_where_the_exact_path_does() {
    let refused = |limit: u64| {
        format!(
            "Error(Bound(BoundExceeded {{ bound: Charge, limit: {limit}, function: Some(FunctionName(\"main\")), site: Some(Site {{ unit: Main, path: [] }}) }}))"
        )
    };
    let documents = [
        "main {\n  let n = 0\n  while num.lt(n, 2) metered 18446744073709551615 {\n    set n = num.add(n, 1)\n  }\n  return n\n}",
        "main {\n  do invoke law.wide(\"abc\")\n  return 4\n}",
    ];
    for text in documents {
        let before = core_run(text, 1_000, u64::MAX, true, None);
        let pre = before.1.unwrap();
        assert!(pre < 10, "{before:?}");
        for limit in (0..=pre + 1).chain([1_000, 1 << 63, u64::MAX - 1, u64::MAX]) {
            let exact = core_run(text, limit, u64::MAX, true, None);
            let end = if limit < pre {
                exact.0.clone()
            } else {
                refused(limit)
            };
            assert_eq!(
                exact,
                (end, Some(pre.min(limit)), Vec::new()),
                "limit {limit}"
            );
            assert_eq!(
                core_run(text, limit, u64::MAX, false, None),
                exact,
                "limit {limit}"
            );
            assert_eq!(
                core_run(text, limit, 1, false, None),
                exact,
                "limit {limit}, sliced"
            );
        }
    }
}

/// A run over mutable lists and maps that a cold run faults on again and
/// again: index and loop reads, a native fee and two library completions
/// reading cold sizes, a raise unwinding through an uncovered completion,
/// and a two-argument `perform` copying both out. `law.deep` and
/// `law.oops` run inline: a cold index inside the region, a cold return
/// fee and a cold fee of a raise leaving it. `law.at`'s fold reads a cold
/// element, declines, and its region reads it again. `law.first`'s fold
/// holds over the list its statement loaded, and its completion reads the
/// cold map: the completion runs again at the region's leave. `law.get`'s
/// fold reads the cold map, declines, and its region reads it again.
const COLD_CONTRACT: &str = r#"import 3 operation host.pair(a: Any, b: Any) -> Any
main {
  let xs = [1, 2, 3]
  let m = map{"a": xs, "b": [4, 5]}
  let total = 0
  let i = 0
  while num.lt(i, 3) {
    let n = law.sized(m)
    let d = invoke law.deep(m)
    set total = num.add(total, num.add(xs[1], list.len(m["b"])))
    set total = num.add(total, invoke law.at(xs, 1))
    set total = num.add(xs[0], invoke law.first(xs, m))
    set total = num.add(total, list.len(invoke law.get(m, "b")))
    for x in m["b"] { set total = num.add(total, x) }
    try {
      let bad = invoke law.fails(m)
    } catch e {
      set total = num.add(total, list.len(d))
    }
    try {
      let bad = invoke law.oops(m)
    } catch e {
      set total = num.add(total, 1)
    }
    set i = num.add(i, 1)
  }
  let echoed = perform 3(xs, m) as Any
  print (total, echoed)
  return [total, xs, m]
}"#;

/// Astra cold path, items 1-10: a run whose objects are paged out at
/// every statement boundary shows its host what the resident run does,
/// on the prepaid and exact paths, under Exact and Off, at every charge
/// limit, in slices, when a limiter grows a refused dynamic fee and with
/// cancellation latched across every load. Each fault loads and re-runs
/// only what was not committed: no event, fee, slice test or step twice.
/// Its covered bodies run inline, and both runs show what the run calling
/// them through their frames does.
#[test]
fn a_run_paged_out_at_every_statement_shows_its_host_what_a_resident_run_does() {
    let held = crate::vm::inline::held();
    let both = |how: Drive<'_>| {
        let resident = inlined(COLD_CONTRACT, how);
        let cold = drive(COLD_CONTRACT, Drive { cold: true, ..how });
        assert_eq!(
            cold, resident,
            "cold, exact {}, {:?}",
            how.exact, how.charge
        );
        assert_eq!(
            reference(COLD_CONTRACT, Drive { cold: true, ..how }),
            resident,
            "framed, exact {}, {:?}",
            how.exact,
            how.charge
        );
        resident
    };
    for charge in [crate::Charge::Exact, crate::Charge::Off] {
        for exact in [true, false] {
            for slice in [u64::MAX, 7, 1] {
                for cancel in [false, true] {
                    both(Drive {
                        exact,
                        charge,
                        slice,
                        cancel,
                        ..Drive::default()
                    });
                }
            }
        }
    }
    let whole = both(Drive {
        exact: true,
        ..Drive::default()
    });
    assert!(whole.end.starts_with("List"), "{whole:?}");
    assert_eq!(whole.requests.len(), 1, "{whole:?}");
    let total = whole.charged.expect("charge is metered");
    let grow = total / 3 + 1;
    for limit in 0..=total + 1 {
        for exact in [true, false] {
            both(Drive {
                limit,
                exact,
                ..Drive::default()
            });
            both(Drive {
                limit,
                exact,
                slice: 7,
                ..Drive::default()
            });
            both(Drive {
                limit,
                exact,
                grow: Some(grow),
                ..Drive::default()
            });
        }
    }
    assert!(crate::vm::inline::held() > held, "no region's fold held");
}

/// Astra cold path, item 10: a cold load the memory bound refuses, after
/// the collection that frees the run's garbage, refuses at the load's own
/// event with exactly the charge before it, on the exact and the prepaid
/// path: the block's reservation is cancelled once.
///
/// `xs` and `ys` fit one at a time, each beside the other's proxy, but
/// loading `xs` back beside `ys`'s proxy does not fit, at some bound. A
/// bound refuses that load where the same run reading `ys` instead, which
/// fits wherever `let ys` did, finishes.
/// By `K-CHG-002` the run has charged, up to and including the index
/// node that loads `xs`:
/// `let xs` 1 + 9 nodes + 8 members = 18; `let ys` 1 + 3 + 2 = 6;
/// `let junk` 1 + 4 + 3 = 8; `set junk` 1 + 1 = 2; then `let n`'s
/// statement 1, `xs` 1, `0` 1 and the index 1: 38.
#[test]
fn a_cold_load_refused_for_memory_refuses_at_its_event_with_the_charge_before_it() {
    const BODY: &str = r#"main {
  let xs = [1, 2, 3, 4, 5, 6, 7, 8]
  let ys = [1, 2]
  let junk = [1, 2, 3]
  set junk = 0
  let n = READ
  return [n, ys]
}"#;
    let (text, unread) = (BODY.replace("READ", "xs[0]"), BODY.replace("READ", "ys[0]"));
    let mut refused = 0;
    for memory in 0..=16384 {
        let how = Drive {
            memory,
            cold: true,
            ..Drive::default()
        };
        if !drive(&unread, how).end.starts_with("List") {
            continue;
        }
        let exact = drive(&text, Drive { exact: true, ..how });
        let prepaid = drive(&text, how);
        if exact.end.starts_with("List") {
            assert_eq!(
                prepaid,
                Observed {
                    tape: None,
                    ..exact
                },
                "memory {memory}"
            );
            continue;
        }
        assert!(
            exact.end.contains("bound: Memory"),
            "memory {memory}: {exact:?}"
        );
        assert_eq!(exact.charged, Some(38), "exact, memory {memory}: {exact:?}");
        assert_eq!(prepaid.end, exact.end, "prepaid, memory {memory}");
        assert_eq!(
            prepaid.charged,
            Some(38),
            "prepaid, memory {memory}: {prepaid:?}"
        );
        refused += 1;
    }
    assert!(refused > 0, "no bound refuses the load");
}
