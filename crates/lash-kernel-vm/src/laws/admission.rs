//! The malformed-state law of the bytecode core (cutover contract §5).
//!
//! - ADMIT-ONE: a run sealed at a real pause, with one rule of the
//!   admission relation broken in its saved record, refuses to open with
//!   that rule's [`Inadmissible`], whatever else the record holds. The
//!   expected refusal is the contract's rule, never what the opener
//!   happens to report: each case names the rule it breaks.
//! - ADMIT-RECEIPT: a wait consumed before the seal is a receipt, not an
//!   outstanding wait: the run opens with its frame past the wait site,
//!   and delivering to it again is refused.
//! - ADMIT-ENTRY: a run started on an export opens with its root frame
//!   standing in the export's body.
//!
//! What the record cannot express is not a case: no saved frame holds a
//! returned or raised completion or a quoted fee (a call completes inside
//! one op), no loop counter stands beside the loop's pass slot, and a core
//! run starts on no callable.

use lash_kernel_doc::Datum;
use lash_kernel_state::kernel_internals::tree::Access;
use lash_kernel_state::paged::SealBudget;
use lash_kernel_state::paged::run::continuation::{
    self, PointPhase, RunBill, RunRecord, RunResume, RunStart, RunTaskState, UnitKind, WaitPhase,
};
use lash_kernel_value::names::Name;

use super::embedder::{ROOMY, Setup, Source, World, answer, core_program, wait_of};
use crate::{CellStart, CoreMachine, DeliverError, Inadmissible, Machine, OpenError, Step};

/// Where a run is sealed.
#[derive(Clone, Copy)]
enum At {
    /// Its first park, before the park's requests are delivered.
    Park,
    /// Its first park, once every request is delivered.
    Delivered,
}

/// A sealed run: its document, pages and root.
struct Sealed {
    text: String,
    source: Source,
    root: lash_kernel_state::paged::CheckpointRoot,
    /// The requests of the park it was sealed at.
    requests: Vec<crate::Request>,
}

impl Sealed {
    fn record(&self) -> RunRecord {
        let access = Access::new(Box::new(self.source.clone()));
        continuation::read(&access, self.root.heap).unwrap()
    }

    /// Opens the run with `record` saved in place of its own.
    fn open(&self, record: &RunRecord) -> Result<CoreMachine, OpenError> {
        let access = Access::new(Box::new(self.source.clone()));
        let mut root = self.root.clone();
        root.heap = continuation::write(&access, record).unwrap();
        <CoreMachine as Machine>::open(
            core_program(&self.text, None),
            ROOMY,
            &lash_kernel_state::kernel_internals::checkpoint(root),
            Box::new(self.source.clone()),
        )
    }
}

fn seal(machine: &mut CoreMachine) -> lash_kernel_state::paged::CheckpointRoot {
    let sealed = machine
        .seal(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        })
        .unwrap()
        .unwrap();
    crate::kernel_internals::sealed_root(&sealed).clone()
}

/// Runs `text` from `start` to its first park and seals it `at` there.
fn sealed(text: &str, start: CellStart, at: At) -> Sealed {
    let source = Source::default();
    let mut machine = <CoreMachine as Machine>::start(
        core_program(text, None),
        ROOMY,
        start,
        Box::new(source.clone()),
    )
    .unwrap();
    let Step::Parked(park) = machine.run(&mut World::default(), u64::MAX).unwrap() else {
        panic!("{text}\nnever parks");
    };
    if let At::Delivered = at {
        for request in &park.requests {
            machine.deliver(wait_of(request), answer(request)).unwrap();
        }
    }
    Sealed {
        text: text.into(),
        root: seal(&mut machine),
        source,
        requests: park.requests,
    }
}

fn main_start() -> CellStart {
    Setup::default().start
}

/// A spawned worker waiting on an effect while `main` loops and joins it.
fn scheduler() -> String {
    super::scheduler::document(false)
}

/// A closure, called in `main`, waiting on an effect.
const CLOSURE: &str = "main {\n  let k = 5\n  let f = fn(x) { let v = perform 0(x) as Any return (v, k) }\n  let r = apply f(1)\n  return r\n}";

/// A library function, called in `main`, waiting on an effect.
const CALLED: &str = "definition \"function g(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let v = perform 0(x) as Any return (x, v) }\"\nmain {\n  let r = call g(3)\n  return r\n}";

