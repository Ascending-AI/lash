//! The run-format id rewrite (cutover contract §6, `K-VER-005`):
//! succession onto another document or helper library.
//!
//! - A rewrite moves the run's active code: no old id of it is left in
//!   any page the new root reaches, and its frames run the images.
//! - Code the run holds keeps its exact defining Link and code, a held
//!   closure's body's frames and sites too, against Links the
//!   correspondence declares retained; a held callable of another Link
//!   refuses with it, before it writes a page.
//! - A rewrite whose correspondence misses an id active code names
//!   refuses with it, and one whose site correspondence drops a site of
//!   the run's own active code refuses with that site, before it writes
//!   a page: a node that did not survive is never carried onto whatever
//!   node the successor has at its path. A held closure's body's site
//!   carries by identity whatever the site correspondence says.
//! - A site of a document function whose body the site correspondence
//!   lists is carried or refused like main's; one whose body it does not
//!   list keeps its path.

use std::collections::BTreeSet;

use lash_kernel_doc::Datum;
use lash_kernel_doc::heap::PageId;
use lash_kernel_state::paged::run::rewrite::{
    RewriteFault, RunCorrespondence, SiteMap, live_code, rewrite,
};
use lash_kernel_value::LinkId;
use lash_kernel_value::names::{CallableId, FunctionId, Name};
use lash_kernel_value::sites::{Site, Unit};

use super::embedder::{ROOMY, Setup, Source, World, core_program};
use crate::{CellStart, CoreMachine, Machine, Step};

/// A run parked on a `perform` inside a closure it holds, `p`, whose
/// argument, a slot, a list's row and a large list's last chunk each hold
/// a host function of another link, beside another closure of its own.
const TEXT: &str = "main {\n  let f = fn() { return 1 }\n  let g = [h]\n  let big = []\n  let i = 0\n  while num.lt(i, 300) {\n    set big[list.len(big)] = i\n    set i = num.add(i, 1)\n  }\n  set big[list.len(big)] = h\n  let p = fn() { let w = perform 0(h) as Any return w }\n  let v = apply p()\n  return [f, g, big, v]\n}";

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

/// A rewrite to images that share no byte with the ids they replace moves
/// main's frame to the new link and keeps the held closure's frame, every
/// held callable and nothing else of the old ids in the pages its root
/// reaches; a correspondence that does not retain the host function's
/// link refuses with it and writes nothing; one that drops main's site
/// refuses with it, and one that drops only the held closure's body's
/// sites carries them.
#[test]
fn a_rewrite_moves_active_code_keeps_held_code_and_refuses_before_writing() {
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
    let image = LinkId::from_bytes(flipped(root.link.as_bytes()));
    let running = live
        .closures
        .iter()
        .find(|closure| {
            live.frames
                .contains(&(root.link, Unit::Library(closure.function)))
        })
        .copied()
        .expect("the parked run stands in a closure it holds");
    assert_eq!(
        live.frames,
        BTreeSet::from([
            (root.link, Unit::Main),
            (root.link, Unit::Library(running.function))
        ]),
        "main and the held closure it applies run the run's own link"
    );
    let mut map = RunCorrespondence::default();
    let mut old = BTreeSet::new();
    for link in &live.links {
        map.links
            .insert(*link, LinkId::from_bytes(flipped(link.as_bytes())));
        old.insert(*link.as_bytes());
    }
    for function in &live.functions {
        map.functions.insert(
            *function,
            FunctionId::from_bytes(flipped(function.as_bytes())),
        );
        old.insert(*function.as_bytes());
    }
    let mut kept = BTreeSet::new();
    for callable in &live.callables {
        map.retained.insert(callable.link);
        kept.extend([*callable.link.as_bytes(), *callable.function.as_bytes()]);
        old.insert(*callable.function.as_bytes());
    }
    assert!(
        map.retained.contains(&root.link) && map.retained.contains(&foreign.link),
        "the run holds closures of its own link and the host function"
    );
    let in_held = |site: &Site| {
        live.closures
            .iter()
            .any(|closure| site.unit == Unit::Library(closure.function))
    };
    let body: Vec<Site> = live
        .sites
        .iter()
        .filter(|site| in_held(site))
        .cloned()
        .collect();
    assert!(
        !body.is_empty(),
        "the held closure's frame stands in its body"
    );
    let identity = |without: &dyn Fn(&Site) -> bool| {
        Some(SiteMap::new(
            lash_kernel_doc::Correspondence::identity(
                root.document,
                live.sites.iter().filter(|site| !without(site)).cloned(),
            )
            .unwrap(),
        ))
    };
    let before = held(&source, root.heap.id, &old);
    assert!(
        [
            *root.link.as_bytes(),
            *foreign.link.as_bytes(),
            *foreign.function.as_bytes()
        ]
        .iter()
        .all(|digest| before.contains(digest)),
        "the old run holds the ids the rewrite must replace or keep"
    );

    let written = source.0.borrow().len();
    let mut missing = map.clone();
    missing.retained.remove(&foreign.link);
    let refused = rewrite(&mut pages, &root, &missing);
    assert!(
        matches!(&refused, Err(RewriteFault::NotRetained(callable)) if *callable == foreign),
        "{refused:?}"
    );
    assert_eq!(source.0.borrow().len(), written, "a refusal wrote a page");

    let dropped = live
        .sites
        .iter()
        .filter(|site| site.unit == Unit::Main && !in_held(site))
        .max()
        .cloned()
        .expect("the parked run stands in its main");
    let mut moved = map.clone();
    moved.sites = identity(&|site| *site == dropped);
    let refused = rewrite(&mut pages, &root, &moved);
    assert!(
        matches!(
            &refused,
            Err(RewriteFault::Unmapped(
                lash_kernel_state::paged::run::rewrite::Unmapped::Site { site, .. }
            )) if *site == dropped
        ),
        "{refused:?}"
    );
    assert_eq!(source.0.borrow().len(), written, "a refusal wrote a page");

    map.sites = identity(&|site| in_held(site));
    let rewritten = rewrite(&mut pages, &root, &map).unwrap();
    assert_eq!(rewritten.link, image);
    let after = live_code(&mut pages, &rewritten).unwrap();
    assert_eq!(
        after.frames,
        BTreeSet::from([
            (image, Unit::Main),
            (root.link, Unit::Library(running.function))
        ]),
        "main runs the new link, the held closure its own"
    );
    assert_eq!(after.callables, live.callables, "held callables are kept");
    assert!(
        body.iter().all(|site| after.sites.contains(site)),
        "the held closure's body's sites carry by identity"
    );
    assert_eq!(held(&source, rewritten.heap.id, &old), kept, "old ids left");
}

