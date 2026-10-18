//! A native call reads the arena as it is: an object the run holds on its
//! pages answers `Retry::Cold`, and the machine decodes it and runs the call
//! again, which changes nothing a run can observe (FIG-6708).

use lash_kernel_doc::Datum;

use super::embedder::{Embedder, Setup};
use crate::{End, Layout, Machine, Step};

/// Long texts, tuples, lists and maps nested in one another, each read by
/// a native, then by the machine.
const PROGRAM: &str = r#"
main {
  let long = text.concat("a text longer than an inline one", ", and longer still")
  let xs = [long, (long, 1), map{"k": long}]
  let m = map{"a": xs, "b": [1, 2, 3]}
  let joined = text.concat(long, long)
  let again = text.concat(joined, xs[0])
  let t = (joined, xs)
  let counted = (list.len(xs), list.len(t), list.len(m["a"]), list.len(m))
  for x in m["b"] { print (x, list.len(xs)) }
  let twice = pair.twice(list.len(m["b"]))
  return (counted, again, twice, m)
}
"#;

fn observe(layout: Layout, cold: bool) -> (End, Vec<Datum>, u64, u64) {
    let mut embedder = Embedder::with(
        PROGRAM,
        Setup {
            layout,
            ..Setup::default()
        },
    );
    if cold {
        embedder.machine.force_cold();
    }
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
        embedder.machine.cold_reads(),
    )
}

/// Every object a native call's arguments reach is put back on its pages
/// before the call, so each chunk it reads answers `Retry::Cold` first: the
/// run ends with the values, output and charge of the run that held them.
#[test]
fn a_native_that_reads_every_chunk_cold_runs_as_it_runs_warm() {
    for layout in [Layout(0), Layout(1)] {
        let (end, printed, charged, warm_reads) = observe(layout, false);
        assert!(matches!(end, End::Finished(_)), "{end:?}");
        assert_eq!(warm_reads, 0);
        let (cold_end, cold_printed, cold_charged, cold_reads) = observe(layout, true);
        assert_eq!(cold_end, end, "layout {layout:?}");
        assert_eq!(cold_printed, printed);
        assert_eq!(cold_charged, charged);
        // Each of the eleven reading calls reads at least one chunk.
        assert!(cold_reads >= 11, "{cold_reads} cold reads");
    }
}
