//! The session laws of the bytecode core (cutover contract §6).
//!
//! - `K-SES-001`: a cell's session is the session it started with, plus
//!   the non-private top-level variables of its `main`, less the host's
//!   bindings. A variable declared in a nested block is not one.
//! - `K-SES-002`: two bindings that share an object share it in the next
//!   cell.
//! - `K-SES-003`: a binding whose value reaches a closure, a function or a
//!   task is not carried, and is named with why, however deep the value
//!   holds it, a large list's last chunk included.
//! - A binding the cell never names rides through a seal at a park and a
//!   cold reopen.

use lash_kernel_doc::Datum;
use lash_kernel_state::kernel_internals::tree::Access;
use lash_kernel_state::paged::run::session;
use lash_kernel_state::paged::{SealBudget, SessionRoot};
use lash_kernel_value::names::Name;

use super::embedder::{ROOMY, Setup, Source, World, answer, core_program, int, wait_of};
use crate::durable::session::NOT_CARRIED;
use crate::{CellStart, CoreMachine, End, Finished, Machine, Step};

fn start(session: Option<SessionRoot>, host: Vec<(Name, Datum)>) -> CellStart {
    CellStart {
        session,
        host,
        ..Setup::default().start
    }
}

fn finished(end: End) -> Finished {
    match end {
        End::Finished(finished) => *finished,
        other => panic!("the cell did not finish: {other:?}"),
    }
}

/// Runs `text` as a session cell over `source` to its end, answering each
/// park.
fn cell(text: &str, start: CellStart, source: &Source) -> Finished {
    let mut machine = <CoreMachine as Machine>::start(
        core_program(text, None),
        ROOMY,
        start,
        Box::new(source.clone()),
    )
    .unwrap_or_else(|error| panic!("{error}\n{text}"));
    drive(&mut machine)
}

fn drive(machine: &mut CoreMachine) -> Finished {
    loop {
        match machine.run(&mut World::default(), u64::MAX).unwrap() {
            Step::Ended(end) => return finished(end),
            Step::Slice => {}
            Step::Parked(park) => {
                for request in &park.requests {
                    machine.deliver(wait_of(request), answer(request)).unwrap();
                }
            }
        }
    }
}