/// A run parked inside a function its document declares.
const DECLARED: &str = "definition \"function inner() -> Any\\nkernel 1\\nmetering statements\\nbody { let w = perform 0(1) as Any return w }\"\nmain {\n  let v = call inner()\n  return v\n}";

/// A site correspondence that lists the declared function's body but drops
/// the site its frame stands at refuses with that site and writes nothing;
/// one that lists none of its body carries its sites at their paths.
#[test]
fn a_listed_declared_function_site_is_carried_or_refused_and_an_unlisted_one_keeps_its_path() {
    let source = Source::default();
    let mut machine = <CoreMachine as Machine>::start(
        core_program(DECLARED, None),
        ROOMY,
        Setup::default().start,
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
    let inner = live
        .frames
        .iter()
        .find_map(|(_, unit)| matches!(unit, Unit::Library(_)).then(|| unit.clone()))
        .expect("the run stands in the declared function");
    let mut map = RunCorrespondence::default();
    map.links
        .extend(live.links.iter().map(|link| (*link, *link)));
    map.functions
        .extend(live.functions.iter().map(|function| (*function, *function)));
    map.retained
        .extend(live.callables.iter().map(|callable| callable.link));
    let identity = |keep: &dyn Fn(&Site) -> bool| {
        Some(SiteMap::new(
            lash_kernel_doc::Correspondence::identity(
                root.document,
                live.sites.iter().filter(|site| keep(site)).cloned(),
            )
            .unwrap(),
        ))
    };
    let body: Vec<Site> = live
        .sites
        .iter()
        .filter(|site| site.unit == inner)
        .cloned()
        .collect();
    let dropped = body.iter().max().cloned().expect("its frame stands in it");

    let written = source.0.borrow().len();
    let mut listed = map.clone();
    listed.sites = identity(&|site| *site != dropped);
    let refused = rewrite(&mut pages, &root, &listed);
    assert!(
        matches!(
            &refused,
            Err(RewriteFault::Unmapped(
                lash_kernel_state::paged::run::rewrite::Unmapped::Site { site, .. }
            )) if *site == dropped
        ),
        "{refused:?}"
    );
    assert_eq!(source.0.borrow().len(), written, "a refusal wrote a page");

    map.sites = identity(&|site| site.unit != inner);
    let rewritten = rewrite(&mut pages, &root, &map).unwrap();
    let after = live_code(&mut pages, &rewritten).unwrap();
    assert!(
        body.iter().all(|site| after.sites.contains(site)),
        "an unlisted body's sites keep their paths"
    );
}
