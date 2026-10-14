//! S5 task-count work over an admitted checkpoint and the real S7 census.
use super::*;
use crate::laws::embedder::{Embedder, ROOMY, Source};
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

struct CountTaskReads {
    source: Source,
    task_pages: BTreeSet<PageId>,
    reads: Rc<Cell<u64>>,
}
impl PageSource for CountTaskReads {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        self.reads.set(
            self.reads.get()
                + pages
                    .iter()
                    .filter(|p| self.task_pages.contains(&p.id))
                    .count() as u64,
        );
        self.source.read(pages)
    }
    fn seal(&mut self, page: PageBody) -> Result<PageRef, HeapFault> {
        self.source.seal(page)
    }
}

/// S5 amendment 332782: an unchanged park uses one task-root summary,
/// without enumerating live tasks or resolving their ancestry.
#[test]
fn a_park_with_one_hundred_thousand_live_tasks_reads_one_task_root() {
    let mut embedder = Embedder::new(
        "fn child() { do perform echo(1) as Any } main { do spawn call child() do perform echo(0) as Any }",
    );
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let access = embedder.machine.heap.access.clone();
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
    let mut task_pages = BTreeSet::new();
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
    walk(
        &embedder.source,
        sealed.root.tasks.page.unwrap(),
        &mut task_pages,
    );
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
        Box::new(CountTaskReads {
            source: embedder.source.clone(),
            task_pages,
            reads: Rc::clone(&reads),
        }),
        Box::new(Census::restore(certificate, LogicalBytes(bounds.memory)).unwrap()),
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
