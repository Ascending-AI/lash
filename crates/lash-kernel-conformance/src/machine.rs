//! The scripted environment adapter for the kernel's public machine seam.

use std::collections::{BTreeMap, VecDeque};
use std::marker::PhantomData;
use std::sync::Arc;

use lash_kernel_doc::{Datum, ErrorDatum, FunctionRegistry, Handle, parse_document};

use lash_kernel_vm::{
    CellStart, Delivered, Host, HostFault, Machine, MachineError, PreparedLibrary, Program,
    Request, StartError, Step, Target,
};

use crate::{Case, DocumentRunner, ExpectedEnd, HarnessError, HostAnswer, Observations, Trace};

pub struct MachineRunner<M> {
    library: PreparedLibrary,
    charge: lash_kernel_vm::Charge,
    machine: PhantomData<fn() -> M>,
}

impl<M: Machine> MachineRunner<M> {
    /// Runs the corpus with the selected charge policy. Charge oracles apply only to Exact.
    pub fn with_charge(mut self, charge: lash_kernel_vm::Charge) -> Self {
        self.library = PreparedLibrary::with_charge(Arc::clone(self.library.registry()), charge);
        self.charge = charge;
        self
    }

    pub fn new(registry: Arc<FunctionRegistry>) -> Self {
        Self {
            library: PreparedLibrary::new(registry),
            charge: lash_kernel_vm::Charge::Exact,
            machine: PhantomData,
        }
    }
}

impl<M: Machine> DocumentRunner for MachineRunner<M> {
    fn observe(&mut self, case: &Case) -> Result<Observations, HarnessError> {
        let resume = case.environment.resume;
        let mut start =
            |program, mut bounds: lash_kernel_vm::Bounds, start, source: &crate::pages::Source| {
                bounds.charge = self.charge;
                M::start(program, bounds, start, Box::new(source.clone()))
            };
        let run = Run {
            start: &mut start,
            printed: &mut || {},
            charge: None,
        };
        run_case_in::<M>(
            &self.library,
            case,
            run,
            &mut |at: Park<'_, M>| {
                if !resume {
                    return Ok(at.machine);
                }
                let mut machine = at.machine;
                let sealed = machine
                    .seal(crate::pages::BUDGET)
                    .map_err(|e| HarnessError(e.to_string()))?
                    .ok_or_else(|| HarnessError("unbounded seal incomplete".into()))?;
                crate::pages::open::<M>(
                    at.program.clone(),
                    at.bounds,
                    lash_kernel_vm::kernel_internals::sealed_root(&sealed),
                    at.source,
                )
                .map_err(|e| HarnessError(e.to_string()))
            },
            None,
        )
    }
}

/// A run at one of its parks, after the park's requests are recorded and
/// before its scripted outcomes are delivered.
pub(crate) struct Park<'a, M> {
    pub(crate) machine: M,
    /// The program the run goes on under; a harness that carries the run
    /// to another document replaces it.
    pub(crate) program: &'a mut Program,
    pub(crate) bounds: lash_kernel_vm::Bounds,
    pub(crate) source: &'a crate::pages::Source,
    #[cfg(test)]
    pub(crate) requests: &'a [Request],
}

/// How a run's machine starts.
pub(crate) type Starter<'r, M> = dyn FnMut(
        Program,
        lash_kernel_vm::Bounds,
        CellStart,
        &crate::pages::Source,
    ) -> Result<M, StartError>
    + 'r;

/// How a law instruments a run of a case: how its machine starts, what it
/// sees at each print, and a charge bound in place of the case's own.
pub(crate) struct Run<'r, M> {
    pub(crate) start: &'r mut Starter<'r, M>,
    pub(crate) printed: &'r mut dyn FnMut(),
    /// The charge bound the run takes instead of the case's. A run under
    /// it may end before its script is consumed.
    pub(crate) charge: Option<u64>,
}

