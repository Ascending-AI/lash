//! Worker-owned fresh-cell measurements for the developer comparison harness.
//!
//! This testing-only adapter runs the Test262 production embedding in process,
//! without IPC or durable-engine overhead. Preparation and machine ownership
//! stay here; callers receive only an opaque prepared cell and measurements.
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_dialect::{CellEnvironment, Environment, Mode, NamedLibrary, Namespaces};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::{Datum, ErrorDatum};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_vm::perf_witness::Profiling;
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, PreparedLibrary, Program, Step, Target,
};
use lash_vocabulary::package::{Fill, Link};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T, E = Error> = std::result::Result<T, E>;

fn failure(message: impl std::fmt::Display) -> Error {
    message.to_string().into()
}

/// One fresh-cell run, timed through its printed checksum.
pub struct Measurement {
    pub ns: f64,
    pub result: serde_json::Value,
    /// Exact physical execution of the separate low-count diagnostic cell.
    pub execution_census: Option<lash_kernel_vm::perf_witness::execution::Census>,
}

/// A linked cell whose VM program remains owned by the worker.
pub struct PreparedCell(Program);

fn elapsed(at: std::time::Instant) -> f64 {
    at.elapsed().as_secs_f64() * 1e9
}

#[derive(Default)]
struct Pages(BTreeMap<PageId, PageAnswer>);
impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|p| {
                self.0
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect())
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        self.0.insert(
            page.id,
            PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}

