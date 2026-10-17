//! S5 task-count work over an admitted checkpoint and the real S7 census,
//! and the wait pages a park reads (FIG-6425).
use super::*;
use crate::laws::embedder::{Embedder, ROOMY, Setup, Source};
use lash_kernel_doc::heap::{HeapFault, PageId, PageRef};
use lash_kernel_doc::{Site, Unit};
use lash_kernel_state::paged::census_impl::Census;
use lash_kernel_state::paged::directory::ordinal_key;
use lash_kernel_state::paged::{
    AllocationCensus, PageAnswer, PageBody, PageKind, PageSource, SiteRef, TaskIdentityRef,
};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::rc::Rc;

/// Counts the reads of `counted`, the pages of one tree.
struct CountReads {
    source: Source,
    counted: BTreeSet<PageId>,
    reads: Rc<Cell<u64>>,
}
impl PageSource for CountReads {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        self.reads.set(
            self.reads.get()
                + pages
                    .iter()
                    .filter(|p| self.counted.contains(&p.id))
                    .count() as u64,
        );
        self.source.read(pages)
    }
    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        self.source.seal(page)
    }
}

/// The index and leaf pages of the keyed tree rooted at `page`, never what
/// its rows name.
fn tree_pages(source: &Source, page: Option<PageRef>) -> BTreeSet<PageId> {
    fn walk(source: &Source, page: PageRef, pages: &mut BTreeSet<PageId>) {
        if !pages.insert(page.id) {
            return;
        }
        let answer = source.0.borrow().get(&page.id).unwrap().clone();
        if answer.kind == PageKind::KEYED_INDEX {
            for child in answer.children {
                walk(source, child, pages);
            }
        } else {
            assert_eq!(answer.kind, PageKind::KEYED_LEAF);
        }
    }
    let mut pages = BTreeSet::new();
    if let Some(page) = page {
        walk(source, page, &mut pages);
    }
    pages
}

/// S5 amendment 332782: an unchanged park uses one task-root summary,
/// without enumerating live tasks or resolving their ancestry.
#[test]
fn a_park_with_one_hundred_thousand_live_tasks_reads_one_task_root() {
    let mut embedder = Embedder::new(
        "fn child() { do perform echo(1) as Any } main { do spawn call child() do perform echo(0) as Any }",
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let access = embedder.machine.heap.pages().clone();
    let template = embedder
        .machine
        .tasks
        .get_mut(1)
        .unwrap()
        .unwrap()
        .record
        .clone();
    let mut main = embedder
        .machine
        .tasks
        .get_mut(0)
        .unwrap()
        .unwrap()
        .record
        .clone();
    let spawn_site = Site::new(Unit::Main, [0, 0]);
    let saved_site = SiteRef::new(&spawn_site, &access).unwrap();
    main.occurrences
        .write(
            lash_kernel_state::paged::metadata::metadata_key(&spawn_site),
            100_000,
            &access,
        )
        .unwrap();
    let mut tasks = Tree::new(false);
    tasks.write(ordinal_key(0), main, &access).unwrap();
    let mut waits = Tree::new(false);
    waits
        .write(
            ordinal_key(0),
            WaitRecord {
                task: TaskId::MAIN,
                kind: lash_kernel_state::paged::WaitKindRecord::Effect,
                phase: WaitPhase::Admitted,
            },
            &access,
        )
        .unwrap();
    for id in 1..=100_000 {
        let mut task = template.clone();
        task.identity =
            TaskIdentityRef::spawn(TaskIdentityRef::MAIN, saved_site.clone(), id - 1, &access)
                .unwrap();
        task.state = TaskState::Waiting(id);
        tasks.write(ordinal_key(id), task, &access).unwrap();
        waits
            .write(
                ordinal_key(id),
                WaitRecord {
                    task: TaskId(id),
                    kind: lash_kernel_state::paged::WaitKindRecord::Effect,
                    phase: WaitPhase::Admitted,
                },
                &access,
            )
            .unwrap();
    }
    // The fixture is a table of already admitted waits using the real
    // pending KIR frame, not a tracing or allocation-policy replacement.
    embedder.machine.tasks = tasks::Tasks::open(tasks, access, Arc::clone(&embedder.machine.exe));
    embedder.machine.waits = waits;
    embedder.machine.live_tasks = 100_001;
    embedder.machine.next_wait = 100_001;
    let sealed = embedder
        .machine
        .seal(lash_kernel_state::paged::SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        })
        .unwrap()
        .unwrap();
    let certificate = embedder.machine.accounting();
    let task_pages = tree_pages(&embedder.source, sealed.root.tasks.page());
    let reads = Rc::new(Cell::new(0));
    let bounds = Bounds {
        live_tasks: 100_001,
        ..ROOMY
    };
    let (program, _) = embedder.program(true);
    let mut machine = KernelMachine::open(
        program,
        bounds,
        &sealed.root,
        Vec::new(),
        Box::new(CountReads {
            source: embedder.source.clone(),
            counted: task_pages,
            reads: Rc::clone(&reads),
        }),
        Box::new(
            Census::restore(
                certificate,
                lash_kernel_doc::heap::LogicalBytes(bounds.memory),
            )
            .unwrap(),
        ),
    )
    .unwrap();
    reads.set(0);
    let Step::Parked(park) = Machine::run(&mut machine, &mut embedder.world, u64::MAX).unwrap()
    else {
        panic!("all tasks wait");
    };
    assert_eq!((park.live, park.outstanding), (100_000, 100_000));
    assert!(park.requests.is_empty());
    assert_eq!(
        reads.get(),
        1,
        "only the root aggregate, never member paths"
    );
}

