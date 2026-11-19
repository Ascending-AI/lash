//! Differential observations at the public machine boundary.

use std::sync::Arc;

use lash_kernel_doc::Datum;
use lash_kernel_doc::ErrorDatum;

use lash_kernel_vm::{
    Bound, CellStart, ChargeTape, Delivered, End, Host, KernelMachine, Machine, Meters, Outcome,
    Program, Request, RunError, StartError, Step, Target, Weight,
};

use super::{Smith, library};
use crate::HarnessError;

use crate::projection::{Projector, Recorded};
use lash_kernel_state::semantic::Checkpoint;

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
    Print { charged: u64, value: Datum },
}

impl HostCall {
    fn charged(&self) -> u64 {
        match self {
            Self::Print { charged, .. } => *charged,
        }
    }
}

/// A Smith plan supplies its charge at each host effect, inside each slice.
pub trait SmithMachine: Machine {
    fn record_charges(&mut self, tape: ChargeTape);
}

impl SmithMachine for KernelMachine {
    fn record_charges(&mut self, tape: ChargeTape) {
        self.record_charges(tape);
    }
}

impl SmithMachine for Recorded {
    fn record_charges(&mut self, tape: ChargeTape) {
        self.record_charges(tape);
    }
}

struct World {
    calls: Vec<HostCall>,
    charges: ChargeTape,
    seed: u64,
}