/// Runs a case under its script, visiting every park.
#[cfg(test)]
pub(crate) fn run_case<M: Machine>(
    library: &PreparedLibrary,
    case: &Case,
    at_park: &mut dyn FnMut(Park<'_, M>) -> Result<M, HarnessError>,
) -> Result<Observations, HarnessError> {
    run_case_projected(library, case, at_park, None)
}

/// Runs a case under its script, projecting each slice and the final state.
#[cfg(test)]
pub(crate) fn run_case_projected<M: Machine>(
    library: &PreparedLibrary,
    case: &Case,
    at_park: &mut dyn FnMut(Park<'_, M>) -> Result<M, HarnessError>,
    projection: Option<&mut crate::projection::Projector<'_>>,
) -> Result<Observations, HarnessError> {
    let mut start =
        |program, bounds: lash_kernel_vm::Bounds, start, source: &crate::pages::Source| {
            M::start(program, bounds, start, Box::new(source.clone()))
        };
    let run = Run {
        start: &mut start,
        printed: &mut || {},
        charge: None,
    };
    run_case_in(library, case, run, at_park, projection)
}

/// Runs a case under its script, instrumented by `run`.
pub(crate) fn run_case_in<M: Machine>(
    library: &PreparedLibrary,
    case: &Case,
    run: Run<'_, M>,
    at_park: &mut dyn FnMut(Park<'_, M>) -> Result<M, HarnessError>,
    projection: Option<&mut crate::projection::Projector<'_>>,
) -> Result<Observations, HarnessError> {
    let Some(program) = case_program(library, case) else {
        return Ok(refused());
    };
    run_program_in(program, case, run, at_park, projection)
}

fn refused() -> Observations {
    Observations {
        prints: Vec::new(),
        end: ExpectedEnd::Refused,
        trace: Vec::new(),
        charged: None,
        parks: 0,
    }
}

/// Parses and admits a case once. Retaining the document allocation lets
/// the prepared library reuse its executable across a law's sweep points.
pub(crate) fn case_program(library: &PreparedLibrary, case: &Case) -> Option<Program> {
    let document = parse_document(&case.document).ok()?;
    lash_kernel_check::admit(
        &document,
        &lash_kernel_check::Environment::new(library.registry().as_ref()),
    )
    .ok()?;
    prepare_document(
        library,
        Arc::new(document),
        case.environment.effects.as_ref(),
    )
}

pub(crate) fn prepare_document(
    library: &PreparedLibrary,
    document: Arc<lash_kernel_doc::Document>,
    offered: Option<&BTreeMap<lash_vocabulary::EffectName, lash_vocabulary::Signature>>,
) -> Option<Program> {
    use lash_vocabulary::{
        ContractId, ImportKind, OperationRequirement,
        package::{Fill, Link},
    };
    // Dialect witnesses retain the helper release that fulfils their Code Imports.
    // The independently authored machine corpus has only Operation Imports.
    #[cfg(test)]
    if document
        .imports
        .values()
        .any(|import| !matches!(import.kind, ImportKind::Operation { .. }))
    {
        return Some(crate::linked::program(document, library.clone()));
    }
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let ImportKind::Operation { signature } = &import.kind else {
                return None;
            };
            let signature = match offered {
                Some(offered) => {
                    offered.get(&lash_vocabulary::EffectName::new(import.name.to_string()).ok()?)?
                }
                None => signature,
            };
            Some((
                *slot,
                Fill::Operation(OperationRequirement::new(
                    signature.clone(),
                    ContractId::new("conformance"),
                )),
            ))
        })
        .collect::<Option<BTreeMap<_, _>>>()?;
    let link = Link {
        document: document.identity(),
        fills,
    };
    lash_kernel_package::LinkedContent {
        documents: BTreeMap::from([(document.identity(), (*document).clone())]),
        ..Default::default()
    }
    .check_link(&link)
    .ok()?;
    Program::prepare(
        link.identity(),
        BTreeMap::from([(document.identity(), document)]),
        &BTreeMap::from([(link.identity(), link)]),
        crate::pages::CLOSURE,
        library.clone(),
    )
    .ok()
}

/// Runs an already admitted case, retaining its prepared code but starting
/// fresh execution, host script and page state at every sweep point.
pub(crate) fn run_program_in<M: Machine>(
    mut program: Program,
    case: &Case,
    run: Run<'_, M>,
    at_park: &mut dyn FnMut(Park<'_, M>) -> Result<M, HarnessError>,
    mut projection: Option<&mut crate::projection::Projector<'_>>,
) -> Result<Observations, HarnessError> {
    let env = &case.environment;
    if env.reads.iter().any(|name| {
        !program.document.imports.values().any(|import| {
            matches!(import.kind, lash_vocabulary::ImportKind::Operation { .. })
                && name.as_str() == import.name.to_string()
        })
    }) {
        return Ok(refused());
    }
    let mut bounds: lash_kernel_vm::Bounds = env.bounds.into();
    if let Some(charge) = run.charge {
        bounds.charge_limit = charge;
    }
    let cut = run.charge.is_some();
    let source = projection
        .as_ref()
        .and_then(|p| p.imports)
        .and_then(|images| images.first())
        .map(|image| image.source.clone())
        .unwrap_or_default();
    let start = CellStart {
        target: env.entry.clone().map_or(Target::Main, Target::Entry),
        args: env.args.clone(),
        session: None,
        owner: crate::pages::OWNER,
        modes: program
            .document
            .imports
            .iter()
            .filter(|(_, import)| {
                env.reads
                    .iter()
                    .any(|name| name.as_str() == import.name.to_string())
            })
            .map(|(slot, _)| {
                (
                    lash_vocabulary::OperationUse {
                        link: program.link,
                        slot: *slot,
                    },
                    lash_vocabulary::package::OperationMode::Read,
                )
            })
            .collect(),
        host: Vec::new(),
    };
    let mut machine = match (run.start)(program.clone(), bounds, start, &source) {
        Ok(machine) => machine,
        Err(StartError::NotAdmitted(_) | StartError::UndeclaredRead { .. }) => {
            return Ok(refused());
        }
        Err(error) => return Err(HarnessError(error.to_string())),
    };
    let mut host = ScriptHost {
        answers: env.host.clone().into(),
        prints: Vec::new(),
        fault: None,
        printed: run.printed,
        operations: program
            .instances
            .iter()
            .flat_map(|(link, instance)| {
                program.documents[&instance.document]
                    .imports
                    .iter()
                    .map(move |(slot, import)| {
                        (
                            lash_vocabulary::OperationUse {
                                link: *link,
                                slot: *slot,
                            },
                            import.name.to_string(),
                        )
                    })
            })
            .collect(),
    };
    let mut batches = env.deliveries.iter();
    let mut waits = Vec::new();
    let mut trace = Vec::new();
    let mut parks = 0;
    if env.slice == 0 && projection.is_none() {
        return Err(HarnessError("a corpus slice must be positive".into()));
    }
    for _ in 0..env.max_steps {
        let step = machine.run(&mut host, env.slice);
        if let Some(fault) = host.fault.take() {
            return Err(HarnessError(fault));
        }
        let step = match step {
            Ok(step) => step,
            Err(MachineError::Host(_)) => {
                if !cut && (batches.next().is_some() || !host.answers.is_empty()) {
                    return Err(HarnessError(
                        "run stopped before its script was consumed".into(),
                    ));
                }
                return Ok(Observations {
                    prints: host.prints,
                    end: ExpectedEnd::HostFault,
                    trace,
                    charged: machine.meters().charged,
                    parks,
                });
            }
            Err(error) => return Err(HarnessError(error.to_string())),
        };
        match step {
            Step::Slice => {
                if let Some(p) = &mut projection {
                    p.at(&mut machine, &program, bounds, &source)?;
                }
            }
            Step::Ended(end) => {
                if let Some(p) = &mut projection {
                    p.end(&end, &source, machine.meters().memory)?;
                }
                if !cut && (batches.next().is_some() || !host.answers.is_empty()) {
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
                for request in &park.requests {
                    match request {
                        Request::Effect(effect) => {
                            waits.push(effect.wait);
                            trace.push(Trace::Effect {
                                identity: Some(
                                    lash_kernel_doc::kernel_internals::effect_parts(
                                        &effect.identity,
                                    )
                                    .clone(),
                                ),
                                effect: host.operations.get(&effect.use_).cloned().ok_or_else(
                                    || HarnessError("undeclared Operation Use".into()),
                                )?,
                                args: effect.args.clone(),
                                result: effect.result.clone(),
                            });
                        }
                    }
                }
                if let Some(p) = &mut projection {
                    p.at(&mut machine, &program, bounds, &source)?;
                }
                machine = at_park(Park {
                    machine,
                    program: &mut program,
                    bounds,
                    source: &source,
                    #[cfg(test)]
                    requests: &park.requests,
                })?;
                let batch = batches.next().ok_or_else(|| {
                    HarnessError(format!("park {parks} has no scripted delivery batch"))
                })?;
                for delivery in batch {
                    let wait = waits.get(delivery.request).ok_or_else(|| {
                        HarnessError(format!("request {} has not been issued", delivery.request))
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
                if let Some(p) = &mut projection {
                    p.at(&mut machine, &program, bounds, &source)?;
                }
            }
        }
    }
    Err(HarnessError(format!(
        "run passed the harness step bound {}",
        env.max_steps
    )))
}

struct ScriptHost<'r> {
    answers: VecDeque<HostAnswer>,
    prints: Vec<Datum>,
    fault: Option<String>,
    printed: &'r mut dyn FnMut(),
    operations: BTreeMap<lash_vocabulary::OperationUse, String>,
}

impl ScriptHost<'_> {
    fn mismatch(&mut self, expected: &str, answer: Option<HostAnswer>) {
        self.fault.get_or_insert_with(|| {
            format!("host requested {expected}, next answer was {answer:?}")
        });
    }
}

impl Host for ScriptHost<'_> {
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

    fn perform(
        &mut self,
        effect: &lash_vocabulary::names::OperationUse,
        args: &[Datum],
        _site: &lash_vocabulary::sites::PositionLabel,
        _origin: &lash_vocabulary::sites::Site,
    ) -> Result<Result<Datum, ErrorDatum>, HostFault> {
        match self.answers.pop_front() {
            Some(HostAnswer::Perform(perform))
                if self
                    .operations
                    .get(effect)
                    .is_some_and(|name| name == perform.effect.as_str())
                    && perform.args == args =>
            {
                perform
                    .answer
                    .ok_or_else(|| HostFault::new("the scripted host cannot answer the read"))
            }
            answer => {
                self.mismatch("matching read-bound perform", answer);
                Err(HostFault::new("unscripted read-bound perform"))
            }
        }
    }

    fn print(&mut self, value: &Datum) {
        (self.printed)();
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
