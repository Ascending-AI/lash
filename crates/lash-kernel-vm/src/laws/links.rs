//! Function values across Links (cutover contract §1).
//!
//! - LINK-CALL: a function value names a Link and a function. It enters a
//!   run only when the run's build loads that Link and the Link declares
//!   the function, and applying it runs that Link's body. A value naming a
//!   Link the build does not load is refused, typed, where it enters,
//!   even when the run's own Link declares the same function: the core
//!   never substitutes another Link's body.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::heap::PageId;
use lash_kernel_doc::{Datum, Document};
use lash_kernel_value::{CallableId, LinkId};

use super::embedder::{ROOMY, Setup, Source, World, document, link, shared_library, wait_of};
use crate::{CoreMachine, End, Machine, Outcome, PreparedLibrary, Program, RunError, Step};

/// The root Link: `f` answers `(x, "root")`, and main, which names `f`
/// as a value itself, asks the host for a function and applies it to 1.
const ROOT: &str = "definition \"function f(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { return (x, \\\"root\\\") }\"\nmain { let own = &f let g = perform 0() as Any let v = apply g(1) return v }";

/// Another Link the build loads: `h` answers `(x, "other")`.
const OTHER: &str = "definition \"function h(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { return (x, \\\"other\\\") }\"\nmain {}";

/// Which function value the host answers, from the root's document and
/// Link and the other's.
type Pick = fn(&Document, LinkId, &Document, LinkId) -> CallableId;

/// How a run of [`ROOT`] whose host answers with `pick`'s function value
/// ends, the build loading [`ROOT`] and [`OTHER`] as two Links.
fn applied(pick: Pick) -> String {
    let library = shared_library(true, None, true);
    let text = |main: &str| format!("numbers by_spelling\n{}{main}", library.header);
    let root = Arc::new(document(&text(ROOT), &library.registry));
    let other = Arc::new(document(&text(OTHER), &library.registry));
    let (root_link, other_link) = (link(&root), link(&other));
    let callable = pick(&root, root_link.identity(), &other, other_link.identity());
    let program = Program::prepare(
        root_link.identity(),
        BTreeMap::from([
            (root.identity(), Arc::clone(&root)),
            (other.identity(), Arc::clone(&other)),
        ]),
        &BTreeMap::from([
            (root_link.identity(), root_link),
            (other_link.identity(), other_link),
        ]),
        PageId([8; 32]),
        PreparedLibrary::new(Arc::clone(&library.registry)),
    )
    .unwrap();
    let mut machine = <CoreMachine as Machine>::start(
        program,
        ROOMY,
        Setup::default().start,
        Box::new(Source::default()),
    )
    .unwrap();
    let mut world = World::default();
    loop {
        match machine.run(&mut world, u64::MAX).unwrap() {
            Step::Ended(End::Finished(finished)) => return format!("{:?}", finished.result),
            Step::Ended(End::Error(RunError::Uncaught {
                error: Datum::Error(error),
                ..
            })) => return format!("raised {}", error.kind),
            Step::Ended(other) => return format!("{other:?}"),
            Step::Slice => {}
            Step::Parked(park) => {
                for request in &park.requests {
                    let answer = Outcome::Completed(Datum::Function(callable));
                    machine.deliver(wait_of(request), answer).unwrap();
                }
            }
        }
    }
}

fn declared(document: &Document, name: &str) -> lash_kernel_value::FunctionId {
    *document
        .functions
        .iter()
        .find(|(_, definition)| definition.name.as_str() == name)
        .unwrap_or_else(|| panic!("no function {name}"))
        .0
}

/// LINK-CALL: a value names its Link's function, run as that Link's; one
/// naming an unloaded Link is refused as it enters, never run as the root's
/// function of the same identity.
#[test]
fn a_function_value_runs_its_own_link_s_body_or_is_refused() {
    let ran = |result: &str| {
        format!(
            "{:?}",
            Datum::Tuple(vec![Datum::Int(1.into()), Datum::Text(result.into())])
        )
    };
    let cases: [(&str, Pick, String); 3] = [
        (
            "the root's own",
            |root, link, _, _| CallableId {
                link,
                function: declared(root, "f"),
            },
            ran("root"),
        ),
        (
            "the other Link's",
            |_, _, other, link| CallableId {
                link,
                function: declared(other, "h"),
            },
            ran("other"),
        ),
        (
            "the root's function under an unloaded Link",
            |root, _, _, _| CallableId {
                link: LinkId::from_bytes([77; 32]),
                function: declared(root, "f"),
            },
            "raised decode_refused".to_owned(),
        ),
    ];
    for (case, pick, expected) in cases {
        assert_eq!(applied(pick), expected, "{case}");
    }
}