/// The task waiting on an effect, and the wait.
fn waiting(record: &RunRecord) -> (usize, u64) {
    record
        .tasks
        .iter()
        .enumerate()
        .find_map(|(at, task)| match task.state {
            RunTaskState::Effect(wait) | RunTaskState::Ready(RunResume::Outcome(wait)) => {
                Some((at, wait))
            }
            _ => None,
        })
        .expect("a task waits on an effect")
}

type Break = Box<dyn Fn(&Sealed, &mut RunRecord) -> Inadmissible>;

/// Each rule, the state it is broken in, how, and the refusal the rule
/// names.
fn cases() -> Vec<(&'static str, String, At, Break)> {
    let s = scheduler;
    vec![
        (
            "no wait performs an operation the run binds as a read (K-EFF-012)",
            s(),
            At::Park,
            Box::new(|sealed, record| {
                let crate::Request::Effect(effect) = &sealed.requests[0];
                record.reads.push(effect.use_);
                Inadmissible::ReadBoundWait {
                    wait: waiting(record).1,
                }
            }),
        ),
        (
            "a delivered outcome is held once",
            s(),
            At::Delivered,
            Box::new(|_, record| {
                let twice = record.waits[0].clone();
                record.waits.push(twice.clone());
                Inadmissible::DuplicateWait { wait: twice.number }
            }),
        ),
        (
            "a wait is one handed out: numbered below the next",
            s(),
            At::Park,
            Box::new(|_, record| {
                record.next_wait = record.waits[0].number;
                Inadmissible::WaitNumber {
                    wait: record.waits[0].number,
                }
            }),
        ),
        (
            "a withdrawn wait is not held",
            s(),
            At::Park,
            Box::new(|_, record| {
                record.withdrawn.push(record.waits[0].number);
                Inadmissible::WaitNumber {
                    wait: record.waits[0].number,
                }
            }),
        ),
        (
            "Issued owns exactly one outstanding wait: a second wait of the task is an orphan",
            s(),
            At::Park,
            Box::new(|_, record| {
                let mut second = record.waits[0].clone();
                second.number = record.next_wait;
                record.next_wait += 1;
                record.waits.push(second.clone());
                Inadmissible::OrphanWait {
                    wait: second.number,
                }
            }),
        ),
        (
            "a consumed receipt is not an outstanding wait",
            s(),
            At::Park,
            Box::new(|_, record| {
                let (task, wait) = waiting(record);
                record.tasks[task].state = RunTaskState::Ready(RunResume::Continue);
                record.ready.push(task as u32);
                Inadmissible::OrphanWait { wait }
            }),
        ),
        (
            "Issued owns its outstanding wait: a waiting task with none",
            s(),
            At::Park,
            Box::new(|_, record| {
                let (task, wait) = waiting(record);
                record.waits.retain(|held| held.number != wait);
                Inadmissible::MissingWait {
                    task: task as u32,
                    wait,
                }
            }),
        ),
        (
            "Delivered owns a ready continuation: a delivered wait whose task still waits",
            s(),
            At::Delivered,
            Box::new(|_, record| {
                let (task, wait) = waiting(record);
                record.tasks[task].state = RunTaskState::Effect(wait);
                record.ready.retain(|queued| *queued as usize != task);
                Inadmissible::MissingWait {
                    task: task as u32,
                    wait,
                }
            }),
        ),
        (
            "ReadyToIssue has no admitted wait",
            s(),
            At::Park,
            Box::new(|_, record| {
                let (task, wait) = waiting(record);
                record.tasks[task].frames.last_mut().unwrap().phase = PointPhase::Ready;
                Inadmissible::WaitPoint { wait }
            }),
        ),
        (
            "a wait decodes by its perform's type",
            s(),
            At::Park,
            Box::new(|_, record| {
                record.waits[0].then_yield = !record.waits[0].then_yield;
                Inadmissible::WaitPoint {
                    wait: record.waits[0].number,
                }
            }),
        ),
        (
            "a wait handed over holds no request, and is not listed as issued",
            s(),
            At::Park,
            Box::new(|_, record| {
                record.issued.push(record.waits[0].number);
                Inadmissible::Issued {
                    wait: record.waits[0].number,
                }
            }),
        ),
        (
            "Delivered holds its raw outcome: a failure holds an error",
            s(),
            At::Delivered,
            Box::new(|_, record| {
                record.waits[0].phase = WaitPhase::Failed(Datum::Null);
                Inadmissible::Outcome {
                    wait: record.waits[0].number,
                }
            }),
        ),
        (
            "the ready queue has no duplicates",
            s(),
            At::Delivered,
            Box::new(|_, record| {
                let first = record.ready[0];
                record.ready.push(first);
                Inadmissible::ReadyDuplicate { task: first }
            }),
        ),
        (
            "Delivered is ready: its task stands in the ready queue",
            s(),
            At::Delivered,
            Box::new(|_, record| {
                let (task, _) = waiting(record);
                record.ready.retain(|queued| *queued as usize != task);
                Inadmissible::Queue { task: task as u32 }
            }),
        ),
        (
            "the ready queue holds only ready tasks",
            s(),
            At::Park,
            Box::new(|_, record| {
                let (task, _) = waiting(record);
                record.ready.push(task as u32);
                Inadmissible::Queue { task: task as u32 }
            }),
        ),
        (
            "a suspended call is non-top and stands at its call",
            s(),
            At::Park,
            Box::new(|_, record| {
                let frames = &mut record.tasks[0].frames;
                assert!(frames.len() > 1, "main stands under a call");
                frames[0].phase = PointPhase::Statement;
                Inadmissible::SuspendedCall { task: 0, frame: 0 }
            }),
        ),
        (
            "a frame's bill stands within its task's billed values",
            CALLED.into(),
            At::Park,
            Box::new(|_, record| {
                let task = &mut record.tasks[0];
                let at = task.frames.len() - 1;
                let past = task.billed.len() as u32 + 1;
                task.frames[at].bill = Some(RunBill { from: past });
                Inadmissible::Bill {
                    task: 0,
                    frame: at as u32,
                }
            }),
        ),
        (
            "a closure body's frame has every capture: a vacant closure below the root",
            CLOSURE.into(),
            At::Park,
            Box::new(|_, record| {
                let frames = &mut record.tasks[0].frames;
                let at = frames
                    .iter()
                    .position(|frame| frame.unit.kind == UnitKind::Closure)
                    .expect("a closure frame");
                assert!(at > 0, "the closure is called, not the root");
                frames[at].closure = None;
                Inadmissible::Captures {
                    task: 0,
                    frame: at as u32,
                }
            }),
        ),
        (
            "a root that is no closure body names no closure",
            CLOSURE.into(),
            At::Park,
            Box::new(|_, record| {
                let at = record.tasks[0].frames.len() - 1;
                let closure = record.tasks[0].frames[at].closure;
                record.tasks[0].frames[0].closure = closure;
                Inadmissible::Captures { task: 0, frame: 0 }
            }),
        ),
        (
            "the first task's root frame is the run's start",
            CALLED.into(),
            At::Park,
            Box::new(|_, record| {
                record.tasks[0].frames.remove(0);
                Inadmissible::Root { task: 0 }
            }),
        ),
        (
            "the task table is in spawn order",
            s(),
            At::Park,
            Box::new(|_, record| {
                record.tasks.swap(0, 1);
                Inadmissible::SpawnOrder { task: 0 }
            }),
        ),
        (
            "occurrence counters cover every minted identity",
            s(),
            At::Park,
            Box::new(|_, record| {
                let lash_kernel_value::sites::TaskIdentity::Spawned(spawned) =
                    &record.tasks[1].identity
                else {
                    panic!("task 1 is spawned");
                };
                let site = spawned.site.clone();
                record.tasks[0]
                    .occurrences
                    .retain(|occurrence| occurrence.site != site);
                Inadmissible::Counter { task: 0 }
            }),
        ),
        (
            "a joiner names a task that joins it",
            s(),
            At::Park,
            Box::new(|_, record| {
                assert!(matches!(record.tasks[1].state, RunTaskState::Effect(_)));
                record.tasks[1]
                    .joiners
                    .push(continuation::RunJoiner::Single(1));
                Inadmissible::Join { task: 1 }
            }),
        ),
        (
            "a list join is joined by the task it names",
            s(),
            At::Park,
            Box::new(|_, record| {
                record.joins.push(continuation::RunJoin {
                    number: record.next_join,
                    joiner: 0,
                    mode: continuation::JoinKind::All,
                    members: vec![1],
                });
                record.next_join += 1;
                Inadmissible::ListJoin {
                    join: record.next_join - 1,
                }
            }),
        ),
        (
            "an ended task holds its result and no frames",
            s(),
            At::Park,
            Box::new(|_, record| {
                let (task, wait) = waiting(record);
                record.waits.retain(|held| held.number != wait);
                record.issued.retain(|held| *held != wait);
                record.tasks[task].state = RunTaskState::Returned(0);
                record.live -= 1;
                Inadmissible::Ended { task: task as u32 }
            }),
        ),
        (
            "only the installed task runs",
            s(),
            At::Park,
            Box::new(|_, record| {
                let (task, wait) = waiting(record);
                record.waits.retain(|held| held.number != wait);
                record.tasks[task].state = RunTaskState::Running;
                Inadmissible::Running { task: task as u32 }
            }),
        ),
        (
            "the live count is the tasks that have not ended",
            s(),
            At::Park,
            Box::new(|_, record| {
                record.live += 1;
                Inadmissible::Live {
                    saved: record.live,
                    counted: record.live - 1,
                }
            }),
        ),
    ]
}