#[derive(Default)]
struct Console(Vec<Datum>, bool);
impl Host for Console {
    fn observes_sites(&self) -> bool {
        self.1
    }
    fn perform(
        &mut self,
        _: &lash_vocabulary::OperationUse,
        _: &[Datum],
        _: &lash_vocabulary::sites::PositionLabel,
        _: &lash_vocabulary::sites::Site,
    ) -> Result<Result<Datum, ErrorDatum>, lash_kernel_vm::HostFault> {
        Err(lash_kernel_vm::HostFault::new(
            "compare workloads must be effect-free",
        ))
    }
    fn print(&mut self, value: &Datum) {
        self.0.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// The process-once production embedding used by comparison measurements.
pub struct Kernel {
    registry: Arc<lash_kernel_doc::FunctionRegistry>,
    library: PreparedLibrary,
    pub setup_ns: f64,
}
impl Kernel {
    /// Assemble the same natives and released helpers as Test262.
    ///
    /// # Errors
    /// Refused embedding or helper release.
    pub fn new() -> Result<Self> {
        let at = std::time::Instant::now();
        let embedding = crate::built_embedding(&Default::default()).map_err(failure)?;
        let registry = Arc::clone(embedding.registry().map_err(failure)?);
        let library = PreparedLibrary::with_helpers(
            Arc::clone(&registry),
            Arc::clone(&lash_vm_releases::helper_release().map_err(failure)?.helpers),
        );
        Ok(Self {
            registry,
            library,
            setup_ns: elapsed(at),
        })
    }
    /// Lower and link a cell outside the measured execution interval.
    ///
    /// # Errors
    /// Refused source, imports, helper release or program preparation.
    pub fn prepare(
        &self,
        source: &str,
        python: bool,
    ) -> Result<(PreparedCell, BTreeMap<String, f64>)> {
        let mut phases = BTreeMap::new();
        let release = lash_vm_releases::helper_release().map_err(failure)?;
        let library =
            NamedLibrary::resolving(&self.registry, |id| release.resolver_roots().contains(id))
                .map_err(|e| failure(format!("{e:?}")))?;
        let namespaces = Namespaces::new(BTreeMap::new(), Default::default(), &|_| false)
            .map_err(|e| failure(format!("{e:?}")))?;
        let environment = Environment::new(&library, &namespaces);
        let mode = Mode::Cell(CellEnvironment::first());
        let document = {
            let _scope = Scope::new("admission_lowering", &mut phases);
            if python {
                lash_dialect_python::lower(source, &environment, mode)
                    .map(|l| Arc::clone(l.document.shared()))
                    .map_err(|e| failure(format!("{e:?}")))?
            } else {
                lash_dialect_typescript::lower(source, &environment, mode)
                    .map(|l| Arc::clone(l.document.shared()))
                    .map_err(|e| failure(format!("{e:?}")))?
            }
        };
        let _scope = Scope::new("linking", &mut phases);
        let link = Link {
            document: document.identity(),
            fills: document
                .imports
                .iter()
                .map(|(slot, import)| {
                    let lash_vocabulary::ImportKind::Operation { signature } = &import.kind else {
                        return Err(failure("unexpected guest import"));
                    };
                    Ok((
                        *slot,
                        Fill::Operation(
                            lash_dialect_python::helper_operations()
                                .iter()
                                .chain(lash_dialect_typescript::helper_operations())
                                .find(|operation| operation.name == import.name)
                                .map(|operation| operation.requirement.clone())
                                .unwrap_or_else(|| lash_vocabulary::OperationRequirement {
                                    signature: signature.clone(),
                                    contract: lash_vocabulary::ContractId::new("compare"),
                                }),
                        ),
                    ))
                })
                .collect::<Result<_>>()?,
        };
        let program = Program::prepare(
            link.identity(),
            BTreeMap::from([(document.identity(), document)]),
            &BTreeMap::from([(link.identity(), link)]),
            PageId([7; 32]),
            self.library.clone(),
            lash_kernel_doc::Selection::Fresh,
        )
        .map_err(failure)?;
        drop(_scope);
        Ok((PreparedCell(program), phases))
    }
}

/// Construct and run a fresh machine; checkpoint only the separate diagnostic run.
/// The interval excludes preparation, machine drop and output validation.
///
/// # Errors
/// Refused construction, checkpoint, execution or checksum output.
pub fn execute(
    program: &PreparedCell,
    checkpoint: bool,
    observe: bool,
) -> Result<(Measurement, BTreeMap<String, f64>)> {
    let mut phases = BTreeMap::new();
    let began = std::time::Instant::now();
    let mut machine = {
        let _scope = Scope::new("machine_construction", &mut phases);
        KernelMachine::start(
            program.0.clone(),
            Bounds {
                charge_limit: 10_000_000_000,
                memory: 512 << 20,
                call_depth: 1000,
                live_tasks: 1024,
                requests_per_park: 1024,
                join_members: 1024,
                memory_meter: lash_kernel_vm::MemoryMeter::On,
                charge: lash_kernel_vm::Charge::Exact,
            },
            CellStart {
                owner: PageId([9; 32]),
                modes: BTreeMap::new(),
                target: Target::Main,
                args: Vec::new(),
                session: None,
                host: Vec::new(),
            },
            Box::<Pages>::default(),
        )
        .map_err(failure)?
    };
    if observe {
        machine.count_work();
        machine.publish_context(lash_kernel_vm::perf_witness::GuestContext::new());
        machine.set_options(lash_kernel_vm::MachineOptions {
            observation: lash_kernel_vm::ObservationMode::Counts,
            trace_path: true,
            ..Default::default()
        });
    }
    // Diagnostic only: the ordinary population never seals before execution.
    if checkpoint {
        let _scope = Scope::new("first_checkpoint", &mut phases);
        machine.checkpoint().map_err(failure)?;
    }
    // The existing diagnostic cell owns counting. Quiet timing cells never
    // enable it; both diagnostic counts and its extra instructions are fixed
    // at the low trip count when compare's high factor changes.
    if checkpoint {
        machine.count_execution();
    }
    let mut console = Console(Vec::new(), observe);
    loop {
        match machine.run(&mut console, u64::MAX).map_err(failure)? {
            Step::Slice => {}
            Step::Ended(End::Finished(_)) => break,
            other => return Err(failure(format!("workload did not finish: {other:?}"))),
        }
    }
    let ns = elapsed(began);
    if observe {
        if machine.work().is_none() || machine.take_counts().is_empty() {
            return Err(failure(
                "observed execution produced no work or site counts",
            ));
        }
        machine.take_path().map_err(failure)?;
    }
    let execution_census = checkpoint.then(|| machine.take_execution_census());
    if console.0.len() != 1 {
        return Err(failure("workload must print exactly one result"));
    }
    let Datum::Text(text) = &console.0[0] else {
        return Err(failure(format!(
            "workload must print JSON text: {:?}",
            console.0
        )));
    };
    let result = serde_json::from_str(text)?;
    Ok((
        Measurement {
            ns,
            result,
            execution_census,
        },
        phases,
    ))
}

// Scope clocks come from the existing perf-witness monotonic clock. These
// diagnostic boundaries never enter the ordinary measurement population.
struct Scope<'a> {
    name: &'static str,
    at: lash_core_execution::perf_witness::latency::Stamp,
    phases: &'a mut BTreeMap<String, f64>,
}
impl<'a> Scope<'a> {
    fn new(name: &'static str, phases: &'a mut BTreeMap<String, f64>) -> Self {
        Self {
            name,
            at: lash_core_execution::perf_witness::latency::Stamp::now(),
            phases,
        }
    }
}
impl Drop for Scope<'_> {
    fn drop(&mut self) {
        self.phases.insert(
            self.name.into(),
            lash_core_execution::perf_witness::latency::Stamp::now().since(self.at) * 1e6,
        );
    }
}
