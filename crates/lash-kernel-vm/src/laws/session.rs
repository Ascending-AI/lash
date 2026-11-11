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
//! - `K-SES-004`: a saved function rides on, and a binding that holds it
//!   carries, the function by the same identity.

use lash_kernel_doc::Datum;
use lash_kernel_doc::arena::native::NewObject;
use lash_kernel_state::kernel_internals::tree::Access;
use lash_kernel_state::paged::run::directory::Directory;
use lash_kernel_state::paged::run::ids::HighWater;
use lash_kernel_state::paged::run::objects::{Record, RunValue};
use lash_kernel_state::paged::run::session::{self, RunFunction, RunSession};
use lash_kernel_state::paged::{SealBudget, SessionRoot};
use lash_kernel_value::names::Name;

use super::embedder::{ROOMY, Setup, Source, World, answer, core_program, int, wait_of};
use crate::durable::session::NOT_CARRIED;
use crate::heap::Heap;
use crate::heap::slots::SlotHeap;
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
        .map(SessionRoot::page)
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
        start(first.carry, Vec::new()),
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

/// `K-SES-004`, `K-SES-003`: a session's saved function rides a cell
/// that never names it, and a host's edit, and a binding holding it is no
/// binding that reaches a function: it carries, holding the saved
/// function's object.
#[test]
fn a_binding_holding_a_saved_function_carries_it_by_identity() {
    let source = Source::default();
    let mut heap = SlotHeap::durable(
        u64::MAX,
        crate::heap::open::Durable {
            directory: Directory::new(Box::new(source.clone())),
            high_water: HighWater(0),
        },
    );
    let mut make = |object: NewObject| {
        heap.admit(crate::heap::slots::schedule(&object))
            .expect("room");
        heap.create(object).expect("an object")
    };
    let function = make(NewObject::Function(lash_kernel_value::CallableId {
        link: lash_kernel_value::LinkId::from_bytes([4; 32]),
        function: lash_kernel_value::FunctionId::from_bytes([5; 32]),
    }));
    let held = make(NewObject::List(vec![function]));
    let saved = RunFunction {
        name: Name::new("f"),
        description: b"f()".to_vec(),
    };
    let root = heap.seal(&[held, function]).expect("a seal");
    let page = session::write(
        &Access::new(Box::new(source.clone())),
        &RunSession {
            heap: root,
            names: vec![Name::new("fs")],
            functions: vec![saved.clone()],
        },
        None,
    )
    .unwrap();
    let end = cell(
        "main {\n  let n = 1\n  return 0\n}",
        start(Some(SessionRoot::run(page)), Vec::new()),
        &source,
    );
    assert!(end.not_carried.is_empty(), "{:?}", end.not_carried);
    assert_eq!(carried(&end, &source), names(&["fs", "n"]));
    let edited = crate::SessionEditor::open(end.carry.as_ref(), Box::new(source.clone()))
        .unwrap()
        .seal()
        .unwrap();
    for session in [end.carry.unwrap(), edited] {
        let page = session.page();
        let (carried, directory) = session::open(Box::new(source.clone()), page).unwrap();
        assert_eq!(carried.functions, vec![saved.clone()]);
        let list = carried.binding(0).and_then(RunValue::id).unwrap();
        let Record::List(members) = directory.get(list).unwrap().unwrap().record else {
            panic!("`fs` holds a list");
        };
        assert_eq!(members.first(), carried.function(0));
    }
}

/// A dialect's save on the core: the dialect's rules over the sealed heap,
/// each saved function described by its name.
struct DialectSave {
    program: crate::Program,
}

