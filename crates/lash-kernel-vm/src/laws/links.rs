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
