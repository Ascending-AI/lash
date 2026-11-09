//! Cold storage preserves native results, output and exact charge (FIG-6708).
//! The source oracle retries cold chunks; regions reopen canonical checkpoints.

use lash_kernel_doc::{
    Datum,
    heap::{HeapFault, PageRef},
};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use std::{cell::Cell, rc::Rc};

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

struct ColdPages {
    source: super::embedder::Source,
    reads: Rc<Cell<usize>>,
}

impl PageSource for ColdPages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        self.reads.set(self.reads.get() + pages.len());
        self.source.read(pages)
    }

    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        self.source.seal(body)
    }
}

#[derive(Debug, PartialEq)]
struct EndState {
    end: End,
    session: Option<lash_kernel_state::semantic::Checkpoint>,
}

fn observe(
    layout: Layout,
    cold: bool,
    source: bool,
) -> (EndState, Vec<Datum>, u64, u64, usize, usize) {
    let mut embedder = Embedder::with(
        PROGRAM,
        Setup {
            layout,
            slice: if cold && !source { 1 } else { u64::MAX },
            ..Setup::default()
        },
    );
    if cold && source {
        embedder.machine.force_cold();
    }
    let reads = Rc::new(Cell::new(0));
    let mut reopens = 0;
    let mut end = loop {
        if source {
            embedder.machine.force_source_execution();
        }
        match embedder.run() {
            Step::Ended(end) => break end,
            Step::Slice if cold && !source => {
                let root = embedder.checkpoint();
                let (program, library) = embedder.program(true);
                embedder.machine = crate::KernelMachine::open_in(
                    program,
                    embedder.bounds,
                    &root,
                    Box::new(ColdPages {
                        source: embedder.source.clone(),
                        reads: Rc::clone(&reads),
                    }),
                    layout,
                )
                .unwrap();
                embedder.library = library;
                reopens += 1;
            }
            Step::Slice => (),
            Step::Parked(_) => panic!("the document performs nothing"),
        }
    };
    // Reopen may change allocator history and tree packing. Compare the
    // complete carry graph through the kernel's PLAN-INDEP equality domain.
    let session = if !source && let End::Finished(finished) = &mut end {
        finished.carry.take().map(|carry| {
            lash_kernel_state::semantic::session(&carry, Box::new(embedder.source.clone())).unwrap()
        })
    } else {
        None
    };
    (
        EndState { end, session },
        embedder.world.printed,
        embedder.machine.meters().charged.expect("exact charge"),
        embedder.machine.cold_reads(),
        reads.get(),
        reopens,
    )
}

/// Cold native arguments preserve values, output and exact charge. The source
/// oracle forces each chunk to retry cold. Regions reopen their canonical
/// checkpoints in a fresh machine at every slice and read actual backing pages.
#[test]
fn a_native_that_reads_every_chunk_cold_runs_as_it_runs_warm() {
    for layout in [Layout(0), Layout(1)] {
        for source in [true, false] {
            let (end, printed, charged, warm_reads, _, _) = observe(layout, false, source);
            assert!(matches!(&end.end, End::Finished(_)), "{end:?}");
            let (cold_end, cold_printed, cold_charged, cold_reads, pages, reopens) =
                observe(layout, true, source);
            assert_eq!(cold_end, end, "layout {layout:?}, source={source}");
            assert_eq!(cold_printed, printed);
            assert_eq!(cold_charged, charged);
            if source {
                assert_eq!(warm_reads, 0);
                // Each of the eleven reading calls reads at least one chunk.
                assert!(cold_reads >= 11, "{cold_reads} cold reads");
            } else {
                assert!(reopens > 0, "regions must actually reopen cold");
                assert!(pages > 0, "reopen must read actual backing pages");
            }
        }
    }
}

mod mixed_cycle {
    use lash_kernel_doc::Value;
    use lash_kernel_doc::arena::native::NewObject;
    use lash_kernel_state::paged::run::directory::Directory;
    use lash_kernel_state::paged::run::ids::HighWater;

    use crate::heap::Heap;
    use crate::heap::slots::SlotHeap;
    use crate::laws::embedder::Source;

    fn make(heap: &mut SlotHeap, object: NewObject) -> Value {
        heap.admit(crate::heap::slots::schedule(&object))
            .expect("room");
        heap.create(object).expect("an object")
    }

    /// Contract §6 (B4), dropped last root of a mixed cycle: `a` and `b`
    /// name each other; `b` is paged out, `a` is resident and changed
    /// since the seal, and holds a large list. Once the last root is
    /// dropped, before another seal, a collection frees `a` and its list,
    /// so an allocation that fits only then is admitted. The cold object
    /// `b` names `a`, but nothing reaches `b`: its edges are a candidate
    /// set, not a root set.
    #[test]
    fn dropped_last_root_of_a_mixed_cycle_frees_its_resident_half() {
        let pages = Source::default();
        let mut heap = SlotHeap::durable(
            u64::MAX,
            crate::heap::open::Durable {
                directory: Directory::new(Box::new(pages)),
                high_water: HighWater(0),
            },
        );
        let a = make(&mut heap, NewObject::List(vec![Value::Int(1)]));
        let b = make(&mut heap, NewObject::List(vec![a]));
        let (a_handle, b_handle) = (a.handle().expect("a"), b.handle().expect("b"));
        heap.arena_mut().list_set(a_handle, 0, b).expect("a ↔ b");
        heap.page_out(&[a]).expect("a page-out");
        heap.load(a_handle, 0, &mut |visit| visit(&a))
            .expect("a reload");
        assert!(
            heap.arena().facts(b_handle).expect("b").cold,
            "b is paged out"
        );
        assert!(
            !heap.arena().facts(a_handle).expect("a").cold,
            "a is resident"
        );

        // `a` changes after the seal: it now holds a list of 4,096 ints.
        let list = make(
            &mut heap,
            NewObject::List((0..4096).map(Value::Int).collect()),
        );
        heap.arena_mut()
            .list_set(a_handle, 1, list)
            .expect("an append");
        heap.settle(a_handle, 0);

        // The last root is dropped. The bound leaves room for half the
        // list: what is wanted fits only once `a` and its list are freed.
        let wanted = crate::heap::slots::schedule(&NewObject::List(vec![Value::Int(0); 2048]));
        heap.rebound(heap.held() + wanted / 2);
        assert!(
            heap.admit(wanted).is_err(),
            "it does not fit before a collection"
        );
        heap.collect(&mut |_| {});
        heap.admit(wanted)
            .expect("the collection freed the cycle's resident half");
        assert!(heap.arena().facts(a_handle).is_err(), "a is freed");
    }
}
