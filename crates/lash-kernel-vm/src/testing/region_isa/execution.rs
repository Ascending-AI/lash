//! The adapters execute fresh machines. Their only shared observation code reads
//! each machine's own published state; it computes no expected program values,
//! prices, continuation phases or debt from the other executor's output.
use super::*;
use crate::{ChargeTape, End, Host, KernelMachine, Machine, Outcome, Request, Step};
use lash_kernel_doc::Datum;
use lash_kernel_doc::heap::{HeapFault, PageRef};
use lash_kernel_state::paged::{PageAnswer, PageBody, SealBudget};
use std::{cell::RefCell, collections::VecDeque, rc::Rc};

#[derive(Clone)]
struct Pages(Rc<RefCell<Box<dyn PageSource>>>);
impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        self.0.borrow_mut().read(pages)
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        self.0.borrow_mut().seal(body)
    }
}

#[derive(Default)]
struct World {
    printed: Vec<Vec<u8>>,
}
impl Host for World {
    fn print(&mut self, value: &Datum) {
        self.printed
            .push(lash_kernel_value::tokens::encode_body(value));
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

fn failure(prefix: usize, error: impl std::fmt::Display) -> QualificationFailure {
    let mut failure = QualificationFailure::unavailable(&error.to_string());
    failure.prefix = prefix;
    failure
}

/// Encodes copied payloads and opaque identities in full. Debug text is only
/// diagnostic: several identity Debug implementations deliberately abbreviate.
fn event(step: Option<&Step>) -> Vec<Vec<u8>> {
    use crate::{Bound, RunError};
    use lash_kernel_value::tokens::encode_body;
    match step {
        None => vec![encode_body(&0_u32)],
        Some(Step::Slice) => vec![encode_body(&1_u32)],
        Some(Step::Parked(park)) => {
            let mut parts = vec![
                encode_body(&2_u32),
                encode_body(&park.outstanding),
                encode_body(&park.live),
                encode_body(&(park.requests.len() as u64)),
            ];
            for request in &park.requests {
                let Request::Effect(effect) = request;
                parts.extend([
                    encode_body(&lash_kernel_doc::kernel_internals::wait_number(effect.wait)),
                    encode_body(&effect.identity),
                    encode_body(&effect.use_),
                    encode_body(&effect.args),
                    encode_body(&effect.result),
                    encode_body(&effect.origin),
                ]);
            }
            parts.push(encode_body(&park.cancel_requests));
            parts
        }
        Some(Step::Ended(end)) => match end {
            End::Finished(finished) => vec![
                encode_body(&3_u32),
                encode_body(&finished.result),
                encode_body(&finished.finish),
                encode_body(&finished.not_carried),
            ],
            End::Failed(value) => vec![encode_body(&4_u32), encode_body(value)],
            End::Cancelled => vec![encode_body(&5_u32)],
            End::Error(error) => match error {
                RunError::Uncaught { error, site } => {
                    vec![encode_body(&6_u32), encode_body(error), encode_body(site)]
                }
                RunError::TasksOutstanding {
                    unfinished,
                    unobserved,
                    sources,
                } => vec![
                    encode_body(&7_u32),
                    encode_body(unfinished),
                    encode_body(unobserved),
                    encode_body(sources),
                ],
                RunError::Deadlock { waiting } => vec![encode_body(&8_u32), encode_body(waiting)],
                RunError::Bound(exceeded) => {
                    let (tag, function) = match &exceeded.bound {
                        Bound::Charge => (0_u32, None),
                        Bound::Memory => (1, None),
                        Bound::CallDepth => (2, None),
                        Bound::LiveTasks => (3, None),
                        Bound::RequestsPerPark => (4, None),
                        Bound::JoinMembers => (5, None),
                        Bound::Guard { function } => (6, Some(*function)),
                        Bound::Effects => (7, None),
                        Bound::CheckpointBytes => (8, None),
                    };
                    vec![
                        encode_body(&9_u32),
                        encode_body(&tag),
                        encode_body(&function),
                        encode_body(&exceeded.limit),
                        encode_body(&exceeded.function),
                        encode_body(&exceeded.site.as_deref().cloned()),
                    ]
                }
            },
        },
    }
}

fn observe(
    machine: &mut KernelMachine,
    tape: &ChargeTape,
    world: &World,
    step: Option<&Step>,
    pages: &Pages,
    interruption: Option<Interruption>,
) -> Result<PrefixSnapshot, QualificationFailure> {
    let mut session = None;
    let mut step = step.cloned();
    if let Some(Step::Ended(End::Finished(finished))) = &mut step
        && let Some(carry) = finished.carry.take()
    {
        session = Some(
            lash_kernel_state::semantic::session(&carry, Box::new(pages.clone()))
                .map_err(|error| failure(0, error))?,
        );
    }
    let state = machine
        .qualification_checkpoint()
        .map_err(|error| failure(0, error))?;
    let resources = machine
        .qualification_resources()
        .map_err(|error| failure(0, error))?;
    Ok(PrefixSnapshot {
        facts: None,
        canonical: Some(Box::new(CanonicalPrefix {
            state,
            session,
            resources,
            meters: machine.meters(),
            peak: machine.qualification_peak(),
            charges: lash_kernel_exec::Weight::expand(tape.steps()),
            event: event(step.as_ref()),
            stop: step.map_or_else(|| "initial".into(), |step| format!("{step:?}")),
            printed: world.printed.clone(),
            interruption,
        })),
        witness: None,
    })
}

fn preserve(mut failure: QualificationFailure, trace: &[PrefixSnapshot]) -> QualificationFailure {
    failure.reached = trace.to_vec();
    failure
}

fn validate_schedule(case: &QualificationCase) -> Result<(), QualificationFailure> {
    for &(prefix, interruption) in case.schedule() {
        if matches!(
            interruption,
            Interruption::BeforePrepareFailure
                | Interruption::BeforeCommitFailure
                | Interruption::Observer
        ) {
            return Err(failure(
                prefix as usize,
                format!("{interruption:?} has no execution injection adapter"),
            ));
        }
    }
    Ok(())
}

fn reopen(
    machine: &mut KernelMachine,
    case: &QualificationCase,
    pages: &Pages,
    strategy: crate::testing::ExecutionStrategy,
    tape: &ChargeTape,
) -> Result<(), QualificationFailure> {
    let sealed = machine
        .seal(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        })
        .map_err(|error| failure(0, error))?
        .ok_or_else(|| failure(0, "unbounded seal did not finish"))?;
    let root = lash_kernel_state::kernel_internals::checkpoint(sealed.root);
    let peak = machine.qualification_peak();
    *machine = crate::testing::open(
        case.program().clone(),
        *case.bounds(),
        &root,
        Box::new(pages.clone()),
        strategy,
    )
    .map_err(|error| failure(0, error))?;
    machine.qualification_restore_peak(peak);
    machine.record_charges(tape.clone());
    Ok(())
}

fn deliver(
    machine: &mut KernelMachine,
    pending: &mut VecDeque<Request>,
    case: &QualificationCase,
    prefix: usize,
) -> Result<(), QualificationFailure> {
    let request = pending
        .pop_front()
        .ok_or_else(|| failure(prefix, "park has no answerable pending request"))?;
    let answer = case.answers.ok_or_else(|| {
        failure(
            prefix,
            "a parked qualification case needs explicit external answers",
        )
    })?;
    let Request::Effect(effect) = &request;
    let outcome: Outcome = answer(&request);
    machine
        .deliver(effect.wait, outcome)
        .map_err(|error| failure(prefix, error))?;
    Ok(())
}

pub(super) fn source(
    case: &QualificationCase,
) -> Result<Vec<PrefixSnapshot>, QualificationFailure> {
    validate_schedule(case)?;
    let pages = Pages(Rc::new(RefCell::new(case.initial())));
    // This executor is selected before its first instruction and on every reopen.
    let mut machine = crate::testing::start(
        case.program().clone(),
        *case.bounds(),
        case.input().clone(),
        crate::testing::ExecutionStrategy::Source,
        Box::new(pages.clone()),
    )
    .map_err(|error| failure(0, error))?;
    let tape = ChargeTape::new();
    machine.record_charges(tape.clone());
    let mut world = World::default();
    let mut pending = VecDeque::new();
    let mut trace = vec![observe(&mut machine, &tape, &world, None, &pages, None)?];
    loop {
        let ordinal = trace.len() as u64 - 1;
        let interruption = case
            .schedule()
            .iter()
            .find(|(at, _)| *at == ordinal)
            .map(|(_, kind)| *kind);
        if interruption == Some(Interruption::SealReopen) {
            reopen(
                &mut machine,
                case,
                &pages,
                crate::testing::ExecutionStrategy::Source,
                &tape,
            )
            .map_err(|error| preserve(error, &trace))?;
        }
        let step = machine.run(&mut world, 0).map_err(|error| {
            let mut failure = failure(trace.len(), error);
            failure.reached = trace.clone();
            failure
        })?;
        trace.push(
            observe(
                &mut machine,
                &tape,
                &world,
                Some(&step),
                &pages,
                interruption,
            )
            .map_err(|mut error| {
                error.prefix = trace.len();
                error.reached = trace.clone();
                error
            })?,
        );
        match step {
            Step::Ended(_) => return Ok(trace),
            Step::Slice => {}
            Step::Parked(park) => {
                pending.extend(park.requests);
                if case.reopen_parks {
                    reopen(
                        &mut machine,
                        case,
                        &pages,
                        crate::testing::ExecutionStrategy::Source,
                        &tape,
                    )
                    .map_err(|error| preserve(error, &trace))?;
                }
                deliver(&mut machine, &mut pending, case, trace.len() - 1)
                    .map_err(|error| preserve(error, &trace))?;
            }
        }
    }
}

pub(super) fn isa(
    case: &QualificationCase,
    layout: IsaLayout,
) -> Result<Vec<PrefixSnapshot>, QualificationFailure> {
    validate_schedule(case)?;
    let strategy = match layout {
        IsaLayout::Generic => crate::testing::ExecutionStrategy::GenericRegions,
        IsaLayout::Specialized => crate::testing::ExecutionStrategy::Optimized,
    };
    let pages = Pages(Rc::new(RefCell::new(case.initial())));
    let mut machine = crate::testing::start(
        case.program().clone(),
        *case.bounds(),
        case.input().clone(),
        strategy,
        Box::new(pages.clone()),
    )
    .map_err(|error| failure(0, error))?;
    let tape = ChargeTape::new();
    machine.record_charges(tape.clone());
    let mut world = World::default();
    let mut pending = VecDeque::new();
    let mut trace = vec![observe(&mut machine, &tape, &world, None, &pages, None)?];
    let mut operations = 0;
    loop {
        let ordinal = trace.len() as u64 - 1;
        let interruption = case
            .schedule()
            .iter()
            .find(|(at, _)| *at == ordinal)
            .map(|(_, kind)| *kind);
        if interruption == Some(Interruption::SealReopen) {
            operations += machine.qualification_operations();
            reopen(&mut machine, case, &pages, strategy, &tape)
                .map_err(|error| preserve(error, &trace))?;
        }
        let step = machine.run(&mut world, 0).map_err(|error| {
            let mut failure = failure(trace.len(), error);
            failure.reached = trace.clone();
            failure
        })?;
        let snapshot = observe(
            &mut machine,
            &tape,
            &world,
            Some(&step),
            &pages,
            interruption,
        )
        .map_err(|mut error| {
            error.prefix = trace.len();
            error.reached = trace.clone();
            error
        })?;
        trace.push(snapshot.with_witness(ExecutionWitness {
            verified_operations: operations + machine.qualification_operations(),
            ..ExecutionWitness::default()
        }));
        match step {
            Step::Ended(_) => return Ok(trace),
            Step::Slice => {}
            Step::Parked(park) => {
                pending.extend(park.requests);
                if case.reopen_parks {
                    operations += machine.qualification_operations();
                    reopen(&mut machine, case, &pages, strategy, &tape)
                        .map_err(|error| preserve(error, &trace))?;
                }
                deliver(&mut machine, &mut pending, case, trace.len() - 1)
                    .map_err(|error| preserve(error, &trace))?;
            }
        }
    }
}
