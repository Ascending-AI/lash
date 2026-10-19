//! A library prepares each function when a run first uses it (FIG-6843):
//! what was prepared before a run changes nothing the run observes, and a
//! body that reaches a function the registry lacks is refused as it was
//! when preparation walked the whole registry.

use std::sync::Arc;

use lash_kernel_doc::{Datum, FunctionRegistry, parse_definition, parse_document};

use super::embedder::{Embedder, Setup, library_with};
use lash_kernel_doc::heap::PageId;

use crate::{End, Machine, PreparedLibrary, Program, StartError, Step};

/// Closures over a body-only helper, a native, a helper whose native
/// stands beside its body, and a loop that calls them.
const PROGRAM: &str = r#"
main {
  let xs = [1, 2, 3]
  let doubled = invoke each.map(fn(x) { return num.add(x, x) }, xs)
  let twice = pair.twice(list.len(doubled))
  let joined = text.concat("a", "b")
  for x in doubled { print (x, twice) }
  return (doubled, twice, joined)
}
"#;

fn observe(prepared: &PreparedLibrary) -> (End, Vec<Datum>, u64) {
    let mut embedder = Embedder::try_over(PROGRAM, Setup::default(), Some(prepared.clone()))
        .unwrap_or_else(|error| panic!("{error}"));
    let end = loop {
        match embedder.run() {
            Step::Ended(end) => break end,
            Step::Slice => (),
            Step::Parked(_) => panic!("the document performs nothing"),
        }
    };
    (
        end,
        embedder.world.printed,
        embedder.machine.meters().charged.expect("exact charge"),
    )
}

/// PLAN-INDEP, FIG-6843: a run over a library that prepared nothing before
/// it, over one that prepared and compiled every function first, and over
/// the first library again once it has run, ends with the same values,
/// output and charge. The first run prepares only what it uses.
#[test]
fn a_run_answers_and_charges_alike_whatever_its_library_prepared_before() {
    let fresh = PreparedLibrary::new(Arc::clone(&library_with(true, None).registry));
    let cold = observe(&fresh);
    assert!(matches!(cold.0, End::Finished(_)), "{:?}", cold.0);
    assert!(
        fresh.prepared() < fresh.registry().len(),
        "a run prepares only the functions it uses"
    );
    let everything = PreparedLibrary::new(Arc::clone(&library_with(true, None).registry));
    everything.segments();
    everything
        .verify_stream()
        .expect("every library body verifies");
    assert_eq!(everything.prepared(), everything.registry().len());
    let warm = observe(&everything);
    let again = observe(&fresh);
    for other in [warm, again] {
        assert_eq!(format!("{:?}", other.0), format!("{:?}", cold.0));
        assert_eq!(other.1, cold.1);
        assert_eq!(other.2, cold.2);
    }
}

/// FIG-6843: a library function whose body lists a function the registry
/// does not hold (one it names but never calls, which registration
/// admits) refuses a document that lists it with the function it lacks,
/// and runs once that function is registered after it.
#[test]
fn a_body_that_reaches_an_absent_function_refuses_its_document_until_it_is_registered() {
    let inner = parse_definition(
        "function probe.inner(x: Any) -> Any\nkernel 1\ncharge 1\nbody { return x }\n",
    )
    .unwrap();
    let inner_id = inner.identity();
    let outer = parse_definition(&format!(
        "function probe.outer(x: Any) -> Any\nkernel 1\ncharge 5\nuse probe.inner = @{inner_id}\n\
         body {{ return x }}\n"
    ))
    .unwrap();
    let mut registry = FunctionRegistry::new();
    let outer_id = registry.register(outer, None).unwrap();
    let document = Arc::new(
        parse_document(&format!(
            "numbers by_spelling\nkernel 1\nuse probe.outer = @{outer_id}\n\
             main {{ let x = invoke probe.outer(7) return x }}\n"
        ))
        .unwrap(),
    );
    let start = |registry: &FunctionRegistry| {
        let setup = Setup::default();
        crate::KernelMachine::start(
            Program {
                release: PageId([8; 32]),
                document: Arc::clone(&document),
                library: PreparedLibrary::new(Arc::new(registry.clone())),
            },
            setup.bounds,
            setup.start,
            Box::new(super::embedder::Source::default()),
        )
    };
    assert!(matches!(
        start(&registry),
        Err(StartError::MissingFunction { function }) if function == inner_id
    ));
    registry.register(inner, None).unwrap();
    assert!(start(&registry).is_ok());
}
