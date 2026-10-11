//! Observing a run: its work counters account for every unit of charge
//! once, observing changes nothing the run can observe, and a profile of
//! sampled contexts accounts for every tick.

use std::sync::Arc;

use lash_kernel_doc::{Datum, ErrorDatum, Handle, Integer, Timestamp};
use lash_kernel_state::ParkedRun;

use super::embedder::{Embedder, ROOMY, Setup};
use crate::observe::{CodeKind, GuestContext, GuestProfile, LibKind, WorkReport};
use crate::{Bounds, End, Host, Layout, Machine, Outcome, Park, Request, Step};

/// Declared functions, closures, tasks, effects in a loop, `try` with
/// `catch` and `finally`, natives on their fast path and their general
/// call, a guarded native, a native that builds an object, kernel bodies
/// that call closures, and memory pressure that collects mid-call.
const PROGRAM: &str = r#"
fn helper(x) { return num.add(x, 1) }
fn worker(tag) {
  let total = 0
  for step in [1, 2] {
    try {
      let got = perform echo(tag) as Any
      set total = num.add(total, step)
      let broken = text.concat(total, step)
    } catch e {
      print e.kind
    } finally {
      print (tag, step)
    }
  }
  return total
}
main {
  let a = call helper(1)
  let b = call helper(a)
  let spun = work.spin(3)
  let filled = work.fill(2)
  let hs = []
  for tag in ["x", "y"] {
    let h = spawn call worker(tag)
    set hs[list.len(hs)] = h
  }
  let results = join all hs
  let bump = fn(n) { return num.add(n, 2) }
  let twice = invoke each.twice(bump, b)
  let xs = [1, 2, 3]
  let mapped = invoke each.map(bump, xs)
  let doubled = invoke pair.twice(21)
  let i = 0
  while num.lt(i, 40) {
    let pad = work.fill(500)
    set i = num.add(i, 1)
  }
  return (a, b, spun, results, twice, mapped, doubled, i)
}
"#;

const PRESSED: Bounds = Bounds {
    memory: 24 << 10,
    ..ROOMY
};

fn setup(layout: Layout, slice: u64) -> Setup {
    Setup {
        bounds: PRESSED,
        layout,
        slice,
        ..Setup::default()
    }
}

/// What a run shows its embedder: every step, each park's exported state,
/// what it printed and requested, and its charge.
#[derive(Debug, PartialEq)]
struct Seen {
    steps: Vec<Step>,
    exports: Vec<ParkedRun>,
    printed: Vec<Datum>,
    requests: Vec<Request>,
    charged: u64,
}

/// How a run is observed.
#[derive(Clone, Copy, Debug)]
enum Observing {
    Nothing,
    Work,
    Context,
    Both,
}

fn run(setup: Setup, observing: Observing) -> (Seen, Option<WorkReport>) {
    let mut embedder = Embedder::with(PROGRAM, setup);
    let context = GuestContext::new();
    if matches!(observing, Observing::Work | Observing::Both) {
        embedder.machine.count_work();
    }
    if matches!(observing, Observing::Context | Observing::Both) {
        embedder.machine.publish_context(Arc::clone(&context));
    }
    let mut seen = Seen {
        steps: Vec::new(),
        exports: Vec::new(),
        printed: Vec::new(),
        requests: Vec::new(),
        charged: 0,
    };
    loop {
        let step = embedder.run();
        seen.steps.push(step.clone());
        match step {
            Step::Ended(_) => break,
            Step::Slice => {}
            Step::Parked(_) => {
                seen.exports.push(embedder.machine.export().unwrap());
                let next = super::embedder::label(&embedder.pending[0]);
                embedder.deliver(&next);
            }
        }
    }
    seen.printed = std::mem::take(&mut embedder.world.printed);
    seen.requests = std::mem::take(&mut embedder.requests);
    seen.charged = embedder.machine.meters().charged;
    (seen, embedder.machine.work())
}