/// FIG-6425: a park reports only the waits issued since the last one. A
/// 1,000-way fan-out delivered one at a time, each delivery on a machine
/// opened cold from the last checkpoint, reads at most a constant number of
/// wait pages per delivery: O(W) in all, never the whole table per park.
/// The children are joined one by one, so each delivery wakes one joiner.
#[test]
fn a_thousand_way_fan_out_delivered_one_at_a_time_reads_linear_wait_pages() {
    const W: u64 = 1_000;
    let bounds = Bounds {
        live_tasks: 1_001,
        requests_per_park: 1_000,
        ..ROOMY
    };
    let mut embedder = Embedder::with(
        &format!(
            "fn child(i) {{ let r = perform echo(i) as Any return r }}
main {{
  let hs = []
  let i = 0
  while num.lt(i, {W}) {{
    let h = spawn call child(i)
    set hs[list.len(hs)] = h
    set i = num.add(i, 1)
  }}
  let n = 0
  for h in hs {{
    let r = join h
    set n = num.add(n, 1)
  }}
  return n
}}"
        ),
        Setup {
            bounds,
            ..Setup::default()
        },
    );
    let step = embedder.run();
    let Step::Parked(park) = step else {
        panic!("every child waits: {step:?}");
    };
    assert_eq!(park.requests.len() as u64, W);
    let (mut reads, mut delivered) = (0, 0);
    let end = loop {
        let (root, certificate) = embedder.checkpoint();
        let counted = tree_pages(&embedder.source, root.waits.page());
        let count = Rc::new(Cell::new(0));
        let (program, _) = embedder.program(true);
        embedder.machine = KernelMachine::open(
            program,
            bounds,
            &root,
            Vec::new(),
            Box::new(CountReads {
                source: embedder.source.clone(),
                counted,
                reads: Rc::clone(&count),
            }),
            Box::new(
                Census::restore(
                    certificate,
                    lash_kernel_doc::heap::LogicalBytes(bounds.memory),
                )
                .unwrap(),
            ),
        )
        .unwrap();
        embedder.deliver_next();
        delivered += 1;
        let step = embedder.run();
        reads += count.get();
        if let Step::Ended(end) = step {
            break end;
        }
        assert!(matches!(step, Step::Parked(_)));
    };
    assert_eq!(
        crate::laws::embedder::result(end),
        crate::laws::embedder::int(W as i64)
    );
    assert_eq!(delivered, W);
    assert!(
        reads <= 8 * W,
        "{reads} wait pages read over {W} deliveries"
    );
}