/// LINK-IDENTITY: a run starts only on documents that are the bytes their
/// Links name. A prepared program whose root document, or another
/// document it retains, is changed afterwards is refused at its start,
/// typed, whichever of the two changed; the unchanged program starts.
#[test]
fn a_program_whose_root_or_retained_document_changed_is_refused_at_start() {
    use lash_kernel_value::Name;
    let library = shared_library(true, None, true);
    let text = |main: &str| format!("numbers by_spelling\n{}{main}", library.header);
    let root = Arc::new(document(&text(ROOT), &library.registry));
    let other = Arc::new(document(&text(OTHER), &library.registry));
    let (root_link, other_link) = (link(&root), link(&other));
    let other_id = other.identity();
    let program = Program::prepare(
        root_link.identity(),
        BTreeMap::from([
            (root.identity(), Arc::clone(&root)),
            (other_id, Arc::clone(&other)),
        ]),
        &BTreeMap::from([
            (root_link.identity(), root_link),
            (other_link.identity(), other_link),
        ]),
        PageId([8; 32]),
        PreparedLibrary::new(Arc::clone(&library.registry)),
    )
    .unwrap();
    let start = |program: Program| {
        <CoreMachine as Machine>::start(
            program,
            ROOMY,
            Setup::default().start,
            Box::new(Source::default()),
        )
    };
    let changed = |document: &mut Arc<Document>| {
        Arc::make_mut(document)
            .private_bindings
            .insert(Name::new("changed"));
    };
    assert!(
        start(program.clone()).is_ok(),
        "the prepared program starts"
    );
    let mut root_changed = program.clone();
    changed(&mut root_changed.document);
    let mut retained_changed = program;
    changed(retained_changed.documents.get_mut(&other_id).unwrap());
    for (case, program) in [("root", root_changed), ("retained", retained_changed)] {
        let started = start(program);
        assert!(
            matches!(started, Err(crate::StartError::Linked { .. })),
            "{case}: {:?}",
            started.map(|_| ())
        );
    }
}

/// LINK-FILL (FIG-7109): an import runs the export its Link fills it
/// with. Two Links over the same documents that fill one import with two
/// exports of one retained document run two bodies: a loop calling it
/// four times sums 4 under `first` and 8 under `second`. The export a
/// Link does not fill is not executable in its program.
#[test]
fn an_import_runs_the_export_its_link_fills_it_with() {
    use lash_kernel_value::package::{Fill, Link};
    use lash_kernel_value::{ContractId, ExportName, ImportKind, Name, OperationRequirement};
    let library = shared_library(true, None, true);
    let text = |main: &str| format!("numbers by_spelling\n{}{main}", library.header);
    let helpers = Arc::new(document(
        &text(
            "definition \"function first(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { return 1 }\"\n\
             definition \"function second(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { return 2 }\"\nmain {}",
        ),
        &library.registry,
    ));
    let root = Arc::new(document(
        &text(
            "import 90 code law.helper(x: Any) -> Any direct\nmain { let i = 0 let s = 0 while num.lt(i, 4) { let x = invoke import 90(i) set s = num.add(s, x) set i = num.add(i, 1) } return s }",
        ),
        &library.registry,
    ));
    let helper_link = link(&helpers);
    for (name, unfilled, expected) in [("first", "second", 4), ("second", "first", 8)] {
        let root_link = Link {
            helper_fills: Default::default(),
            document: root.identity(),
            fills: root
                .imports
                .iter()
                .map(|(slot, import)| {
                    let fill = match &import.kind {
                        ImportKind::Operation { signature } => Fill::Operation(
                            OperationRequirement::new(signature.clone(), ContractId::new("1")),
                        ),
                        _ => Fill::Export {
                            link: helper_link.identity(),
                            export: ExportName {
                                name: Name::new(name),
                                member: None,
                            },
                        },
                    };
                    (*slot, fill)
                })
                .collect(),
        };
        let program = Program::prepare(
            root_link.identity(),
            BTreeMap::from([
                (root.identity(), Arc::clone(&root)),
                (helpers.identity(), Arc::clone(&helpers)),
            ]),
            &BTreeMap::from([
                (root_link.identity(), root_link),
                (helper_link.identity(), helper_link.clone()),
            ]),
            PageId([8; 32]),
            PreparedLibrary::new(Arc::clone(&library.registry)),
        )
        .unwrap();
        assert!(
            !program.functions.contains(&declared(&helpers, unfilled)),
            "{name}: the unfilled export is executable"
        );
        let mut machine = <CoreMachine as Machine>::start(
            program,
            ROOMY,
            Setup::default().start,
            Box::new(Source::default()),
        )
        .unwrap();
        let end = loop {
            match machine.run(&mut World::default(), u64::MAX).unwrap() {
                Step::Ended(end) => break end,
                Step::Slice => {}
                Step::Parked(park) => panic!("{name}: parked {park:?}"),
            }
        };
        assert_eq!(
            super::embedder::result(end),
            super::embedder::int(expected),
            "{name}"
        );
    }
}

/// A run keeps no document alive past itself: once the program and the
/// machine that ran it are dropped, nothing a start or a run compiled or
/// cached holds the root document.
#[test]
fn a_finished_run_retains_no_document() {
    let library = shared_library(true, None, true);
    // Parsed here, not through the laws' document cache, which keeps
    // what it parsed.
    let root = Arc::new(
        lash_kernel_doc::parse_document(
            "numbers by_spelling\nkernel 1\nmain { let xs = [1, 2] let f = fn() { return xs } let v = apply f() return v }\n",
        )
        .unwrap(),
    );
    let lifetime = Arc::downgrade(&root);
    let root_link = link(&root);
    let program = Program::prepare(
        root_link.identity(),
        BTreeMap::from([(root.identity(), root)]),
        &BTreeMap::from([(root_link.identity(), root_link)]),
        PageId([8; 32]),
        PreparedLibrary::new(Arc::clone(&library.registry)),
    )
    .unwrap();
    let mut machine = <CoreMachine as Machine>::start(
        program,
        ROOMY,
        Setup::default().start,
        Box::new(Source::default()),
    )
    .unwrap();
    assert!(matches!(
        machine.run(&mut World::default(), u64::MAX).unwrap(),
        Step::Ended(End::Finished(_))
    ));
    drop(machine);
    assert!(
        lifetime.upgrade().is_none(),
        "the document outlived its run"
    );
}
