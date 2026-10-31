//! Memory admission precedes the next effect (`K-BND-003`), even while the
//! growth's frame is unpublished: the capacity meter counts storage and
//! control as they grow, and decides at its next verdict point (FIG-6791).

use lash_kernel_doc::Datum;
use lash_kernel_value::names::FunctionName;

use super::embedder::{Embedder, ROOMY, Setup, document, library, text};
use crate::machine::Boundary;
use crate::{Bound, BoundExceeded, Bounds, End, RunError};

/// What a run ends with and prints under `bounds`, in slices of `slice`,
/// collecting at every statement boundary when `collect`.
fn verdict(text: &str, bounds: Bounds, slice: u64, collect: bool) -> (End, Vec<Datum>) {
    let mut embedder = Embedder::with(
        text,
        Setup {
            bounds,
            slice,
            ..Setup::default()
        },
    );
    if collect {
        embedder.machine.boundary = Some(Box::new(|_| Boundary::Collect));
    }
    let end = embedder.run_to_end(&[]);
    (end, embedder.world.printed)
}

/// K-BND-003: locals that come to hold more than the bound leaves are
/// refused before a later print, even while their frame is unpublished.
/// Each is a text of its own: one value bound three times holds its
/// storage once.
#[test]
fn growing_local_holdings_have_one_verdict_before_the_next_print() {
    let program = r#"
definition "function local(n: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  let a = null\n  let b = null\n  let c = null\n  print \"before\"\n  set a = work.repeat(n)\n  set b = work.repeat(n)\n  set c = work.repeat(n)\n  print \"escaped\"\n  return [a, b, c]\n }"

main {
  let s = work.repeat(16777216)
  let out = call local(16777216)
  return [s, out]
}
"#;
    let memory = End::Error(RunError::Bound(BoundExceeded {
        bound: Bound::Memory,
        limit: 64 << 20,
        function: Some(FunctionName::new("work.repeat").unwrap()),
        site: Some(Box::new(lash_kernel_value::sites::Site::new(
            lash_kernel_value::sites::Unit::Library(
                *document(program, &library(true).registry)
                    .functions
                    .iter()
                    .find(|(_, definition)| definition.name.as_str() == "local")
                    .unwrap()
                    .0,
            ),
            vec![6],
        ))),
    }));
    for slice in [u64::MAX, 1] {
        for collect in [false, true] {
            let (end, printed) = verdict(program, ROOMY, slice, collect);
            assert_eq!(
                (&end, &printed),
                (&memory, &vec![text("before")]),
                "slice {slice}, collected {collect}"
            );
        }
    }
}

/// K-BND-003 (FIG-6791): control state is capacity too. Frames pushed past
/// the bound, each counted as it is pushed, are refused at the statement's
/// end, before the next print.
#[test]
fn frames_grown_past_the_bound_have_a_verdict_before_the_next_print() {
    let program = r#"
definition "function deep(self: Any, n: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  if num.lt(n, 2000) {\n    let r = apply self(self, num.add(n, 1))\n    return r\n  }\n  print \"bottom\"\n  return 0\n }"

main {
  print "before"
  let r = call deep(&deep, 0)
  return r
}
"#;
    let bounds = Bounds {
        memory: 64 << 10,
        call_depth: 10_000,
        ..ROOMY
    };
    let memory = End::Error(RunError::Bound(BoundExceeded {
        bound: Bound::Memory,
        limit: 64 << 10,
        function: Some(FunctionName::new("deep").unwrap()),
        site: Some(Box::new(lash_kernel_value::sites::Site::new(
            lash_kernel_value::sites::Unit::Library(
                *document(program, &library(true).registry)
                    .functions
                    .iter()
                    .find(|(_, definition)| definition.name.as_str() == "deep")
                    .unwrap()
                    .0,
            ),
            vec![0, 1, 0],
        ))),
    }));
    for slice in [u64::MAX, 1] {
        for collect in [false, true] {
            let (end, printed) = verdict(program, bounds, slice, collect);
            assert_eq!(
                (&end, &printed),
                (&memory, &vec![text("before")]),
                "slice {slice}, collected {collect}"
            );
        }
    }
}