impl Host for World {
    fn perform(
        &mut self,
        use_: &lash_kernel_value::OperationUse,
        _: &[Datum],
        _: &lash_kernel_value::sites::PositionLabel,
        _: &lash_kernel_value::sites::Site,
    ) -> Result<Result<Datum, ErrorDatum>, lash_kernel_vm::HostFault> {
        let lash_kernel_value::OperationSlot::Import(slot) = use_.slot else {
            return Err(lash_kernel_vm::HostFault::new(
                "unexpected helper operation",
            ));
        };
        let value = match slot.0 {
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
        self.calls.push(HostCall::Print {
            charged: self.charges.steps().into_iter().map(Weight::units).sum(),
            value: value.clone(),
        });
    }

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

fn error(data: Datum) -> ErrorDatum {
    ErrorDatum::new("scripted".into(), "scripted failure".into(), data)
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

fn observe<M: SmithMachine>(
    smith: &Smith,
    slices: &[u64],
    projection: &mut Projector<'_>,
) -> Result<Execution<Observation>, HarnessError> {
    observe_in::<M>(program(smith), smith, slices, projection)
}

fn observe_in<M: SmithMachine>(
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
                    lash_kernel_value::OperationUse {
                        link: program.link,
                        slot: (*slot).into(),
                    },
                    lash_kernel_value::OperationMode::Read,
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
        charges: ChargeTape::new(),
        seed: smith.schedule.host_seed,
    };
    let mut observations = Vec::new();
    // At least one statement is executed per positive slice. This exceeds
    // the generator's worst-case bounded loops, calls and work charge.
    machine.record_charges(host.charges.clone());
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
                        let data = if effect.use_.slot == lash_kernel_value::ImportSlot(1).into() {
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
        if projection.imports.is_some() {
            machine.record_charges(host.charges.clone());
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

fn memory_refusal(end: &End) -> bool {
    matches!(end, End::Error(RunError::Bound(error)) if error.bound == Bound::Memory)
}

fn agree(a: &Execution<Observation>, b: &Execution<Observation>) -> bool {
    agree_through_memory_cut(a, b, true, |row| {
        let end = match &row.step {
            Step::Ended(end) => Some(end),
            _ => None,
        };
        (end, row.meters.charged, &row.host_calls)
    })
}

/// Compare every completed return and host call through the earlier refusal.
/// A host call has its own charge because a slice can span the other plan's
/// refusal. At equal refusal charges both complete histories must agree.
fn agree_through_memory_cut<R: PartialEq>(
    a: &Execution<R>,
    b: &Execution<R>,
    may_finish: bool,
    observe: impl Fn(&R) -> (Option<&End>, Option<u64>, &[HostCall]),
) -> bool {
    let (Execution::Ran(a_rows), Execution::Ran(b_rows)) = (a, b) else {
        return a == b;
    };
    let (Some(a_last), Some(b_last)) = (a_rows.last(), b_rows.last()) else {
        return a == b;
    };
    let (a_end, Some(a_charge), _) = observe(a_last) else {
        return a == b;
    };
    let (b_end, Some(b_charge), _) = observe(b_last) else {
        return a == b;
    };
    let a_refused = a_end.is_some_and(memory_refusal);
    let b_refused = b_end.is_some_and(memory_refusal);
    let same_cut = a_refused && b_refused && a_charge == b_charge;
    let (earlier, later, cut) = match (a_refused, b_refused) {
        (true, true) if a_charge <= b_charge => (a_rows, b_rows, a_charge),
        (true, true) => (b_rows, a_rows, b_charge),
        (true, false) if may_finish && a_charge <= b_charge => (a_rows, b_rows, a_charge),
        (false, true) if may_finish && b_charge <= a_charge => (b_rows, a_rows, b_charge),
        _ => return a == b,
    };
    let completed = &earlier[..earlier.len() - 1];
    let later_completed = if same_cut {
        &later[..later.len() - 1]
    } else {
        let Some((prefix, remainder)) = later.split_at_checked(completed.len()) else {
            return false;
        };
        if remainder
            .first()
            .and_then(|row| observe(row).1)
            .is_some_and(|charged| charged < cut)
        {
            return false;
        }
        prefix
    };
    completed == later_completed
        && earlier.iter().flat_map(|row| observe(row).2).eq(later
            .iter()
            .flat_map(|row| observe(row).2)
            .take_while(|call| same_cut || call.charged() <= cut))
}

/// A's images opened by B at every slice and park. The public comparison
/// excludes physical root ids and memory meters.
pub fn compare<A: SmithMachine, B: SmithMachine>(smith: &Smith) -> Result<(), HarnessError> {
    smith.admit().map_err(fault)?;
    let mut reference = Projector::default();
    let a = observe::<A>(smith, &smith.schedule.slices, &mut reference)?;
    reference.check_memory()?;
    let mut imported = Projector {
        imports: Some(&reference.images),
        ..Projector::default()
    };
    let b = observe::<B>(smith, &smith.schedule.slices, &mut imported)?;
    imported.check_memory()?;
    if !agree(&a, &b) || reference.terminal != imported.terminal {
        return Err(fault(format!("cross-resume mismatch:\n{a:#?}\n{b:#?}")));
    }
    Ok(())
}

/// Cross-slice semantic agreement with a projection after every statement.
pub fn check_interpreter(smith: &Smith) -> Result<(), HarnessError> {
    compare::<Recorded, Recorded>(smith)?;
    let mut statements = smith.clone();
    statements.schedule.slices = vec![0];
    compare::<Recorded, Recorded>(&statements)?;
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

fn schedule_agree(a: &Execution<ScheduleReturn>, b: &Execution<ScheduleReturn>) -> bool {
    agree_through_memory_cut(a, b, false, |row| {
        (row.end.as_ref(), row.charged, &row.host_calls)
    })
}

// Carry storage is compared through the shared graph reader, retaining every
// other terminal field (including finish policy and typed error context).
fn semantic_end(mut end: lash_kernel_vm::End) -> lash_kernel_vm::End {
    if let lash_kernel_vm::End::Finished(finished) = &mut end {
        finished.carry = None;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_agreement_compares_effects_through_the_earlier_refusal() {
        let print = |charged, value| HostCall::Print { charged, value };
        let first = print(1, Datum::Null);
        let last = print(2, Datum::Bool(true));
        let extra = print(3, Datum::Bool(false));
        let run = |charged, host_calls| {
            Execution::Ran(vec![Observation {
                step: Step::Ended(End::Error(RunError::Bound(lash_kernel_vm::BoundExceeded {
                    bound: Bound::Memory,
                    limit: 1024,
                    function: None,
                    site: None,
                }))),
                carry: None,
                meters: Meters {
                    charged: Some(charged),
                    ..Meters::default()
                },
                after_deliveries: None,
                parked: None,
                host_calls,
                deliveries: vec![],
            }])
        };
        let cases = [
            (true, 2, vec![first.clone(), last.clone()]),
            (false, 2, vec![first.clone()]),
            (true, 3, vec![first.clone(), last.clone(), extra.clone()]),
            (false, 3, vec![first, extra]),
        ];
        for (agrees, charged, calls) in cases {
            let a = run(2, vec![print(1, Datum::Null), last.clone()]);
            let b = run(charged, calls);
            assert_eq!(agree(&a, &b), agrees, "resume: {b:?}");
            assert_eq!(
                schedule_agree(&without_slices(a), &without_slices(b)),
                agrees,
                "slices at charge {charged}"
            );
        }
    }
}
