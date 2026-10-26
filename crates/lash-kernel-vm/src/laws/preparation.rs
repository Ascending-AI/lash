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
    everything.prepare_every_function();
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
    let limits = lash_kernel_value::data_limits::KernelDataLimits::standard();
    assert!(
        !prepared.admit_identified(
            &document,
            identity,
            limits,
            &lash_kernel_doc::Selection::Fresh
        ),
        "validation refuses it"
    );
    prepared.note_validated(identity, limits);
    let copy = Arc::new((*document).clone());
    assert!(
        prepared.admit_identified(&copy, identity, limits, &lash_kernel_doc::Selection::Fresh),
        "an identity validated before is not validated again"
    );
}

/// ADMIT-ONCE: warmed callable and library queries examine no helper edges,
/// including negative queries. Permission belongs to the defining Link.
#[test]
fn helper_admission_work_is_bounded_by_distinct_origins() {
    use lash_kernel_doc::{
        BuildHelpers, Document, LibraryDependencies, NumberPolicy, Selection, parse_definition,
    };
    use std::collections::BTreeSet;
    use {lash_kernel_value::CallableId, lash_kernel_value::FunctionId};
    let child =
        parse_definition("function law.child() -> Null\nkernel 1\ncharge 1\nbody { return null }")
            .unwrap();
    let child_id = child.identity();
    let parent = parse_definition(&format!("function law.parent() -> Null\nkernel 1\ncharge 1\nuse law.child = @{child_id}\nbody {{ let answer = invoke law.child() return answer }}")).unwrap();
    let parent_id = parent.identity();
    let mut registry = lash_kernel_doc::FunctionRegistry::declarations();
    registry.register(child, None).unwrap();
    registry.register(parent, None).unwrap();
    let helpers = Arc::new(BuildHelpers::new(registry.clone(), [parent_id]).unwrap());
    let library = PreparedLibrary::with_helpers(Arc::new(registry), helpers);
    let mut root = Document::new(NumberPolicy::BySpelling, vec![]);
    root.library = LibraryDependencies([parent_id].into());
    let root = Arc::new(root);
    let program = super::embedder::program(Arc::clone(&root), library.clone());
    let admission = library
        .admit(
            &root,
            lash_kernel_value::data_limits::KernelDataLimits::standard(),
            &Selection::Fresh,
        )
        .unwrap_or_else(|_| panic!("valid origin"));
    let exe = admission
        .executable(
            &root,
            &library,
            &program.documents,
            &BTreeSet::new(),
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("complete library"));
    let callable = CallableId {
        link: program.link,
        function: child_id,
    };
    assert!(exe.admits_callable(&callable));
    assert!(exe.lib_of(&child_id).is_some());
    BuildHelpers::take_admission_edge_visits();
    for _ in 0..64 {
        assert!(exe.admits_callable(&callable));
        assert!(exe.lib_of(&child_id).is_some());
        assert!(!exe.admits_callable(&CallableId {
            function: FunctionId::from_bytes([91; 32]),
            ..callable
        }));
    }
    let again = admission
        .executable(
            &root,
            &library,
            &program.documents,
            &BTreeSet::new(),
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("same validated origin"));
    assert!(again.admits_callable(&callable));
    let edges = BuildHelpers::take_admission_edge_visits();
    println!(
        "ADMIT-ONCE warmed_edges={edges} cache_bytes={}",
        again.admission_bytes()
    );
    assert_eq!(
        edges, 0,
        "ADMIT-ONCE: repeated origin queries rewalked {edges} edges"
    );
    // A second, empty manifest cannot inherit the first origin's permission.
    let empty = Arc::new(Document::new(NumberPolicy::BySpelling, vec![]));
    let other = super::embedder::program(Arc::clone(&empty), library.clone());
    let admitted = library
        .admit(
            &empty,
            lash_kernel_value::data_limits::KernelDataLimits::standard(),
            &Selection::Fresh,
        )
        .unwrap_or_else(|_| panic!("valid origin"));
    let exe = admitted
        .executable(
            &empty,
            &library,
            &other.documents,
            &BTreeSet::new(),
            &other.instances,
        )
        .unwrap_or_else(|_| panic!("complete origin"));
    assert!(!exe.admits_callable(&CallableId {
        link: other.link,
        ..callable
    }));
    assert!(!exe.admits_callable(&callable));
    // The same retained source under Restored is selected by exact live
    // CallableId AND pin AND current membership, independently of Fresh.
    for (live, pins, permitted) in [
        (true, true, true),
        (false, true, false),
        (true, false, false),
    ] {
        let selection = Selection::Restored(lash_kernel_doc::RestoreSelection {
            live: if live {
                [callable].into()
            } else {
                Default::default()
            },
            pins: if pins {
                [child_id].into()
            } else {
                Default::default()
            },
        });
        let admitted = library
            .admit(
                &root,
                lash_kernel_value::data_limits::KernelDataLimits::standard(),
                &selection,
            )
            .unwrap_or_else(|_| panic!("retained empty body"));
        let restored = admitted
            .executable(
                &root,
                &library,
                &program.documents,
                &BTreeSet::new(),
                &program.instances,
            )
            .unwrap_or_else(|_| panic!("restored origin"));
        assert_eq!(restored.admits_callable(&callable), permitted);
        assert!(!restored.admits_callable(&CallableId {
            link: other.link,
            ..callable
        }));
        BuildHelpers::take_admission_edge_visits();
        for _ in 0..64 {
            assert_eq!(restored.admits_callable(&callable), permitted);
        }
        assert_eq!(BuildHelpers::take_admission_edge_visits(), 0);
    }
    let replaced = library.with_helper_handle(BuildHelpers::empty());
    assert!(
        replaced
            .admit(
                &root,
                lash_kernel_value::data_limits::KernelDataLimits::standard(),
                &Selection::Fresh
            )
            .is_err(),
        "helper replacement rebuilds admission and retains its typed refusal"
    );
}

/// FIG-7285/K-SITE-001: a nested closure is a separate unit. Its census
/// must not replace the parent path of a charged sibling call.
#[test]
fn library_closure_census_preserves_sibling_sites_across_nested_units() {
    let mut registry = library_with(true, None).registry.as_ref().clone();
    let mut names = lash_kernel_dialect::NamedLibrary::from_registry(&registry).unwrap();
    let source = r#"
use num.add
function law.factory() -> Any
kernel 1
charge 1
body {
  let outer = fn() {
    if true {
      let answer = invoke num.add(1, 2)
      let inner = fn() { return 0 }
      return answer
    }
    return 0
  }
  return outer
}
"#;
    for definition in lash_kernel_dialect::define_functions(source, &mut names, &[]).unwrap() {
        registry.register(definition, None).unwrap();
    }
    let factory = lash_kernel_dialect::Library::resolve(&names, "law.factory").unwrap();
    let helpers =
        Arc::new(lash_kernel_doc::BuildHelpers::new(registry.clone(), [factory]).unwrap());
    let prepared = PreparedLibrary::with_helpers(Arc::new(registry), helpers);
    let program = format!(
        "library @{factory}\nmain {{ let outer = invoke @{factory}() let answer = apply outer() return answer }}"
    );
    let mut embedder = Embedder::try_over(&program, Setup::default(), Some(prepared)).unwrap();
    assert_eq!(
        super::embedder::result(embedder.run_to_end(&[])),
        super::embedder::int(3)
    );
}