/// ADMIT-ONE: each case's untouched record opens, and with its one rule
/// broken refuses with that rule.
#[test]
fn a_sealed_run_with_one_admission_rule_broken_refuses_with_that_rule() {
    for (rule, text, at, broken) in cases() {
        let sealed = sealed(&text, main_start(), at);
        let mut record = sealed.record();
        if let Err(error) = sealed.open(&record) {
            panic!("{rule}: the untouched record refuses: {error:?}");
        }
        let expected = broken(&sealed, &mut record);
        match sealed.open(&record) {
            Err(OpenError::Inadmissible(refused)) => assert_eq!(refused, expected, "{rule}"),
            Err(other) => panic!("{rule}: refused as {other:?}, not {expected:?}"),
            Ok(_) => panic!("{rule}: opened; expected {expected:?}"),
        }
    }
}

/// ADMIT-RECEIPT: the task consumes its delivered outcome and moves past
/// the wait site before the seal; the reopened run holds no wait for it,
/// a second delivery is refused, and the run ends as one never sealed.
#[test]
fn a_consumed_wait_opens_as_a_receipt_that_refuses_a_second_delivery() {
    let source = Source::default();
    let start = |source: &Source| {
        <CoreMachine as Machine>::start(
            core_program(CALLED, None),
            ROOMY,
            main_start(),
            Box::new(source.clone()),
        )
        .unwrap()
    };
    let ends = |machine: &mut CoreMachine| match machine.run(&mut World::default(), u64::MAX) {
        Ok(Step::Ended(end)) => format!("{end:?}"),
        other => panic!("{other:?}"),
    };
    let mut whole = start(&Source::default());
    let Step::Parked(park) = whole.run(&mut World::default(), u64::MAX).unwrap() else {
        panic!("parks");
    };
    let request = &park.requests[0];
    whole.deliver(wait_of(request), answer(request)).unwrap();
    let expected = ends(&mut whole);

    let mut machine = start(&source);
    let Step::Parked(park) = machine.run(&mut World::default(), u64::MAX).unwrap() else {
        panic!("parks");
    };
    let request = &park.requests[0];
    machine.deliver(wait_of(request), answer(request)).unwrap();
    assert!(matches!(
        machine.run(&mut World::default(), 1).unwrap(),
        Step::Slice
    ));
    let sealed = Sealed {
        text: CALLED.into(),
        root: seal(&mut machine),
        source,
        requests: park.requests.clone(),
    };
    let record = sealed.record();
    assert!(record.waits.is_empty(), "the outcome is consumed");
    assert!(
        record.tasks[0]
            .frames
            .last()
            .is_some_and(|top| top.phase != PointPhase::Issued),
        "the frame is past the wait site"
    );
    let mut opened = sealed.open(&record).unwrap();
    assert!(matches!(
        opened.deliver(wait_of(request), answer(request)),
        Err(DeliverError::AlreadyDelivered { .. })
    ));
    assert_eq!(ends(&mut opened), expected);
}

/// ADMIT-ENTRY: a run started on an export, sealed at its park, opens
/// with the export's body as its root frame.
#[test]
fn a_run_started_on_an_export_opens_at_its_root() {
    let text = "definition \"function go(x: Any) -> Any\\nkernel 1\\nmetering statements\\nbody { let v = perform 0(x) as Any return (x, v) }\"\nmain {}";
    let mut start = main_start();
    start.target = crate::Target::Entry(Name::new("go"));
    start.args = vec![Datum::Text("in".into())];
    let sealed = sealed(text, start, At::Park);
    let record = sealed.record();
    assert_eq!(record.start, RunStart::Entry(Name::new("go")));
    assert_eq!(
        record.tasks[0].frames.len(),
        1,
        "the export's body is the root"
    );
    if let Err(error) = sealed.open(&record) {
        panic!("the entry run refuses: {error:?}");
    }
}
