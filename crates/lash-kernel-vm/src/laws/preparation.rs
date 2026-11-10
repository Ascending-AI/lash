//! A library prepares each function when a run first uses it (FIG-6843):
//! what was prepared before a run changes nothing the run observes, and a
//! body that reaches a function the registry lacks is refused as it was
//! when preparation walked the whole registry. The core compiles each
//! library body once for all the documents a library runs, and only for
//! that library's registry.

use std::sync::Arc;

use lash_kernel_doc::Datum;

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

/// Other closures over the same library functions, with other constants
/// and another Link.
const OTHER: &str = r#"
main {
  print "first"
  let joined = text.concat("c", "d")
  let tripled = invoke each.map(fn(y) { return num.add(num.add(y, y), y) }, [7, 8])
  return (pair.twice(list.len(tripled)), joined)
}
"#;

fn observe(prepared: &PreparedLibrary) -> (End, Vec<Datum>, u64) {
    observe_text(PROGRAM, prepared)
}

fn observe_text(text: &str, prepared: &PreparedLibrary) -> (End, Vec<Datum>, u64) {
    let mut embedder = Embedder::try_over(text, Setup::default(), Some(prepared.clone()))
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
/// it, over one that prepared and compiled every function first, over
/// one whose core code another document's run compiled, and over the
/// first library again once it has run, ends with the same values, output
/// and charge. The first run prepares only what it uses.
#[test]
fn a_run_answers_and_charges_alike_whatever_its_library_prepared_before() {
    let fresh = PreparedLibrary::new(Arc::clone(&library_with(true, None).registry));
    let cold = observe(&fresh);
    assert!(matches!(cold.0, End::Finished(_)), "{:?}", cold.0);
    let everything = PreparedLibrary::new(Arc::clone(&library_with(true, None).registry));
    let warm = observe(&everything);
    let shared = PreparedLibrary::new(Arc::clone(&library_with(true, None).registry));
    assert!(matches!(observe_text(OTHER, &shared).0, End::Finished(_)));
    let after_other = observe(&shared);
    let again = observe(&fresh);
    for other in [warm, after_other, again] {
        assert_eq!(format!("{:?}", other.0), format!("{:?}", cold.0));
        assert_eq!(other.1, cold.1);
        assert_eq!(other.2, cold.2);
    }
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

/// The core's code for a library body is its own registry's: a body
/// compiled where a function it calls has a native calls that native, and
/// the same body over the registry of bodies only (`testing::bodies`, the
/// same functions by identity) runs that function's body, before and
/// after the first library ran it, with the same answer (`K-LIB-006`).
#[test]
fn a_library_body_compiled_for_one_registry_never_runs_for_another() {
    let mut registry = library_with(true, None).registry.as_ref().clone();
    let mut names = lash_kernel_dialect::NamedLibrary::from_registry(&registry).unwrap();
    let source = r#"
use pair.twice
function law.via(x: Any) -> Any
kernel 1
charge 1
body {
  let y = invoke pair.twice(x)
  return y
}
"#;
    for definition in lash_kernel_dialect::define_functions(source, &mut names, &[]).unwrap() {
        registry.register(definition, None).unwrap();
    }
    let via = lash_kernel_dialect::Library::resolve(&names, "law.via").unwrap();
    let helpers = Arc::new(lash_kernel_doc::BuildHelpers::new(registry.clone(), [via]).unwrap());
    let natives = PreparedLibrary::with_helpers(Arc::new(registry), helpers);
    let bodies = crate::testing::bodies(&natives);
    let program = format!("library @{via}\nmain {{ let y = invoke @{via}(3) return y }}");
    for prepared in [&natives, &bodies, &natives, &bodies] {
        let mut embedder = Embedder::try_over(&program, Setup::default(), Some(prepared.clone()))
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            super::embedder::result(embedder.run_to_end(&[])),
            super::embedder::int(6)
        );
    }
}

/// Numbers the registries [`unique`] adds to, so no two share a digest.
static FRESH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Adds `law.work`, which reaches library bodies, natives and a closure a
/// library body applies, and a function no other registry of the process
/// has.
fn unique(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    _: &std::collections::BTreeMap<&'static str, lash_kernel_value::FunctionId>,
) -> Vec<(&'static str, lash_kernel_value::FunctionId)> {
    let n = FRESH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let fresh = lash_kernel_doc::parse_definition(&format!(
        "function law.fresh() -> Any\nkernel 1\ncharge 1\nbody {{ return {n} }}\n"
    ))
    .unwrap();
    let fresh = registry.register(fresh, None).unwrap();
    let mut names = lash_kernel_dialect::NamedLibrary::from_registry(registry).unwrap();
    let source = r#"
use each.map
use num.add
use pair.twice
use list.len
use text.concat
function law.work(xs: Any, asked: Any) -> Any
kernel 1
charge 1
body {
  let doubled = invoke each.map(fn(x) { let y = invoke num.add(x, x) return y }, xs)
  let n = invoke list.len(doubled)
  let twice = invoke pair.twice(n)
  let joined = invoke text.concat(asked, "!")
  return (doubled, twice, joined)
}
"#;
    for definition in lash_kernel_dialect::define_functions(source, &mut names, &[]).unwrap() {
        registry.register(definition, None).unwrap();
    }
    let work = lash_kernel_dialect::Library::resolve(&names, "law.work").unwrap();
    vec![("law.fresh", fresh), ("law.work", work)]
}

/// The laws' registry with a function of its own: the process has compiled
/// none of its library.
fn cold_registry() -> Arc<lash_kernel_doc::FunctionRegistry> {
    let registry = library_with(true, Some(unique)).registry;
    let core = PreparedLibrary::new(Arc::clone(&registry)).core(None);
    assert!(core.code().unwrap().units.is_empty(), "the library is cold");
    registry
}

/// `law.work` in `registry`.
fn work(registry: &lash_kernel_doc::FunctionRegistry) -> lash_kernel_value::FunctionId {
    registry
        .iter()
        .find(|(_, registered)| registered.definition().name.as_str() == "law.work")
        .map(|(id, _)| *id)
        .expect("law.work")
}

/// `registry` prepared with `law.work` in its build image.
fn prepared(registry: &Arc<lash_kernel_doc::FunctionRegistry>) -> PreparedLibrary {
    let image = lash_kernel_doc::BuildHelpers::new(registry.as_ref().clone(), [work(registry)]);
    PreparedLibrary::with_helpers(Arc::clone(registry), Arc::new(image.unwrap()))
}

/// A document that performs, prints and calls `law.work` in the library.
fn shared_text(registry: &lash_kernel_doc::FunctionRegistry) -> String {
    let work = work(registry);
    format!(
        "library @{work}\nmain {{ let asked = perform 0(\"ab\") as Any \
         let r = invoke @{work}([1, 2, 3], asked) print r return r }}"
    )
}

/// What a run shows its host: its end, output, requests, charge and the
/// memory it holds at the end.
#[derive(Debug, PartialEq)]
struct Shown {
    end: String,
    printed: Vec<Datum>,
    asked: Vec<String>,
    charged: Option<u64>,
    memory: u64,
}

/// Runs [`shared_text`] over `registry`'s library under `bounds`, sealed at its
/// park and opened cold over `reopen`'s library when given.
fn shown(
    registry: &Arc<lash_kernel_doc::FunctionRegistry>,
    bounds: crate::Bounds,
    mut reopen: Option<&Arc<lash_kernel_doc::FunctionRegistry>>,
) -> Shown {
    let setup = Setup {
        bounds,
        ..Setup::default()
    };
    let text = shared_text(registry);
    let mut embedder = Embedder::try_over(&text, setup, Some(prepared(registry))).unwrap();
    let end = loop {
        match embedder.run() {
            Step::Ended(end) => break end,
            Step::Slice => {}
            Step::Parked(_) => {
                if let Some(registry) = reopen.take() {
                    let root = embedder.checkpoint();
                    let program = super::embedder::program(
                        Arc::clone(&embedder.document),
                        prepared(registry),
                    );
                    let source = Box::new(embedder.source.clone());
                    embedder.machine =
                        crate::KernelMachine::open_in(program, bounds, &root, source).unwrap();
                }
                embedder.deliver_next();
            }
        }
    };
    let meters = embedder.machine.meters();
    Shown {
        end: format!("{end:?}"),
        printed: embedder.world.printed.clone(),
        asked: embedder.asked(),
        charged: meters.charged,
        memory: meters.memory,
    }
}

/// SHARED-LIB (FIG-7688): a start, and a reopen, over a library the
/// process has not compiled show their host what they show over one it
/// has: values, output, effects, charge, held memory, and a refusal.
#[test]
fn a_run_and_its_reopen_show_alike_over_a_cold_and_a_warm_library() {
    let roomy = super::embedder::ROOMY;
    let warm = cold_registry();
    let cold = shown(&warm, roomy, None);
    assert!(cold.end.starts_with("Finished"), "{}", cold.end);
    assert_eq!(cold.asked.len(), 1, "the run parks once");
    let core = PreparedLibrary::new(Arc::clone(&warm)).core(None);
    assert!(
        !core.code().unwrap().units.is_empty(),
        "the run compiled it"
    );
    assert_eq!(shown(&warm, roomy, None), cold, "a warm start");
    assert_eq!(
        shown(&warm, roomy, Some(&cold_registry())),
        cold,
        "a warm start reopened over a cold library"
    );
    assert_eq!(
        shown(&cold_registry(), roomy, Some(&warm)),
        cold,
        "a cold start reopened over a warm library"
    );
    let tight = crate::Bounds {
        charge_limit: cold.charged.expect("exact charge") / 2,
        ..roomy
    };
    let refused = shown(&cold_registry(), tight, None);
    assert!(refused.end.contains("Error"), "{}", refused.end);
    assert_eq!(shown(&warm, tight, None), refused, "a warm refusal");
}

/// SHARED-LIB (FIG-7688): a library compiled for one registry is never
/// handed to a registry that differs from it, or to another compiler
/// revision of the same registry.
#[test]
fn a_registry_or_revision_change_never_reuses_a_compiled_library() {
    use crate::bytecode::library::{COMPILER_REVISION, digest, held, key, key_at};
    let registry = cold_registry();
    assert!(
        shown(&registry, super::embedder::ROOMY, None)
            .end
            .starts_with("Finished")
    );
    let digest = digest(&registry);
    let compiled = held(key(&digest, None));
    assert!(
        !compiled.code().unwrap().units.is_empty(),
        "the run compiled it"
    );
    let changed = cold_registry();
    for (case, other) in [
        ("another registry", PreparedLibrary::new(changed).core(None)),
        (
            "another revision",
            held(key_at(&digest, None, COMPILER_REVISION + 1)),
        ),
    ] {
        assert!(!Arc::ptr_eq(&compiled, &other), "{case}");
        assert!(other.code().unwrap().units.is_empty(), "{case}");
    }
}

/// SHARED-LIB (FIG-7688): a library body that performs is compiled once
/// and fills each perform in the Link of the instance that runs it, cold
/// or warm.
#[test]
fn two_links_over_one_library_fill_their_own_performs() {
    use lash_kernel_value::package::{Fill, HelperFill};
    use lash_kernel_value::{ContractId, ImportKind, OperationRequirement};
    let mut registry = library_with(true, Some(unique)).registry.as_ref().clone();
    let ask = lash_kernel_doc::parse_definition(
        "function law.ask(x: Any) -> Any\nkernel 1\ncharge 1\n\
         import 0 operation host.ask(x: Any) -> Any\n\
         body { let y = perform 0(x) as Any return y }\n",
    )
    .unwrap();
    let ask = registry.register(ask, None).unwrap();
    let helpers = Arc::new(lash_kernel_doc::BuildHelpers::new(registry.clone(), [ask]).unwrap());
    let prepared = PreparedLibrary::with_helpers(Arc::new(registry), Arc::clone(&helpers));
    let header = &super::embedder::shared_library(true, None, false).header;
    for tag in ["first", "second", "first"] {
        let text = format!(
            "numbers by_spelling\n{header}library @{ask}\nmain {{ let y = invoke @{ask}(\"{tag}\") return y }}"
        );
        let parsing = super::embedder::shared_library(true, None, false).registry;
        let document = Arc::new(super::embedder::document(&text, &parsing));
        let mut link = super::embedder::link(&document);
        link.helper_fills = lash_kernel_doc::helper_imports(&document, helpers.as_ref())
            .unwrap()
            .into_iter()
            .map(|(id, import)| {
                let ImportKind::Operation { signature } = &import.kind else {
                    panic!("an operation")
                };
                let requirement =
                    OperationRequirement::new(signature.clone(), ContractId::new("1"));
                let fill = Fill::Operation(requirement);
                (id, HelperFill { import, fill })
            })
            .collect();
        let identity = link.identity();
        let program = crate::Program::prepare(
            identity,
            std::collections::BTreeMap::from([(document.identity(), Arc::clone(&document))]),
            &std::collections::BTreeMap::from([(identity, link)]),
            lash_kernel_doc::heap::PageId([8; 32]),
            prepared.clone(),
        )
        .unwrap();
        let mut machine = <crate::CoreMachine as crate::Machine>::start(
            program,
            super::embedder::ROOMY,
            Setup::default().start,
            Box::new(super::embedder::Source::default()),
        )
        .unwrap();
        let mut world = super::embedder::World::default();
        let mut performed = Vec::new();
        let end = loop {
            match machine.run(&mut world, u64::MAX).unwrap() {
                Step::Ended(end) => break end,
                Step::Slice => {}
                Step::Parked(park) => {
                    for request in &park.requests {
                        let crate::Request::Effect(effect) = request;
                        performed.push(effect.use_.link);
                        let answer = crate::Outcome::Completed(effect.args[0].clone());
                        machine.deliver(effect.wait, answer).unwrap();
                    }
                }
            }
        };
        assert_eq!(performed, [identity], "{tag} fills its own perform");
        assert_eq!(super::embedder::result(end), super::embedder::text(tag));
    }
}
