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
    sealed_in_layout(text, bounds, which, limiter, crate::Layout::default())
}

fn sealed_in_layout(
    text: &str,
    bounds: Bounds,
    which: Which,
    limiter: Option<&Scripted>,
    layout: crate::Layout,
) -> (Trace, Option<(u64, usize)>) {
    let mut embedder = Embedder::with(
        text,
        Setup {
            bounds,
            layout,
            probes: Some(super::layout::scalar_probes),
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
                let root = embedder.reopen(layout, true);
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
            let (reference, _) = sealed_in_layout(
                text,
                started,
                Which::Charge,
                None,
                crate::Layout(0).without_regions(),
            );
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
    let (root, certificate) = embedder.checkpoint();
    assert_eq!(root.limits.charge, total / 2);
    let pending = embedder.pending.clone();
    let mut attempts = Vec::new();
    for _ in 0..2 {
        let limiter = Scripted::answering([LimitDecision::Grow(GROW)]);
        embedder.open_at(&root, certificate, crate::Layout::default(), true);
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

/// FIG-6768 law 3: a run whose conservative count passes the memory limit
/// while the exact count of what it holds fits recounts, frees what
/// nothing holds and runs on, and never asks its limiter.
#[test]
fn an_exact_count_that_fits_never_asks_the_memory_limiter() {
    let limiter = Scripted::default();
    let mut embedder = Embedder::with(
        super::bounds::MEMORY_CHURN,
        Setup {
            bounds: Bounds {
                memory: 64 << 10,
                ..ROOMY
            },
            ..Setup::default()
        },
    );
    limited(&mut embedder, Which::Memory, Some(&limiter));
    crate::perf_witness::enable_work();
    let end = embedder.run_to_end(&[]);
    let work = crate::perf_witness::take_work().unwrap();
    crate::perf_witness::disable_work();
    assert_eq!(result(end), int(200));
    assert!(
        work.heap.refinement_attempts > 0,
        "the conservative count must pass the limit"
    );
    assert_eq!(limiter.asked(), [], "the limiter was asked");
}

fn refused(text: &str, bounds: Bounds, which: Which, limiter: Option<&Scripted>) -> (Trace, End) {
    let mut embedder = Embedder::with(
        text,
        Setup {
            bounds,
            ..Setup::default()
        },
    );
    limited(&mut embedder, which, limiter);
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