impl crate::CellSave for DialectSave {
    fn save(
        &mut self,
        cell: crate::SaveCell<'_>,
    ) -> Result<crate::SavedCell, lash_kernel_doc::heap::HeapFault> {
        let saved = cell.save(crate::SaveRules {
            document: &self.program.document,
            link: self.program.link,
            documents: &std::collections::BTreeMap::new(),
            controls: &std::collections::BTreeMap::new(),
            written: &std::collections::BTreeMap::new(),
            values: &lash_kernel_doc::FunctionRepresentation::Bare,
            catalog: self.program.library.registry().as_ref(),
        })?;
        Ok(crate::SavedCell {
            functions: saved
                .functions
                .into_iter()
                .map(|function| {
                    let description = function.name.as_str().as_bytes().to_vec();
                    (function, description)
                })
                .collect(),
            carried: saved.carried.into_iter().collect(),
            refused: saved
                .refused
                .into_iter()
                .map(|(name, why)| (name, why.to_string()))
                .collect(),
        })
    }
}

/// `K-SES-004`: at a cell's end a closure `main` made is saved, and the
/// bindings holding it carry it by identity; a task is not carried, and a
/// closure whose captured variable holds one is refused, as is every
/// binding that holds that closure.
#[test]
fn a_cell_end_saves_its_closures_and_refuses_one_that_captures_a_task() {
    let source = Source::default();
    let text = "definition \"function work() -> Any\\nkernel 1\\nmetering statements\\nbody { return 1 }\"\n\
main {\n  let k = 1\n  let f = fn() { return k }\n  let fs = [f]\n  let t = spawn call work()\n  let v = join t\n  let g = fn() { return t }\n  let gs = [g]\n  return 0\n}";
    let program = core_program(text, None);
    let mut machine = <CoreMachine as Machine>::start(
        program.clone(),
        ROOMY,
        start(None, Vec::new()),
        Box::new(source.clone()),
    )
    .unwrap();
    machine.set_options(crate::MachineOptions {
        cell_save: Some(Box::new(DialectSave { program })),
        ..crate::MachineOptions::default()
    });
    let end = drive(&mut machine);
    let needs_t = lash_kernel_dialect::NotSaved::Capture {
        name: Name::new("t"),
        why: lash_kernel_dialect::CaptureRefusal::Task,
    }
    .to_string();
    let dropped: Vec<(String, String)> = end
        .not_carried
        .iter()
        .map(|(name, why)| (name.as_str().to_owned(), why.clone()))
        .collect();
    assert_eq!(
        dropped,
        vec![
            ("g".to_owned(), needs_t.clone()),
            ("gs".to_owned(), needs_t),
            (
                "t".to_owned(),
                lash_kernel_dialect::NotSaved::Task.to_string()
            ),
        ]
    );
    assert_eq!(carried(&end, &source), names(&["f", "fs", "k", "v"]));
    let page = end.carry.as_ref().map(SessionRoot::page).unwrap();
    let (session, directory) = session::open(Box::new(source.clone()), page).unwrap();
    assert_eq!(
        session.functions,
        vec![RunFunction {
            name: Name::new("f"),
            description: b"f".to_vec(),
        }]
    );
    let saved = session.function(0).and_then(RunValue::id).unwrap();
    assert_eq!(session.binding(0).and_then(RunValue::id), Some(saved));
    let list = session.binding(1).and_then(RunValue::id).unwrap();
    let Record::List(members) = directory.get(list).unwrap().unwrap().record else {
        panic!("`fs` holds a list");
    };
    assert_eq!(members.first().and_then(RunValue::id), Some(saved));
}

/// `K-SES-001` with `K-CLO-001`: a top-level variable a closure captures
/// is carried as its value, not as the cell the closure shares, and the
/// next cell reads it.
#[test]
fn a_variable_a_closure_captures_carries_its_value() {
    let source = Source::default();
    let first = cell(
        "main {\n  let o = [1]\n  let f = fn() { return o[0] }\n  let r = apply f()\n  return r\n}",
        start(None, Vec::new()),
        &source,
    );
    assert_eq!(carried(&first, &source), names(&["o", "r"]));
    let second = cell(
        "main {\n  return [o[0], r]\n}",
        start(first.carry, Vec::new()),
        &source,
    );
    assert_eq!(second.result, Datum::List(vec![int(1), int(1)]));
}
