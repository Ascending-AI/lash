//! Memory admission precedes control growth (`K-BND-003`), even while
//! its frame is unpublished. Precision uses the current semantic holdings.

use lash_kernel_doc::Datum;

use super::embedder::{Embedder, ROOMY, Setup, text};
use crate::machine::Boundary;
use crate::{Bound, BoundExceeded, Bounds, End, RunError};

/// What a run ends with and prints, under the 64 MiB bound in slices of
/// `slice`, collecting at every statement boundary when `collect`.
fn verdict(text: &str, slice: u64, collect: bool) -> (End, Vec<Datum>) {
    let mut embedder = Embedder::with(
        text,
        Setup {
            bounds: Bounds {
                memory: 64 << 20,
                ..ROOMY
            },
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

/// K-BND-003: replacing a local with a larger holding pays its growth
/// before a later print, even while its frame is unpublished.
#[test]
fn growing_local_holdings_have_one_verdict_before_the_next_print() {
    let program = r#"
fn local(t) {
  let a = null
  let b = null
  let c = null
  print "before"
  set a = t
  set b = t
  set c = t
  print "escaped"
  return [t, a, b, c]
}
main {
  let s = work.repeat(16777216)
  let out = call local(s)
  return out
}
"#;
    let memory = End::Error(RunError::Bound(BoundExceeded {
        bound: Bound::Memory,
        limit: 64 << 20,
        function: None,
    }));
    for slice in [u64::MAX, 1] {
        for collect in [false, true] {
            let (end, printed) = verdict(program, slice, collect);
            assert_eq!(
                (&end, &printed),
                (&memory, &vec![text("before")]),
                "slice {slice}, collected {collect}"
            );
        }
    }
}
