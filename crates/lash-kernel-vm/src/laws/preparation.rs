//! A library prepares each function when a run first uses it (FIG-6843):
//! what was prepared before a run changes nothing the run observes, and a
//! body that reaches a function the registry lacks is refused as it was
//! when preparation walked the whole registry.

use std::sync::Arc;

use lash_kernel_doc::{Datum, parse_document};

use super::embedder::{Embedder, Setup, library_with};

use crate::{End, Machine, PreparedLibrary, Step};

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

/// FIG-6931: admission validates a document once per identity. An identity
/// a caller validated against the library (a dialect that validates while
/// it lowers) is admitted without being validated again, whichever copy of
/// the document arrives; one no caller validated is validated, and an
/// invalid one is refused.
#[test]
fn an_identity_validated_against_the_library_is_admitted_without_validating_it_again() {
    let prepared = PreparedLibrary::new(Arc::clone(&library_with(true, None).registry));
    // `break` outside a loop: validation refuses it.
    let document =
        Arc::new(parse_document("numbers by_spelling\nkernel 1\nmain { break }\n").unwrap());
    let identity = document.identity();
    let limits = lash_vocabulary::data_limits::KernelDataLimits::standard();
    assert!(
        !prepared.admit_identified(&document, identity, limits),
        "validation refuses it"
    );
    prepared.note_validated(identity, limits);
    let copy = Arc::new((*document).clone());
    assert!(
        prepared.admit_identified(&copy, identity, limits),
        "an identity validated before is not validated again"
    );
}
