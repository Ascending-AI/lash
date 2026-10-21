//! A minimal embedder with no dependencies on lash's host, broker or dialects.
//! The caller chooses a machine implementation and a registry.

use std::sync::Arc;

use lash_kernel_doc::{Datum, FunctionRegistry, Handle, parse_document};

use lash_kernel_vm::{
    Bounds, CellStart, EffectRequest, End, Host, Machine, Outcome, PreparedLibrary, Program,
    Request, Step, Target,
};

mod pages;
use lash_kernel_doc::heap::PageId;
/// Two children request effects before their parent joins them.
pub const FAN_OUT: &str = include_str!("../fan-out.kernel");

/// A table row, consumed exactly once when its matching request is admitted.
pub struct Answer {
    pub effect: String,
    pub args: Vec<Datum>,
    pub outcome: Outcome,
}

#[derive(Debug)]
pub struct Report {
    pub end: End,
    pub prints: Vec<Datum>,
    pub requests: Vec<EffectRequest>,
    pub parks: usize,
    pub resumes: usize,
}

/// Loads kernel text, runs it, saves and discards the machine at every park,
/// rebuilds it, then delivers table answers in reverse request order.
/// No table row is executed before the park that admits it.
pub fn embed<M: Machine>(
    text: &str,
    registry: Arc<FunctionRegistry>,
    mut answers: Vec<Answer>,
) -> Result<Report, String> {
    let document = parse_document(text).map_err(|e| e.to_string())?;
    use lash_vocabulary::{
        ContractId, ImportKind, OperationRequirement,
        package::{Fill, Link},
    };
    use std::collections::BTreeMap;
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let ImportKind::Operation { signature } = &import.kind else {
                return Err("the embedder expects Operation imports");
            };
            Ok((
                *slot,
                Fill::Operation(OperationRequirement::new(
                    signature.clone(),
                    ContractId::new("conformance"),
                )),
            ))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let link = Link {
        document: document.identity(),
        fills,
    };
    let program = Program::prepare(
        link.identity(),
        BTreeMap::from([(document.identity(), Arc::new(document))]),
        &BTreeMap::from([(link.identity(), link)]),
        PageId([7; 32]),
        PreparedLibrary::new(registry),
    )
    .map_err(|e| e.to_string())?;
    let bounds = Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: 100_000,
        memory: 1024 * 1024,
        call_depth: 64,
        live_tasks: 32,
        requests_per_park: 32,
        join_members: 32,
    };
    let source = pages::Source::default();
    let mut machine = M::start(
        program.clone(),
        bounds,
        CellStart {
            target: Target::Main,
            args: Vec::new(),
            session: None,
            owner: PageId([9; 32]),
            modes: Default::default(),
            host: Vec::new(),
        },
        Box::new(source.clone()),
    )
    .map_err(|e| e.to_string())?;
    let mut host = Output::default();
    let mut requests = Vec::new();
    let mut parks = 0;
    let mut resumes = 0;
    for _ in 0..10_000 {
        match machine
            .run(&mut host, u64::MAX)
            .map_err(|e| e.to_string())?
        {
            Step::Slice => {}
            Step::Ended(end) => {
                if !answers.is_empty() {
                    return Err("unconsumed effect answers".into());
                }
                return Ok(Report {
                    end,
                    prints: host.0,
                    requests,
                    parks,
                    resumes,
                });
            }
            Step::Parked(park) => {
                parks += 1;
                let sealed = machine.checkpoint().map_err(|e| e.to_string())?;
                machine = M::open(
                    program.clone(),
                    bounds,
                    &sealed.checkpoint(),
                    Box::new(source.clone()),
                )
                .map_err(|e| e.to_string())?;
                resumes += 1;
                for request in park.requests.into_iter().rev() {
                    match request {
                        Request::Effect(request) => {
                            let row = answers
                                .iter()
                                .position(|row| {
                                    row.effect
                                        == program.documents
                                            [&program.instances[&request.use_.link].document]
                                            .imports[&request.use_.slot]
                                            .name
                                            .member
                                            .as_str()
                                        && row.args == request.args
                                })
                                .ok_or_else(|| {
                                    format!("no answer for {} {:?}", request.use_, request.args)
                                })?;
                            let answer = answers.remove(row);
                            machine
                                .deliver(request.wait, answer.outcome)
                                .map_err(|e| e.to_string())?;
                            requests.push(request);
                        }
                    }
                }
            }
        }
    }
    Err("embedder step bound exceeded".into())
}

#[derive(Default)]
struct Output(Vec<Datum>);

impl Host for Output {
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, lash_kernel_doc::ErrorDatum> {
        Err(lash_kernel_doc::ErrorDatum {
            kind: "projection_missing".into(),
            message: "no projection registered".into(),
            data: Datum::Null,
        })
    }
    fn print(&mut self, value: &Datum) {
        self.0.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
