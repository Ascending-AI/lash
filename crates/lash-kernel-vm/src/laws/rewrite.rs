//! The run-format id rewrite (cutover contract §6): succession onto
//! another document or helper library.
//!
//! - A rewrite leaves no old code id in any page the new root reaches:
//!   every id-bearing record a run holds is rewritten.
//! - A rewrite whose correspondence misses an id the run names refuses
//!   with that id, before it writes a page.
//! - A rewrite whose site correspondence drops a site of the run's own
//!   code that the run stands at or names refuses with that site, before
//!   it writes a page: a node that did not survive is never carried onto
//!   whatever node the successor has at its path.

use std::collections::BTreeSet;

use lash_kernel_doc::Datum;
use lash_kernel_doc::heap::PageId;
use lash_kernel_state::paged::run::rewrite::{
    RewriteFault, RunCorrespondence, SiteMap, Unmapped, live_code, rewrite,
};
use lash_kernel_value::LinkId;
use lash_kernel_value::names::{CallableId, FunctionId, Name};
use lash_kernel_value::sites::Unit;

use super::embedder::{ROOMY, Setup, Source, World, core_program};
use crate::{CellStart, CoreMachine, Machine, Step};

/// A run parked on a `perform` whose argument, a slot, a list's row and a
/// large list's last chunk each hold a host function of another link,
/// beside a closure of its own.
const TEXT: &str = "main {\n  let f = fn() { return 1 }\n  let g = [h]\n  let big = []\n  let i = 0\n  while num.lt(i, 300) {\n    set big[list.len(big)] = i\n    set i = num.add(i, 1)\n  }\n  set big[list.len(big)] = h\n  let v = perform 0(h) as Any\n  return [f, g, big, v]\n}";

fn flipped(bytes: &[u8; 32]) -> [u8; 32] {
    bytes.map(|byte| !byte)
}

/// Every page `from` reaches, by id.
fn reached(source: &Source, from: PageId) -> BTreeSet<PageId> {
    let pages = source.0.borrow();
    let mut seen = BTreeSet::new();
    let mut work = vec![from];
    while let Some(id) = work.pop() {
        if seen.insert(id) {
            work.extend(pages[&id].children.iter().map(|child| child.id));
        }
    }
    seen
}

/// Which of `digests` some page `from` reaches holds, byte for byte.
fn held(source: &Source, from: PageId, digests: &BTreeSet<[u8; 32]>) -> BTreeSet<[u8; 32]> {
    let pages = source.0.borrow();
    reached(source, from)
        .into_iter()
        .flat_map(|id| {
            let tokens = &pages[&id].tokens;
            digests
                .iter()
                .filter(|digest| tokens.windows(32).any(|at| at == digest.as_slice()))
                .copied()
                .collect::<Vec<_>>()
        })
        .collect()
}

/// A rewrite to images that share no byte with the ids they replace
/// leaves none of the old ids in the pages its root reaches, though the
/// old root's pages hold the host function's and the run's own; a
/// correspondence missing the host function refuses with it and writes
/// nothing.
#[test]
fn a_rewrite_leaves_no_old_code_id_and_refuses_an_unmapped_one_before_writing() {
    let foreign = CallableId {
        link: LinkId::from_bytes([4; 32]),
        function: FunctionId::from_bytes([5; 32]),
    };
    let source = Source::default();
    let mut machine = <CoreMachine as Machine>::start(
        core_program(TEXT, None),
        ROOMY,
        CellStart {
            host: vec![(Name::new("h"), Datum::Function(foreign))],
            ..Setup::default().start
        },
        Box::new(source.clone()),
    )
    .unwrap();
    let Step::Parked(_) = machine.run(&mut World::default(), u64::MAX).unwrap() else {
        panic!("the run never parks");
    };
    let sealed = machine
        .seal(lash_kernel_state::paged::SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        })
        .unwrap()
        .unwrap();
    let root = crate::kernel_internals::sealed_root(&sealed).clone();
    drop(machine);

    let mut pages = source.clone();
    let live = live_code(&mut pages, &root).unwrap();
    let mut map = RunCorrespondence::default();
    let mut old = BTreeSet::new();
    for link in &live.links {
        map.links
            .insert(*link, LinkId::from_bytes(flipped(link.as_bytes())));
        old.insert(*link.as_bytes());
    }
    for function in live
        .functions
        .iter()
        .chain(live.callables.iter().map(|callable| &callable.function))
    {
        map.functions.insert(
            *function,
            FunctionId::from_bytes(flipped(function.as_bytes())),
        );
        old.insert(*function.as_bytes());
    }
    for callable in &live.callables {
        map.callables.insert(
            *callable,
            CallableId {
                link: map.links[&callable.link],
                function: map.functions[&callable.function],
            },
        );
    }
    let before = held(&source, root.heap.id, &old);
    assert!(
        [
            *root.link.as_bytes(),
            *foreign.link.as_bytes(),
            *foreign.function.as_bytes()
        ]
        .iter()
        .all(|digest| before.contains(digest)),
        "the old run holds the ids the rewrite must replace"
    );

    let mut missing = map.clone();
    missing.callables.remove(&foreign);
    let written = source.0.borrow().len();
    let refused = rewrite(&mut pages, &root, &missing);
    assert!(
        matches!(
            refused,
            Err(RewriteFault::Unmapped(Unmapped::Callable(callable))) if callable == foreign
        ),
        "{refused:?}"
    );
    assert_eq!(source.0.borrow().len(), written, "a refusal wrote a page");

    let dropped = live
        .sites
        .iter()
        .filter(|site| site.unit == Unit::Main)
        .max()
        .cloned()
        .expect("the parked run stands in its main");
    let mut moved = map.clone();
    moved.sites = Some(SiteMap::new(
        lash_kernel_doc::Correspondence::identity(
            root.document,
            live.sites.iter().filter(|site| **site != dropped).cloned(),
        )
        .unwrap(),
    ));
    let refused = rewrite(&mut pages, &root, &moved);
    assert!(
        matches!(
            &refused,
            Err(RewriteFault::Unmapped(Unmapped::Site(site))) if *site == dropped
        ),
        "{refused:?}"
    );
    assert_eq!(source.0.borrow().len(), written, "a refusal wrote a page");

    let rewritten = rewrite(&mut pages, &root, &map).unwrap();
    assert_eq!(rewritten.link, map.links[&root.link]);
    let after = held(&source, rewritten.heap.id, &old);
    assert!(after.is_empty(), "old ids left: {}", after.len());
}
