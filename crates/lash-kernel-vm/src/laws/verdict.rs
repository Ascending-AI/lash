//! The memory bound gives one verdict at one statement whatever the host
//! slicing and whenever garbage is collected, the verdict main gives
//! (K-BND-003, FIG-6318): a machine reservation holds its bytes, and only
//! a refinement over published roots refuses.

/// The programs the memory-verdict laws run under a 64 MiB bound: a 1 MiB
/// text held at each level of a recursion, appended to a list, written
/// into a map, and put 80 times into one printed list literal. Each prints
/// where it is, so the statement the bound ends it at shows.
pub(crate) const MIB_TEXT: &str = "let s = work.repeat(1048576)";

pub(crate) fn scenarios(garbage: &str) -> Vec<(&'static str, String)> {
    let items = vec!["s"; 80].join(", ");
    vec![
        (
            "recursion",
            format!(
                "fn f(n, t) {{\n  print n\n  if num.lt(n, 100) {{ let m = num.add(n, 1) let r = call f(m, t) return r }}\n  return n\n}}\n\
                 main {{\n  {garbage}\n  {MIB_TEXT}\n  let r = call f(0, s)\n  return r\n}}"
            ),
        ),
        (
            "list write",
            format!(
                "main {{\n  {garbage}\n  {MIB_TEXT}\n  let xs = []\n  let i = 0\n  while num.lt(i, 100) {{\n    print i\n    set xs[i] = s\n    set i = num.add(i, 1)\n  }}\n  return list.len(xs)\n}}"
            ),
        ),
        (
            "map write",
            format!(
                "main {{\n  {garbage}\n  {MIB_TEXT}\n  let m = map{{}}\n  let i = 0\n  while num.lt(i, 100) {{\n    print i\n    set m[i] = s\n    set i = num.add(i, 1)\n  }}\n  return list.len(m)\n}}"
            ),
        ),
        (
            "print",
            format!(
                "main {{\n  {garbage}\n  {MIB_TEXT}\n  print \"before\"\n  print [{items}]\n  print \"after\"\n  return 1\n}}"
            ),
        ),
    ]
}

/// Garbage of 12 MiB the run made and dropped before the scenario.
pub(crate) const GARBAGE: &str = "let g = work.repeat(12582912) set g = null";

use lash_kernel_doc::Datum;

use super::embedder::{Embedder, ROOMY, Setup, int, text};
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

/// Every scenario ends with the bound's verdict at one statement, the
/// same in whole and short slices and whether garbage waits for pressure
/// or is collected at every boundary. The recursion and the print get
/// main's verdict at main's statement (4c263df58f, the same programs): the
/// 62nd frame holding the text, and nothing of the over-bound literal
/// printed. A list or map write is refused at the write of index 62, where
/// main refuses at 61 and the heap before FIG-6318 at 60: the heap prices
/// a write's operand where it is held, not as main's per-write counter.
#[test]
fn the_memory_verdict_is_one_whatever_the_slicing_and_the_garbage() {
    let memory = End::Error(RunError::Bound(BoundExceeded {
        bound: Bound::Memory,
        limit: 64 << 20,
        function: None,
    }));
    for garbage in ["", GARBAGE] {
        for (name, program) in scenarios(garbage) {
            let printed = match name {
                "print" => vec![text("before")],
                "recursion" => (0..62).map(int).collect(),
                _ => (0..63).map(int).collect::<Vec<_>>(),
            };
            for slice in [u64::MAX, 7] {
                for collect in [false, true] {
                    let (end, seen) = verdict(&program, slice, collect);
                    assert_eq!(
                        (&end, &seen),
                        (&memory, &printed),
                        "{name}, garbage {}, slice {slice}, collected {collect}",
                        !garbage.is_empty()
                    );
                }
            }
        }
    }
}
