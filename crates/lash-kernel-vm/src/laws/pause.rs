//! Seal and open of the bytecode core (cutover contract §5–§7).
//!
//! - PAUSE-ANY: a core run sealed at every slice and every park and opened
//!   cold, each time in a machine built from a program compiled afresh,
//!   leaves what the run that never paused leaves: its end, prints,
//!   requests and charge. Each request keeps its effect identity and its
//!   wait: a reopen continues the occurrence and wait counters, never
//!   recycling one. A park is sealed either before its requests
//!   are delivered or after (the post-deliver window); a delivered
//!   outcome survives the reopen, and delivering it again is refused
//!   (contract §5).
//! - SEAL-BUDGET: each of those seals runs under a budget of one page or
//!   one byte. It yields before it produces the root, and produces it
//!   within one call per phase (the heap, the run record, the checkpoint
//!   root), so the whole seal is never the only way to finish.

use lash_kernel_doc::Datum;
use lash_kernel_state::paged::SealBudget;

use super::embedder::{ROOMY, Setup, Source, World, answer, core_program, wait_of};
use crate::{End, Machine, Step};

/// What a core run leaves observable.
#[derive(Debug, PartialEq)]
struct Left {
    end: String,
    printed: Vec<Datum>,
    /// Each request's identity, wait and arguments.
    requests: Vec<(String, String, Vec<Datum>)>,
    charged: Option<u64>,
}

/// Where a run pauses.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Pause {
    Never,
    /// At every slice, and at every park before it is delivered.
    BeforeDelivery,
    /// At every slice, and at every park once it is delivered.
    AfterDelivery,
}

/// Runs `text` on the core in slices of `slice`, sealing it where `pause`
/// says and going on in a machine opened cold from the root. How many
/// times it paused, and what it left.
fn run(text: &str, slice: u64, pause: Pause) -> (usize, Left) {
    let source = Source::default();
    let mut machine = <crate::CoreMachine as Machine>::start(
        core_program(text, None),
        ROOMY,
        Setup::default().start,
        Box::new(source.clone()),
    )
    .unwrap();
    let mut world = World::default();
    let (mut paused, mut requests) = (0, Vec::new());
    let budget = match pause {
        Pause::AfterDelivery => SealBudget {
            pages: u64::MAX,
            bytes: 1,
        },
        _ => SealBudget {
            pages: 1,
            bytes: u64::MAX,
        },
    };
    let mut reopen = |machine: &mut crate::CoreMachine| {
        let mut calls = 0;
        let sealed = loop {
            calls += 1;
            let sealed = machine
                .seal(budget)
                .unwrap_or_else(|error| panic!("{text}\nseal at pause {paused}: {error:?}"));
            if let Some(sealed) = sealed {
                break sealed;
            }
        };
        assert!(
            (2..=3).contains(&calls),
            "{text}\nat pause {paused}, a seal under {budget:?} took {calls} calls"
        );
        // What a node commits is the root's encoding.
        let root = lash_kernel_value::tokens::decode_body(&lash_kernel_value::tokens::encode_body(
            crate::kernel_internals::sealed_root(&sealed),
        ))
        .unwrap();
        *machine = <crate::CoreMachine as Machine>::open(
            core_program(text, None),
            ROOMY,
            &lash_kernel_state::kernel_internals::checkpoint(root),
            Box::new(source.clone()),
        )
        .unwrap_or_else(|error| panic!("{text}\nat pause {paused}: {error:?}"));
        paused += 1;
    };
    let end = loop {
        match machine.run(&mut world, slice).unwrap() {
            Step::Ended(End::Finished(finished)) => break format!("{:?}", finished.result),
            Step::Ended(other) => break format!("{other:?}"),
            Step::Slice => {
                if pause != Pause::Never {
                    reopen(&mut machine);
                }
            }
            Step::Parked(park) => {
                if pause == Pause::BeforeDelivery {
                    reopen(&mut machine);
                }
                for request in &park.requests {
                    let crate::Request::Effect(effect) = request;
                    requests.push((
                        format!("{:?}", effect.identity),
                        format!("{:?}", wait_of(request)),
                        effect.args.clone(),
                    ));
                    machine.deliver(wait_of(request), answer(request)).unwrap();
                }
                if pause == Pause::AfterDelivery && !park.requests.is_empty() {
                    reopen(&mut machine);
                    for request in &park.requests {
                        assert!(
                            matches!(
                                machine.deliver(wait_of(request), answer(request)),
                                Err(crate::DeliverError::AlreadyDelivered { .. })
                            ),
                            "{text}\na second delivery across a reopen is refused"
                        );
                    }
                }
            }
        }
    };
    let left = Left {
        end,
        printed: world.printed,
        requests,
        charged: machine.meters().charged,
    };
    (paused, left)
}

/// Runs whose frames stand at every kind of point: statements and loop
/// back edges, calls under a callee, a `try` handler, an action ready and
/// issued, a spawned task and a join, a cancelled task's cleanup, and a
/// variable a closure shares, held in its frame as the cell itself. The
/// closure performs with a list it holds, cold after each reopen: the
/// `perform` that loads it runs again, and must name the effect as the
/// unpaused run does.
fn cases() -> Vec<String> {
    let g = "definition \"function g(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let v = perform 0(x) as Any return (x, v) }\"\n";
    let sq = "definition \"function sq(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { return num.add(x, x) }\"\n";
    vec![
        format!(
            "{sq}main {{\n  let s = \"\"\n  let total = 0\n  let i = 0\n  while num.lt(i, 6) {{\n    set s = text.concat(s, \"ab\")\n    let q = call sq(i)\n    set total = num.add(total, q)\n    let held = [i, total, (i, s)]\n    set i = num.add(i, 1)\n  }}\n  print total\n  return (s, total)\n}}"
        ),
        "main {\n  let caught = 0\n  let i = 0\n  while num.lt(i, 4) {\n    try {\n      let x = num.add(i, 1)\n      let bad = text.concat(x, \"x\")\n      set caught = num.add(caught, 100)\n    } catch e {\n      set caught = num.add(caught, 1)\n    }\n    set i = num.add(i, 1)\n  }\n  print caught\n  return caught\n}".into(),
        format!(
            "{g}main {{\n  let out = []\n  for x in [7, 8] {{\n    let r = (x, call g(x), call g(num.add(x, 1)))\n    set out = (out, r)\n  }}\n  print out\n  return out\n}}"
        ),
        super::scheduler::document(false),
        super::scheduler::document(true),
        "main {\n  let k = 5\n  let f = fn(x) { let v = perform 0(x) as Any return (v, k) }\n  let a = apply f([1, k])\n  set k = 6\n  let b = apply f([2, k])\n  print (a, b)\n  return (a, b)\n}".into(),
    ]
}

/// PAUSE-ANY: the uninterrupted run is the oracle; every slice length
/// pauses the run at other points, and each reopen compiles the document
/// again.
#[test]
fn a_core_run_sealed_and_opened_cold_at_every_pause_leaves_what_an_unpaused_run_leaves() {
    for text in cases() {
        let (_, whole) = run(&text, u64::MAX, Pause::Never);
        for slice in [u64::MAX, 1, 2, 5] {
            for pause in [Pause::BeforeDelivery, Pause::AfterDelivery] {
                let (paused, left) = run(&text, slice, pause);
                assert_eq!(
                    left, whole,
                    "{text}\nslice {slice}, {pause:?}, {paused} pauses"
                );
            }
        }
    }
}
