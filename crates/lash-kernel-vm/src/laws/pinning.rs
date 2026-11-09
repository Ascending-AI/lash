//! Helper pinning of the bytecode core (cutover contract §7).
//!
//! - HELPER-PIN: a run parked under one build image opens under another
//!   that lacks a build helper exactly when the run can no longer run
//!   that helper: a call behind every frame's resume point, outside any
//!   loop it stands in, does not pin it. A frame in its body, a call ahead
//!   of a resume point or around a loop's back edge, and a function value
//!   the run holds each pin it, and the open refuses naming it. A run that
//!   opens ends as the run that never parked ends.
//! - HELPER-WALK: the open does not trust the root's record of what pins:
//!   with the record emptied, a frame standing in the helper's body and
//!   the opening build's own walk from each resume point still refuse.

use std::sync::Arc;

use lash_kernel_doc::{BuildHelpers, LibraryDependencies};
use lash_kernel_state::kernel_internals::tree::Access;
use lash_kernel_state::paged::SealBudget;
use lash_kernel_state::paged::run::continuation;

use super::embedder::{Library, ROOMY, Setup, Source, World, answer, library, wait_of};
use crate::{CoreMachine, End, Machine, OpenError, PreparedLibrary, Program, Request, Step};

/// The build helper the cases call, which the opening image lacks.
const HELPER: &str = "each.twice";

/// The image a run starts under, holding the helper, and the image it
/// opens under, which does not.
fn images(library: &Library) -> (Arc<BuildHelpers>, Arc<BuildHelpers>) {
    let image = |root: &str| {
        Arc::new(BuildHelpers::new(library.registry.as_ref().clone(), [library.ids[root]]).unwrap())
    };
    (image(HELPER), image("each.map"))
}

/// `text` as a document whose build helpers are `writer`'s members,
/// prepared under `writer` and run under `image`: a host opening a parked
/// run hands the machine its current build image.
fn program(
    text: &str,
    library: &Library,
    writer: &Arc<BuildHelpers>,
    image: &Arc<BuildHelpers>,
) -> Program {
    let text = format!("numbers by_spelling\n{}{text}", library.header);
    let mut document = super::embedder::document(&text, &library.registry);
    document.functions.retain(|id, _| !writer.contains(id));
    document.library = LibraryDependencies(writer.members().copied().collect());
    let mut program = super::embedder::program(
        Arc::new(document),
        PreparedLibrary::with_helpers(Arc::clone(&library.registry), Arc::clone(writer)),
    );
    program.library =
        PreparedLibrary::with_helpers(Arc::clone(&library.registry), Arc::clone(image));
    program
}

/// Delivers every request of each park and runs `machine` to its end.
fn finish(machine: &mut CoreMachine, mut step: Step) -> String {
    let mut world = World::default();
    loop {
        match step {
            Step::Ended(End::Finished(finished)) => return format!("{:?}", finished.result),
            Step::Ended(other) => return format!("{other:?}"),
            Step::Parked(park) => {
                for request in &park.requests {
                    machine.deliver(wait_of(request), answer(request)).unwrap();
                }
            }
            Step::Slice => {}
        }
        step = machine.run(&mut world, u64::MAX).unwrap();
    }
}

/// A run of `text` parked at its first park and sealed under the writer
/// image before the park's requests are delivered: its pages, its root
/// and the requests.
fn parked(
    text: &str,
    library: &Library,
    writer: &Arc<BuildHelpers>,
) -> (
    Source,
    lash_kernel_state::paged::CheckpointRoot,
    Vec<Request>,
) {
    let source = Source::default();
    let mut machine = <CoreMachine as Machine>::start(
        program(text, library, writer, writer),
        ROOMY,
        Setup::default().start,
        Box::new(source.clone()),
    )
    .unwrap();
    let Step::Parked(park) = machine.run(&mut World::default(), u64::MAX).unwrap() else {
        panic!("{text}\nnever parks");
    };
    let sealed = machine
        .seal(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        })
        .unwrap()
        .unwrap();
    (
        source,
        crate::kernel_internals::sealed_root(&sealed).clone(),
        park.requests,
    )
}

