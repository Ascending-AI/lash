//! Differential observations at the public machine boundary.

use std::sync::Arc;

use lash_kernel_doc::Datum;
use lash_kernel_doc::ErrorDatum;

use lash_kernel_vm::{
    CellStart, Delivered, Host, KernelMachine, Machine, Meters, Outcome, Program, Request,
    StartError, Step, Target,
};

use super::{Smith, library};
use crate::HarnessError;

use crate::projection::Projector;
use lash_kernel_vm::kernel_internals::conformance::Checkpoint;

/// Full observation at one return, including the canonical state at slices
/// and parks. Code caches and host cancellation probes are not observations.
#[derive(Debug)]
pub struct Observation {
    pub step: Step,
    pub carry: Option<Checkpoint>,
    pub meters: Meters,
    pub after_deliveries: Option<Checkpoint>,
    pub parked: Option<Checkpoint>,
    pub host_calls: Vec<HostCall>,
    pub deliveries: Vec<Delivered>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostCall {
    Print(Datum),
}

struct World {
    calls: Vec<HostCall>,
    seed: u64,
}

impl Host for World {
    fn perform(
        &mut self,
        use_: &lash_vocabulary::OperationUse,
        _: &[Datum],
        _: &lash_vocabulary::sites::PositionLabel,
        _: &lash_vocabulary::sites::Site,
    ) -> Result<Result<Datum, ErrorDatum>, lash_kernel_vm::HostFault> {
        let value = match use_.slot.0 {
            2 => Datum::Timestamp(lash_kernel_doc::Timestamp {
                nanoseconds: lash_kernel_doc::Integer::new(self.seed),
            }),
            3 => {
                let value = Datum::Bytes(lash_kernel_doc::Bytes::new(
                    self.seed.to_be_bytes().to_vec(),
                ));
                self.seed = self.seed.wrapping_add(1);
                value
            }
            _ => {
                return Err(lash_kernel_vm::HostFault {
                    problem: "the Smith fixture has no other read operation".into(),
                });
            }
        };
        Ok(Ok(value))
    }