/// The names `finished` carries, read from its session page.
fn carried(finished: &Finished, source: &Source) -> Vec<String> {
    let page = finished
        .carry
        .as_ref()
        .and_then(SessionRoot::run_page)
        .expect("a main cell carries a bytecode-core session");
    let access = Access::new(Box::new(source.clone()));
    session::read(&access, page)
        .unwrap()
        .names
        .iter()
        .map(|name| name.as_str().to_owned())
        .collect()
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// `K-SES-001`, `K-SES-002`: the top-level variables carry, the nested
/// and the host's do not, and an alias still names its object's one copy
/// a cell later.
#[test]
fn top_level_variables_carry_and_aliases_stay_shared() {
    let source = Source::default();
    let first = cell(
        "main {\n  let xs = [1]\n  let ys = xs\n  let n = 3\n  let k = h\n  if true { let inner = 1 }\n  return 0\n}",
        start(None, vec![(Name::new("h"), int(9))]),
        &source,
    );
    assert!(first.not_carried.is_empty(), "{:?}", first.not_carried);
    assert_eq!(carried(&first, &source), names(&["k", "n", "xs", "ys"]));
    let second = cell(
        "main {\n  set xs[0] = 5\n  return [ys[0], n, k]\n}",
        start(first.carry, Vec::new()),
        &source,
    );
    assert_eq!(second.result, Datum::List(vec![int(5), int(3), int(9)]));
}

/// `K-SES-003`: each binding that reaches a closure or a task is named
/// with why and not carried; the data beside them carries.
#[test]
fn bindings_reaching_a_closure_or_a_task_are_dropped_and_named() {
    let source = Source::default();
    let text = "definition \"function work() -> Any\\nkernel 1\\nmetering statements\\nbody { return 1 }\"\n\
main {\n  let data = [1, [2]]\n  let f = fn() { return 1 }\n  let fs = [[f]]\n  let t = spawn call work()\n  let v = join t\n  let ts = [t]\n  let big = []\n  let i = 0\n  while num.lt(i, 300) {\n    set big[list.len(big)] = i\n    set i = num.add(i, 1)\n  }\n  set big[list.len(big)] = t\n  let wide = []\n  set i = 0\n  while num.lt(i, 300) {\n    set wide[list.len(wide)] = i\n    set i = num.add(i, 1)\n  }\n  return v\n}";
    let end = cell(text, start(None, Vec::new()), &source);
    let dropped: Vec<(String, String)> = end
        .not_carried
        .iter()
        .map(|(name, why)| (name.as_str().to_owned(), why.clone()))
        .collect();
    let expected: Vec<(String, String)> = ["big", "f", "fs", "t", "ts"]
        .iter()
        .map(|name| ((*name).to_owned(), NOT_CARRIED.to_owned()))
        .collect();
    assert_eq!(dropped, expected);
    assert_eq!(carried(&end, &source), names(&["data", "i", "v", "wide"]));
}

/// A binding the cell never names rides through a seal at its park and a
/// cold reopen, and the next cell reads it.
#[test]
fn an_untouched_binding_survives_a_seal_and_a_cold_reopen() {
    let source = Source::default();
    let first = cell(
        "main {\n  let xs = [1, 2]\n  let n = 3\n  return 0\n}",
        start(None, Vec::new()),
        &source,
    );
    let text = "main {\n  let v = perform 0(7) as Any\n  set n = num.add(n, 1)\n  return v\n}";
    let mut machine = <CoreMachine as Machine>::start(
        core_program(text, None),
        ROOMY,
        start(first.carry, Vec::new()),
        Box::new(source.clone()),
    )
    .unwrap();
    let Step::Parked(park) = machine.run(&mut World::default(), u64::MAX).unwrap() else {
        panic!("the cell never parks");
    };
    let sealed = machine
        .seal(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        })
        .unwrap()
        .unwrap();
    let root = crate::kernel_internals::sealed_root(&sealed).clone();
    drop(machine);
    let mut machine = <CoreMachine as Machine>::open(
        core_program(text, None),
        ROOMY,
        &lash_kernel_state::kernel_internals::checkpoint(root),
        Box::new(source.clone()),
    )
    .unwrap();
    for request in &park.requests {
        machine.deliver(wait_of(request), answer(request)).unwrap();
    }
    let second = drive(&mut machine);
    assert_eq!(carried(&second, &source), names(&["n", "v", "xs"]));
    let third = cell(
        "main {\n  return [xs[1], n]\n}",
        start(second.carry, Vec::new()),
        &source,
    );
    assert_eq!(third.result, Datum::List(vec![int(2), int(4)]));
}

/// A host's diff of two sessions never leaves out a binding that differs:
/// a rebind, a new binding, and an object mutated in place under every
/// binding that shares it; a binding whose value and rows are as they
/// were is not reported.
#[test]
fn a_session_diff_reports_rebinds_and_in_place_mutations() {
    let source = Source::default();
    let first = cell(
        "main {\n  let xs = [1]\n  let ys = xs\n  let n = 3\n  let m = [4]\n  return 0\n}",
        start(None, Vec::new()),
        &source,
    );
    let second = cell(
        "main {\n  set xs[0] = 5\n  set n = 7\n  let fresh = 1\n  return 0\n}",
        start(first.carry.clone(), Vec::new()),
        &source,
    );
    let editor =
        crate::SessionEditor::open(second.carry.as_ref(), Box::new(source.clone())).unwrap();
    let changed: Vec<(String, (bool, bool))> = editor
        .changed(first.carry.as_ref(), second.carry.as_ref().unwrap())
        .unwrap()
        .into_iter()
        .map(|(name, held)| (name.as_str().to_owned(), held))
        .collect();
    assert_eq!(
        changed,
        vec![
            ("fresh".to_owned(), (false, true)),
            ("n".to_owned(), (true, true)),
            ("xs".to_owned(), (true, true)),
            ("ys".to_owned(), (true, true)),
        ]
    );
}