/// Observing a run, by counting its work or publishing its context or
/// both, leaves its results, failures, charges, parks, prints, requests
/// and exported states as they are: under the natural layout and one that
/// takes every general native call, whole and in slices.
#[test]
fn observing_a_run_changes_nothing_it_can_observe() {
    for layout in [Layout::default(), Layout(1)] {
        for slice in [u64::MAX, 7] {
            let (plain, none) = run(setup(layout, slice), Observing::Nothing);
            assert!(none.is_none());
            assert!(matches!(
                plain.steps.last(),
                Some(Step::Ended(End::Finished(_)))
            ));
            assert!(plain.exports.len() >= 4, "the run parks at each echo");
            for observing in [Observing::Work, Observing::Context, Observing::Both] {
                let (observed, _) = run(setup(layout, slice), observing);
                assert_eq!(
                    observed, plain,
                    "{observing:?} under layout {layout:?} in slices of {slice}"
                );
            }
        }
    }
}

/// The self charges of every function, with the charge no function was
/// running for, add up to the run's charge with nothing left over, and
/// the heap insertions to the objects the run made; every unit of charge
/// has a function. A call's inclusive charge is its own and its callees'.
#[test]
fn work_counters_attribute_every_unit_of_charge_once() {
    for layout in [Layout::default(), Layout(1)] {
        for slice in [u64::MAX, 7] {
            let (seen, work) = run(setup(layout, slice), Observing::Work);
            let work = work.unwrap();
            assert_eq!(work.charged, seen.charged);
            assert_eq!(work.residual(), 0, "layout {layout:?}, slices of {slice}");
            assert_eq!(work.insertion_residual(), 0);
            assert_eq!(work.unattributed_charge, 0);

            let code = |name: &str| {
                work.codes
                    .iter()
                    .find(|code| code.symbol.name == name)
                    .unwrap_or_else(|| panic!("no row for {name}"))
            };
            let native = |name: &str| {
                work.natives
                    .iter()
                    .find(|native| native.symbol.name == name)
                    .unwrap_or_else(|| panic!("no row for {name}"))
            };
            // `main` is the outermost frame of the task every other frame
            // of it is above: everything its task was charged.
            let main = code("main");
            assert_eq!(main.symbol.kind, CodeKind::Main);
            assert_eq!(
                main.counts.calls, 0,
                "main's frame is the start's, not a call"
            );
            assert!(main.counts.inclusive_charge >= main.counts.self_charge);
            // Two calls of `helper`, one statement each, each calling
            // `num.add` once, charged 1.
            let helper = code("helper");
            assert_eq!(helper.counts.calls, 2);
            assert_eq!(helper.counts.statements, 2);
            let add = native("num.add");
            assert_eq!(add.symbol.kind, LibKind::Native);
            assert!(helper.native_calls.contains(&(add.symbol.token, 2)));
            assert_eq!(
                helper.counts.inclusive_charge,
                helper.counts.self_charge + 2,
                "a call's inclusive charge is its own and its native callee's"
            );
            // The two workers are tasks of their own: their charge is not
            // `main`'s.
            let worker = code("worker");
            assert_eq!(worker.counts.calls, 2);
            let tasks: u64 = main.counts.inclusive_charge + worker.counts.inclusive_charge;
            assert_eq!(
                tasks, work.charged,
                "main's task and the two workers' are the run"
            );
            // Kernel bodies run in frames of their own; the closure they
            // call is a code of its own.
            assert_eq!(code("each.twice").symbol.kind, CodeKind::LibraryBody);
            assert_eq!(code("each.map").counts.calls, 1);
            let bump = work
                .codes
                .iter()
                .find(|code| code.symbol.kind == CodeKind::Closure)
                .unwrap();
            assert_eq!(bump.counts.calls, 5);
            // The guard's units, the native's object, the caught raise.
            assert_eq!(native("work.spin").counts.guard_work, 3);
            assert_eq!(native("work.fill").counts.calls, 41);
            assert!(native("work.fill").counts.heap_insertions >= 41);
            assert_eq!(native("text.concat").counts.calls, 4);
            let fast: u64 = work.natives.iter().map(|n| n.counts.fast_calls).sum();
            assert_eq!(fast > 0, layout == Layout::default());
        }
    }
}