    fn print(&mut self, value: &Datum) {
        self.calls.push(HostCall::Print(value.clone()));
    }

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

fn error(data: Datum) -> ErrorDatum {
    ErrorDatum {
        kind: "scripted".into(),
        message: "scripted failure".into(),
        data,
    }
}

fn fault(error: impl std::fmt::Display) -> HarnessError {
    HarnessError(error.to_string())
}

fn program(smith: &Smith) -> Program {
    crate::machine::prepare_document(&library().prepared, Arc::new(smith.document.clone()), None)
        .unwrap_or_else(|| panic!("smith document has no prepared linked fixture"))
}

fn session() -> crate::pages::Session {
    crate::pages::Session::seeded([])
}

#[derive(Debug, PartialEq)]
enum Execution<O> {
    Refused(StartError),
    Ran(Vec<O>),
}

fn observe<M: Machine>(
    smith: &Smith,
    slices: &[u64],
    projection: &mut Projector<'_>,
) -> Result<Execution<Observation>, HarnessError> {
    observe_in::<M>(program(smith), smith, slices, projection)
}

fn observe_in<M: Machine>(
    program: Program,
    smith: &Smith,
    slices: &[u64],
    projection: &mut Projector<'_>,
) -> Result<Execution<Observation>, HarnessError> {
    if slices.is_empty() {
        return Err(HarnessError("smith schedules require slices".into()));
    }
    let mut session = session();
    if let Some(image) = projection.imports.and_then(|images| images.first()) {
        session.source = image.source.clone();
    }
    let source = &session.source;
    let start = CellStart {
        owner: crate::pages::OWNER,
        target: Target::Main,
        args: Vec::new(),
        session: session.root,
        modes: smith
            .document
            .imports
            .iter()
            .filter_map(|(slot, import)| {
                matches!(
                    import.name.to_string().as_str(),
                    "typescript.clock" | "typescript.random"
                )
                .then_some((
                    lash_vocabulary::OperationUse {
                        link: program.link,
                        slot: *slot,
                    },
                    lash_vocabulary::OperationMode::Read,
                ))
            })
            .collect(),
        host: Vec::new(),
    };
    let mut machine = match M::start(
        program.clone(),
        smith.schedule.bounds,
        start,
        Box::new(source.clone()),
    ) {
        Ok(machine) => machine,
        Err(refusal) => return Ok(Execution::Refused(refusal)),
    };
    let mut host = World {
        calls: Vec::new(),
        seed: smith.schedule.host_seed,
    };
    let mut observations = Vec::new();
    // At least one statement is executed per positive slice. This exceeds
    // the generator's worst-case bounded loops, calls and work charge.
    for index in 0..25_000 {
        let step = machine
            .run(&mut host, slices[index % slices.len()])
            .map_err(fault)?;
        let meters = machine.meters();
        let ended = matches!(step, Step::Ended(_));
        if let Step::Ended(end) = &step {
            projection.end(end, source, meters.memory)?;
        }
        let parked = if ended {
            None
        } else {
            Some(projection.at(&mut machine, &program, smith.schedule.bounds, source)?)
        };
        let mut observation = Observation {
            meters,
            carry: if ended {
                projection.terminal.clone()
            } else {
                None
            },
            after_deliveries: None,
            step,
            parked,
            host_calls: std::mem::take(&mut host.calls),
            deliveries: Vec::new(),
        };
        if let Step::Parked(park) = &observation.step {
            let mut requests: Vec<_> = park.requests.iter().collect();
            if smith.schedule.reverse_deliveries {
                requests.reverse();
            }
            for request in requests {
                let (wait, outcome) = match request {
                    Request::Effect(effect) => {
                        let data = if effect.use_.slot == lash_vocabulary::ImportSlot(1) {
                            Datum::Null
                        } else {
                            effect.args.first().cloned().unwrap_or(Datum::Null)
                        };
                        (
                            effect.wait,
                            if smith.schedule.fail_effects {
                                Outcome::Failed(error(data))
                            } else {
                                Outcome::Completed(data)
                            },
                        )
                    }
                };
                // Withdrawals are observable. A request and its withdrawal
                // can be handed out together; its late delivery must drop.
                observation
                    .deliveries
                    .push(machine.deliver(wait, outcome).map_err(fault)?);
            }
            let parked =
                Some(projection.at(&mut machine, &program, smith.schedule.bounds, source)?);
            observation.after_deliveries = parked;
        }
        observations.push(observation);
        if ended {
            return Ok(Execution::Ran(observations));
        }
    }
    Err(HarnessError(
        "generated run exhausted its step bound".into(),
    ))
}

impl PartialEq for Observation {
    fn eq(&self, other: &Self) -> bool {
        (match (&self.step, &other.step) {
            (Step::Ended(a), Step::Ended(b)) => semantic_end(a.clone()) == semantic_end(b.clone()),
            (a, b) => a == b,
        }) && self.meters.charged == other.meters.charged
            && self.meters.live_tasks == other.meters.live_tasks
            && self.after_deliveries == other.after_deliveries
            && self.parked == other.parked
            && self.host_calls == other.host_calls
            && self.deliveries == other.deliveries
    }
}

fn memory(execution: &Execution<Observation>) -> bool {
    match execution {
        Execution::Ran(rows) => rows.last().is_some_and(|row| {
            matches!(
                row.step,
                Step::Ended(lash_kernel_vm::End::Error(lash_kernel_vm::RunError::Bound(
                    lash_kernel_vm::BoundExceeded {
                        bound: lash_kernel_vm::Bound::Memory,
                        ..
                    }
                )))
            )
        }),
        _ => false,
    }
}

/// Memory is a separate verdict. A refusal's precise charge/statement is
/// plan-dependent; all completed observations before the earlier refusal agree.
fn agree(a: &Execution<Observation>, b: &Execution<Observation>) -> bool {
    if !memory(a) && !memory(b) {
        return a == b;
    }
    match (a, b) {
        (Execution::Ran(a), Execution::Ran(b)) => {
            let prefix = a.len().min(b.len()).saturating_sub(1);
            a[..prefix] == b[..prefix]
                && a.iter()
                    .flat_map(|r| &r.host_calls)
                    .zip(b.iter().flat_map(|r| &r.host_calls))
                    .all(|(a, b)| a == b)
        }
        _ => true,
    }
}

/// A's images opened by B at every slice and park, with warm and cold code
/// caches. The public comparison excludes physical root ids and memory meters.
pub fn compare<A: Machine, B: Machine>(smith: &Smith) -> Result<(), HarnessError> {
    compare_plans::<A, B>(
        smith,
        lash_kernel_vm::kernel_internals::Layout::default(),
        lash_kernel_vm::kernel_internals::Layout::default(),
    )
}

fn compare_plans<A: Machine, B: Machine>(
    smith: &Smith,
    a_layout: lash_kernel_vm::kernel_internals::Layout,
    b_layout: lash_kernel_vm::kernel_internals::Layout,
) -> Result<(), HarnessError> {
    smith.admit().map_err(fault)?;
    let mut reference = Projector {
        layout: a_layout,
        ..Projector::default()
    };
    let a = observe::<A>(smith, &smith.schedule.slices, &mut reference)?;
    reference.check_memory()?;
    for cold in [false, true] {
        let mut imported = Projector {
            imports: Some(&reference.images),
            cold,
            layout: b_layout,
            ..Projector::default()
        };
        let b = observe::<B>(smith, &smith.schedule.slices, &mut imported)?;
        imported.check_memory()?;
        if !agree(&a, &b) || reference.terminal != imported.terminal {
            return Err(fault(format!(
                "cross-resume mismatch (cold={cold}):\n{a:#?}\n{b:#?}"
            )));
        }
    }
    Ok(())
}

/// Cross-slice semantic agreement with a projection after every statement.
pub fn check_interpreter(smith: &Smith) -> Result<(), HarnessError> {
    compare_plans::<crate::projection::Plan<0>, crate::projection::Plan<3>>(
        smith,
        lash_kernel_vm::kernel_internals::Layout(0),
        lash_kernel_vm::kernel_internals::Layout(3),
    )?;
    let mut statements = smith.clone();
    statements.schedule.slices = vec![0];
    compare_plans::<crate::projection::Plan<0>, crate::projection::Plan<3>>(
        &statements,
        lash_kernel_vm::kernel_internals::Layout(0),
        lash_kernel_vm::kernel_internals::Layout(3),
    )?;
    let a = without_slices(observe::<KernelMachine>(
        smith,
        &[u64::MAX],
        &mut Projector::default(),
    )?);
    for slices in [&smith.schedule.slices[..], &[0][..]] {
        let b = without_slices(observe::<KernelMachine>(
            smith,
            slices,
            &mut Projector::default(),
        )?);
        if !schedule_agree(&a, &b) {
            return Err(fault(format!("cross-slice mismatch:\n{a:#?}\n{b:#?}")));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn check_slices(smith: &Smith) -> Result<(), HarnessError> {
    check_interpreter(smith)
}

#[derive(Debug, PartialEq)]
struct ScheduleReturn {
    end: Option<lash_kernel_vm::End>,
    carry: Option<Checkpoint>,
    park: Option<lash_kernel_vm::Park>,
    charged: Option<u64>,
    live_tasks: u32,
    parked: Option<Checkpoint>,
    after_deliveries: Option<Checkpoint>,
    host_calls: Vec<HostCall>,
    deliveries: Vec<Delivered>,
}

fn without_slices(execution: Execution<Observation>) -> Execution<ScheduleReturn> {
    let observations = match execution {
        Execution::Refused(refusal) => return Execution::Refused(refusal),
        Execution::Ran(observations) => observations,
    };
    let mut calls = Vec::new();
    let mut returns = Vec::new();
    for mut observation in observations {
        calls.append(&mut observation.host_calls);
        if !matches!(observation.step, Step::Slice) {
            let (end, park) = match observation.step {
                Step::Ended(end) => (Some(semantic_end(end)), None),
                Step::Parked(park) => (None, Some(park)),
                Step::Slice => unreachable!(),
            };
            returns.push(ScheduleReturn {
                end,
                carry: observation.carry,
                park,
                charged: observation.meters.charged,
                live_tasks: observation.meters.live_tasks,
                parked: observation.parked,
                after_deliveries: observation.after_deliveries,
                host_calls: std::mem::take(&mut calls),
                deliveries: observation.deliveries,
            });
        }
    }
    Execution::Ran(returns)
}

#[cfg(test)]
pub(super) fn check_memory(smith: &Smith) -> Result<(), HarnessError> {
    let mut a = Projector::default();
    let warm = observe::<crate::projection::Plan<0>>(smith, &[0], &mut a)?;
    a.check_memory()?;
    let mut b = Projector {
        imports: Some(&a.images),
        cold: true,
        layout: lash_kernel_vm::kernel_internals::Layout(3),
        ..Projector::default()
    };
    let cold = observe::<crate::projection::Plan<3>>(smith, &[0], &mut b)?;
    if !agree(&warm, &cold) {
        return Err(fault("plans disagree before the earlier memory refusal"));
    }
    b.check_memory()
}

fn schedule_agree(a: &Execution<ScheduleReturn>, b: &Execution<ScheduleReturn>) -> bool {
    let refused = |rows: &[ScheduleReturn]| {
        rows.last().is_some_and(|row| {
            matches!(
                &row.end,
                Some(lash_kernel_vm::End::Error(lash_kernel_vm::RunError::Bound(
                    lash_kernel_vm::BoundExceeded {
                        bound: lash_kernel_vm::Bound::Memory,
                        ..
                    }
                )))
            )
        })
    };
    match (a, b) {
        (Execution::Ran(a), Execution::Ran(b)) if refused(a) && refused(b) => {
            let prefix = a.len().min(b.len()).saturating_sub(1);
            a[..prefix] == b[..prefix]
                && a.iter()
                    .flat_map(|r| &r.host_calls)
                    .zip(b.iter().flat_map(|r| &r.host_calls))
                    .all(|(a, b)| a == b)
        }
        _ => a == b,
    }
}

// Carry storage is compared through the shared graph reader, retaining every
// other terminal field (including finish policy and typed error context).
fn semantic_end(mut end: lash_kernel_vm::End) -> lash_kernel_vm::End {
    if let lash_kernel_vm::End::Finished(finished) = &mut end {
        finished.carry = None;
    }
    end
}

/// A smith program's two prepared layouts, shared by every schedule and
/// charge bound of the regions PLAN-INDEP law (§3.3). Each observation still
/// starts a fresh machine and opens every checkpoint witness.
#[cfg(test)]
pub(crate) struct Plans<const A: u64, const B: u64> {
    a: Program,
    b: Program,
}

#[cfg(test)]
impl<const A: u64, const B: u64> Plans<A, B> {
    pub(crate) fn new(smith: &Smith) -> Self {
        use lash_kernel_vm::kernel_internals::{Layout, prepare_with_layout};
        let mut a = program(smith);
        let mut b = a.clone();
        a.library = prepare_with_layout(Arc::clone(a.library.registry()), Layout(A));
        b.library = prepare_with_layout(Arc::clone(b.library.registry()), Layout(B));
        Self { a, b }
    }

    /// Compares the end, charge tape and checkpoints, with each plan also
    /// resuming the other's images at every slice and park. Only the smith's
    /// schedule changes within a sweep; its document is held by these plans.
    pub(crate) fn agree(&self, smith: &Smith, slices: &[u64]) -> Result<u64, HarnessError> {
        use crate::projection::{Plan, taped};
        use lash_kernel_vm::kernel_internals::Layout;
        let projector = |layout: u64, program: &Program, imports| Projector {
            imports,
            layout: Layout(layout),
            library: Some(program.library.clone()),
            ..Projector::default()
        };
        let (mut a_images, mut b_images) =
            (projector(A, &self.a, None), projector(B, &self.b, None));
        let (a, a_tape) =
            taped(|| observe_in::<Plan<A>>(self.a.clone(), smith, slices, &mut a_images));
        let (b, b_tape) =
            taped(|| observe_in::<Plan<B>>(self.b.clone(), smith, slices, &mut b_images));
        let (a, b) = (a?, b?);
        let expanded = |tape: &[lash_kernel_exec::charge::Weight]| {
            lash_kernel_exec::charge::Weight::expand(tape.iter().copied())
        };
        if !agree(&a, &b)
            || a_images.terminal != b_images.terminal
            || (!memory(&a) && !memory(&b) && expanded(&a_tape) != expanded(&b_tape))
        {
            return Err(fault(format!("plans disagree:\n{a:#?}\n{b:#?}")));
        }
        let mut b_resuming = projector(B, &self.b, Some(&a_images.images));
        let resumed = observe_in::<Plan<B>>(self.b.clone(), smith, slices, &mut b_resuming)?;
        if !agree(&a, &resumed) || a_images.terminal != b_resuming.terminal {
            return Err(fault(format!(
                "B resuming A's images:\n{a:#?}\n{resumed:#?}"
            )));
        }
        let mut a_resuming = projector(A, &self.a, Some(&b_images.images));
        let resumed = observe_in::<Plan<A>>(self.a.clone(), smith, slices, &mut a_resuming)?;
        if !agree(&b, &resumed) || b_images.terminal != a_resuming.terminal {
            return Err(fault(format!(
                "A resuming B's images:\n{b:#?}\n{resumed:#?}"
            )));
        }
        Ok(match &a {
            Execution::Ran(rows) => rows.last().and_then(|row| row.meters.charged).unwrap_or(0),
            Execution::Refused(_) => 0,
        })
    }
}
