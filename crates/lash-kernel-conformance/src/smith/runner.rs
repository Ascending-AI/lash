//! Differential observations at the public machine boundary.

use std::sync::Arc;

use lash_kernel_doc::{Datum, ErrorDatum, Handle, Integer, Name, Timestamp, Value};
use lash_kernel_state::ParkedRun;
use lash_kernel_vm::{
    Bindings, Delivered, Host, KernelMachine, Machine, Meters, Outcome, Program, Request, Start,
    StartError, Step, Target,
};

use super::{Smith, library};
use crate::HarnessError;

/// Full observation at one return, including the canonical state at slices
/// and parks. Code caches and host cancellation probes are not observations.
#[derive(Debug, PartialEq)]
pub struct Observation {
    pub step: Step,
    pub meters: Meters,
    pub saved_memory: Option<u64>,
    pub after_deliveries: Option<ParkedRun>,
    pub parked: Option<ParkedRun>,
    pub host_calls: Vec<HostCall>,
    pub deliveries: Vec<Delivered>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HostCall {
    Clock(Timestamp),
    Random(u64),
    Read(Handle, Datum, Result<Datum, ErrorDatum>),
    Print(Datum),
}

struct World {
    state: u64,
    calls: Vec<HostCall>,
    fail: bool,
}

impl World {
    fn bits(&mut self) -> u64 {
        // A specified PRNG, independent of interpreter slice boundaries and
        // cancellation probes; only guest host reads advance it.
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut n = self.state;
        n = (n ^ (n >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        n = (n ^ (n >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        n ^ (n >> 31)
    }
}

impl Host for World {
    fn clock(&mut self) -> Timestamp {
        let time = Timestamp {
            nanoseconds: Integer::from(self.bits() as i64),
        };
        self.calls.push(HostCall::Clock(time.clone()));
        time
    }

    fn random(&mut self) -> u64 {
        let bits = self.bits();
        self.calls.push(HostCall::Random(bits));
        bits
    }

    fn read(&mut self, handle: &Handle, request: &Datum) -> Result<Datum, ErrorDatum> {
        let result = if self.fail {
            Err(error(request.clone()))
        } else {
            Ok(request.clone())
        };
        self.calls.push(HostCall::Read(
            handle.clone(),
            request.clone(),
            result.clone(),
        ));
        result
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
    Program {
        document: Arc::new(smith.document.clone()),
        library: library().prepared.clone(),
    }
}

fn start() -> Start {
    let mut bindings = Bindings::default();
    bindings.variables.insert(
        Name::new("projection"),
        Value::Handle(Arc::new(Handle {
            kind: "smith".into(),
            id: "projection".into(),
        })),
    );
    Start {
        target: Target::Main,
        args: Vec::new(),
        bindings,
    }
}

#[derive(Debug, PartialEq)]
enum Execution<O> {
    Refused(StartError),
    Ran(Vec<O>),
}

fn observe<M: Machine<Parked = ParkedRun>>(
    smith: &Smith,
    slices: &[u64],
    rebuild: bool,
) -> Result<Execution<Observation>, HarnessError> {
    if slices.is_empty() || slices.contains(&0) || smith.schedule.rebuild.is_empty() {
        return Err(HarnessError(
            "smith schedules require positive slices and rebuild choices".into(),
        ));
    }
    let program = program(smith);
    let mut machine = match M::start(program.clone(), smith.schedule.bounds, start()) {
        Ok(machine) => machine,
        Err(refusal) => return Ok(Execution::Refused(refusal)),
    };
    let mut host = World {
        state: smith.schedule.host_seed,
        calls: Vec::new(),
        fail: smith.schedule.fail_effects,
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
        let parked = if ended {
            None
        } else {
            Some(machine.export().map_err(fault)?)
        };
        let mut observation = Observation {
            meters,
            saved_memory: parked.as_ref().map(|_| machine.meters().memory),
            after_deliveries: None,
            step,
            parked,
            host_calls: std::mem::take(&mut host.calls),
            deliveries: Vec::new(),
        };
        if rebuild
            && smith.schedule.rebuild[index % smith.schedule.rebuild.len()]
            && let Some(parked) = &observation.parked
        {
            machine =
                M::import(program.clone(), smith.schedule.bounds, parked.clone()).map_err(fault)?;
        }
        if let Step::Parked(park) = &observation.step {
            let mut requests: Vec<_> = park.requests.iter().collect();
            if smith.schedule.reverse_deliveries {
                requests.reverse();
            }
            for request in requests {
                let (wait, outcome) = match request {
                    Request::Sleep(sleep) => (sleep.wait, Outcome::Elapsed),
                    Request::Effect(effect) => {
                        let data = effect.args.first().cloned().unwrap_or(Datum::Null);
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
            let parked = machine.export().map_err(fault)?;
            if rebuild && smith.schedule.rebuild[index % smith.schedule.rebuild.len()] {
                machine = M::import(program.clone(), smith.schedule.bounds, parked.clone())
                    .map_err(fault)?;
            }
            observation.after_deliveries = Some(parked);
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

/// Same schedule, two machines: every Step (including Slice), every meter,
/// host call, delivery result and exported state must agree. This seam will
/// admit the compiled implementation when it exists.
pub fn compare<A: Machine<Parked = ParkedRun>, B: Machine<Parked = ParkedRun>>(
    smith: &Smith,
) -> Result<(), HarnessError> {
    smith.admit().map_err(fault)?;
    let a = observe::<A>(smith, &smith.schedule.slices, false)?;
    let b = observe::<B>(smith, &smith.schedule.slices, true)?;
    if a != b {
        return Err(HarnessError(format!(
            "same-schedule mismatch:\n{a:#?}\n{b:#?}"
        )));
    }
    Ok(())
}

/// Today's oracle: interpreter versus rebuilt interpreter, then interpreter
/// under different positive slice sizes with the same outcome/delivery script.
/// Slices differ by construction, so that comparison joins synchronous host
/// observations across slices and compares at parks and at the terminal.
pub fn check_interpreter(smith: &Smith) -> Result<(), HarnessError> {
    compare::<KernelMachine, KernelMachine>(smith)?;
    let a = observe::<KernelMachine>(smith, &[u64::MAX], false)?;
    let b = observe::<KernelMachine>(smith, &smith.schedule.slices, true)?;
    let a = without_slices(a);
    let b = without_slices(b);
    if a != b {
        return Err(HarnessError(format!(
            "cross-slice mismatch:\n{a:#?}\n{b:#?}"
        )));
    }
    Ok(())
}

// Export performs a collection. A different number of slices therefore
// gives different collection opportunities, and terminal garbage bytes are
// not a cross-schedule invariant. At parks, compare the collected memory and
// complete canonical state. Same-schedule tier comparisons above retain the
// original Meters at *every* return, including terminal and slice returns.
#[derive(Debug, PartialEq)]
struct ScheduleReturn {
    step: Step,
    charged: u64,
    live_tasks: u32,
    saved_memory: Option<u64>,
    parked: Option<ParkedRun>,
    after_deliveries: Option<ParkedRun>,
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
            returns.push(ScheduleReturn {
                step: observation.step,
                charged: observation.meters.charged,
                live_tasks: observation.meters.live_tasks,
                saved_memory: observation.saved_memory,
                parked: observation.parked,
                after_deliveries: observation.after_deliveries,
                host_calls: std::mem::take(&mut calls),
                deliveries: observation.deliveries,
            });
        }
    }
    Execution::Ran(returns)
}
