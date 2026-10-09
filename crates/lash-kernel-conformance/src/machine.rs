//! The scripted environment adapter for the kernel's public machine seam.

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::sync::Arc;

use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionRegistry, Handle, Integer, Timestamp, parse_document,
};
use lash_kernel_vm::{
    Bindings, Delivered, Host, Machine, Program, Request, Start, StartError, Step, Target,
};

use crate::{Case, DocumentRunner, ExpectedEnd, HarnessError, HostAnswer, Observations, Trace};

pub struct MachineRunner<M> {
    registry: Arc<FunctionRegistry>,
    machine: PhantomData<fn() -> M>,
}

impl<M: Machine> MachineRunner<M> {
    pub fn new(registry: Arc<FunctionRegistry>) -> Self {
        Self {
            registry,
            machine: PhantomData,
        }
    }
}

impl<M: Machine> DocumentRunner for MachineRunner<M> {
    fn observe(&mut self, case: &Case) -> Result<Observations, HarnessError> {
        let refused = || Observations {
            prints: Vec::new(),
            end: ExpectedEnd::Refused,
            trace: Vec::new(),
            charged: 0,
            parks: 0,
        };
        let document = match parse_document(&case.document) {
            Ok(document) => document,
            Err(_) => return Ok(refused()),
        };
        let env = &case.environment;
        let mut admission = lash_kernel_check::Environment::new(self.registry.as_ref());
        admission.effects = env
            .effects
            .clone()
            .unwrap_or_else(|| document.manifest.effects.clone());
        if lash_kernel_check::admit(&document, &admission).is_err() {
            return Ok(refused());
        }
        let program = Program {
            document: Arc::new(document),
            registry: self.registry.clone(),
        };
        let bounds = env.bounds.into();
        let start = Start {
            target: env.entry.clone().map_or(Target::Main, Target::Entry),
            args: env.args.clone(),
            bindings: Bindings::default(),
        };
        let mut machine = match M::start(program.clone(), bounds, start) {
            Ok(machine) => machine,
            Err(StartError::NotAdmitted(_)) => return Ok(refused()),
            Err(error) => return Err(HarnessError(error.to_string())),
        };
        let mut host = ScriptHost {
            answers: env.host.clone().into(),
            prints: Vec::new(),
            fault: None,
        };
        let mut batches = env.deliveries.iter();
        let mut waits = Vec::new();
        let mut trace = Vec::new();
        let mut parks = 0;
        if env.slice == 0 {
            return Err(HarnessError("a corpus slice must be positive".into()));
        }
        for _ in 0..env.max_steps {
            let step = machine
                .run(&mut host, env.slice)
                .map_err(|error| HarnessError(error.to_string()))?;
            if let Some(fault) = host.fault.take() {
                return Err(HarnessError(fault));
            }
            match step {
                Step::Slice => {}
                Step::Ended(end) => {
                    if batches.next().is_some() || !host.answers.is_empty() {
                        return Err(HarnessError(
                            "run ended before its script was consumed".into(),
                        ));
                    }
                    return Ok(Observations {
                        prints: host.prints,
                        end: ExpectedEnd::from_end(end),
                        trace,
                        charged: machine.meters().charged,
                        parks,
                    });
                }
                Step::Parked(park) => {
                    parks += 1;
                    for request in park.requests {
                        match request {
                            Request::Effect(effect) => {
                                waits.push(effect.wait);
                                trace.push(Trace::Effect {
                                    identity: Some(effect.identity),
                                    effect: effect.effect.to_string(),
                                    args: effect.args,
                                    result: effect.result,
                                });
                            }
                            Request::Sleep(sleep) => {
                                waits.push(sleep.wait);
                                trace.push(Trace::Sleep {
                                    identity: Some(sleep.identity),
                                    nanoseconds: sleep.duration.as_nanos(),
                                });
                            }
                        }
                    }
                    if env.resume {
                        let parked = machine.export().map_err(|e| HarnessError(e.to_string()))?;
                        machine = M::import(program.clone(), bounds, parked)
                            .map_err(|e| HarnessError(e.to_string()))?;
                    }
                    let batch = batches.next().ok_or_else(|| {
                        HarnessError(format!("park {parks} has no scripted delivery batch"))
                    })?;
                    for delivery in batch {
                        let wait = waits.get(delivery.request).ok_or_else(|| {
                            HarnessError(format!(
                                "request {} has not been issued",
                                delivery.request
                            ))
                        })?;
                        let delivered = machine
                            .deliver(*wait, delivery.outcome.clone().into())
                            .map_err(|e| HarnessError(e.to_string()))?;
                        if (delivered == Delivered::Dropped) != delivery.dropped {
                            return Err(HarnessError(format!(
                                "unexpected delivery result {delivered:?}"
                            )));
                        }
                    }
                }
            }
        }
        Err(HarnessError(format!(
            "run passed the harness step bound {}",
            env.max_steps
        )))
    }
}

struct ScriptHost {
    answers: VecDeque<HostAnswer>,
    prints: Vec<Datum>,
    fault: Option<String>,
}

impl ScriptHost {
    fn mismatch(&mut self, expected: &str, answer: Option<HostAnswer>) {
        self.fault.get_or_insert_with(|| {
            format!("host requested {expected}, next answer was {answer:?}")
        });
    }
}

impl Host for ScriptHost {
    fn clock(&mut self) -> Timestamp {
        match self.answers.pop_front() {
            Some(HostAnswer::Clock(time)) => time,
            answer => {
                self.mismatch("clock", answer);
                Timestamp {
                    nanoseconds: Integer::from(0),
                }
            }
        }
    }

    fn random(&mut self) -> u64 {
        match self.answers.pop_front() {
            Some(HostAnswer::Random(bits)) => bits,
            answer => {
                self.mismatch("random", answer);
                0
            }
        }
    }

    fn read(&mut self, handle: &Handle, request: &Datum) -> Result<Datum, ErrorDatum> {
        match self.answers.pop_front() {
            Some(HostAnswer::Read(read)) if read.handle == *handle && read.request == *request => {
                read.answer
            }
            answer => {
                self.mismatch("matching projection read", answer);
                Err(ErrorDatum {
                    kind: "script_error".into(),
                    message: "unscripted read".into(),
                    data: Datum::Null,
                })
            }
        }
    }

    fn print(&mut self, value: &Datum) {
        self.prints.push(value.clone());
    }

    fn cancel_requested(&mut self) -> bool {
        // Cancellation checks are not guest host reads. Unless explicitly
        // scripted, the embedder has no pending cancellation.
        if matches!(self.answers.front(), Some(HostAnswer::Cancel(_))) {
            match self.answers.pop_front() {
                Some(HostAnswer::Cancel(cancel)) => cancel,
                _ => false,
            }
        } else {
            false
        }
    }
}