fn open(
    text: &str,
    library: &Library,
    (writer, reader): &(Arc<BuildHelpers>, Arc<BuildHelpers>),
    source: &Source,
    root: lash_kernel_state::paged::CheckpointRoot,
) -> Result<CoreMachine, OpenError> {
    <CoreMachine as Machine>::open(
        program(text, library, writer, reader),
        ROOMY,
        &lash_kernel_state::kernel_internals::checkpoint(root),
        Box::new(source.clone()),
    )
}

/// Each run, parked at its first effect, and whether that park still
/// pins the helper. `f` is a closure of the document.
fn cases() -> Vec<(&'static str, bool)> {
    vec![
        // Completed before the park, outside any loop.
        (
            "main {\n  let f = fn(x) { return num.add(x, 1) }\n  let a = invoke each.twice(f, 1)\n  let w = perform 0(a) as Any\n  return (a, w)\n}",
            false,
        ),
        // Called after the park.
        (
            "main {\n  let w = perform 0(1) as Any\n  let f = fn(x) { return num.add(x, 1) }\n  let a = invoke each.twice(f, w)\n  return a\n}",
            true,
        ),
        // Completed in a loop pass the park stands in: the next pass calls it.
        (
            "main {\n  let out = []\n  for i in [1, 2] {\n    let f = fn(x) { return num.add(x, i) }\n    let a = invoke each.twice(f, i)\n    let w = perform 0(a) as Any\n    set out = (out, w)\n  }\n  return out\n}",
            true,
        ),
        // Parked inside it: its frame is live.
        (
            "main {\n  let f = fn(x) { let v = perform 0(x) as Any return v }\n  let a = invoke each.twice(f, 1)\n  return a\n}",
            true,
        ),
        // A function value naming it, held across the park.
        (
            "main {\n  let g = &each.twice\n  let w = perform 0(1) as Any\n  return (w, 0)\n}",
            true,
        ),
    ]
}

/// HELPER-PIN: the text of each case says whether its park pins the
/// helper; the unparked run under the writer image is the oracle for
/// the end of a run that opens.
#[test]
fn a_parked_run_opens_under_an_image_lacking_a_helper_only_when_no_resume_point_reaches_it() {
    let library = library(true);
    let images = images(&library);
    let helper = library.ids[HELPER];
    for (text, pins) in cases() {
        let mut whole = <CoreMachine as Machine>::start(
            program(text, &library, &images.0, &images.0),
            ROOMY,
            Setup::default().start,
            Box::new(Source::default()),
        )
        .unwrap();
        let first = whole.run(&mut World::default(), u64::MAX).unwrap();
        let expected = finish(&mut whole, first);
        let (source, root, requests) = parked(text, &library, &images.0);
        match open(text, &library, &images, &source, root) {
            Err(OpenError::HelperUnavailable { function, .. }) if pins => {
                assert_eq!(function, helper, "{text}");
            }
            Ok(mut machine) if !pins => {
                for request in &requests {
                    machine.deliver(wait_of(request), answer(request)).unwrap();
                }
                let step = machine.run(&mut World::default(), u64::MAX).unwrap();
                assert_eq!(finish(&mut machine, step), expected, "{text}");
            }
            other => panic!(
                "{text}\npins {pins}, open gave {:?}",
                other.map(|_| "a machine")
            ),
        }
    }
}

/// HELPER-WALK: the cases that pin, with the root's pins and held code
/// emptied, still refuse.
#[test]
fn an_open_under_another_image_walks_the_resume_points_itself() {
    let library = library(true);
    let images = images(&library);
    let helper = library.ids[HELPER];
    for (text, pins) in cases() {
        if !pins || text.contains('&') {
            continue;
        }
        let (source, mut root, _) = parked(text, &library, &images.0);
        let access = Access::new(Box::new(source.clone()));
        let mut record = continuation::read(&access, root.heap).unwrap();
        record.pins.clear();
        record.held_code.clear();
        root.heap = continuation::write(&access, &record).unwrap();
        let opened = open(text, &library, &images, &source, root);
        assert!(
            matches!(
                &opened,
                Err(OpenError::HelperUnavailable { function, .. }) if *function == helper
            ),
            "{text}\nopen gave {:?}",
            opened.map(|_| "a machine")
        );
    }
}