/// An embedder's host that reads the context each time the run prints:
/// a sample of a known place.
struct Sampling {
    context: Arc<GuestContext>,
    profile: GuestProfile,
}

impl Host for Sampling {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        0
    }

    fn read(&mut self, _handle: &Handle, request: &Datum) -> Result<Datum, ErrorDatum> {
        Ok(request.clone())
    }

    fn print(&mut self, _value: &Datum) {
        self.profile.tick(&self.context);
    }

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// The folded lines of a profile: each a stack and a count, the counts
/// adding up to the samples accepted.
fn parse(folded: &str) -> Vec<(Vec<String>, u64)> {
    folded
        .lines()
        .map(|line| {
            let (stack, count) = line.rsplit_once(' ').unwrap();
            let frames = stack.split(';').map(str::to_string).collect();
            (frames, count.parse().unwrap())
        })
        .collect()
}

/// A profile's ticks are each accepted, idle, parked, missed or lost; its
/// folded stacks parse and add up to the accepted samples; and a sample
/// shows the task's frames from the outermost, as they were when it was
/// taken.
#[test]
fn a_guest_profile_folds_and_accounts_for_every_tick() {
    let mut embedder = Embedder::new(
        r#"
fn show(x) { print x return x }
fn deeper(x) { let y = call show(x) return y }
main {
  let a = call show(1)
  let b = call deeper(2)
  let c = perform echo(3) as Any
  print c
  return (a, b, c)
}
"#,
    );
    let symbols = embedder.machine.guest_symbols();
    let context = GuestContext::new();
    embedder.machine.publish_context(Arc::clone(&context));
    let mut host = Sampling {
        context: Arc::clone(&context),
        profile: GuestProfile::new(),
    };
    // Before the first `run`, between runs at a park, and after the end.
    host.profile.tick(&context);
    let end = loop {
        match embedder.machine.run(&mut host, u64::MAX).unwrap() {
            Step::Parked(Park { requests, .. }) => {
                host.profile.tick(&context);
                for request in requests {
                    let Request::Effect(effect) = request else {
                        panic!("only effects")
                    };
                    let argument = effect.args[0].clone();
                    embedder
                        .machine
                        .deliver(effect.wait, Outcome::Completed(argument))
                        .unwrap();
                }
            }
            Step::Ended(end) => break end,
            Step::Slice => {}
        }
    };
    assert!(matches!(end, End::Finished(_)));
    host.profile.tick(&context);
    drop(embedder);
    host.profile.tick(&context);
    host.profile.missed(2);

    let counts = host.profile.completeness();
    assert!(counts.adds_up(), "{counts:?}");
    assert_eq!(counts.ticks, 9);
    assert_eq!((counts.accepted, counts.interpret), (3, 3));
    assert_eq!((counts.inactive, counts.parked, counts.missed), (3, 1, 2));
    let lines = parse(&host.profile.folded(Some("law"), &symbols));
    let total: u64 = lines.iter().map(|(_, count)| count).sum();
    assert_eq!(total, counts.accepted);
    let stacks: Vec<(Vec<&str>, u64)> = lines
        .iter()
        .map(|(frames, count)| (frames.iter().map(String::as_str).collect(), *count))
        .collect();
    assert_eq!(
        stacks,
        vec![
            (vec!["law", "main"], 1),
            (vec!["law", "main", "deeper", "show"], 1),
            (vec!["law", "main", "show"], 1),
        ]
    );
    let times = host.profile.symbol_times(&symbols);
    let show = times.iter().find(|time| time.frame == "show").unwrap();
    assert_eq!((show.self_samples, show.inclusive_samples), (2, 2));
    let main = times.iter().find(|time| time.frame == "main").unwrap();
    assert_eq!((main.self_samples, main.inclusive_samples), (1, 3));
}
