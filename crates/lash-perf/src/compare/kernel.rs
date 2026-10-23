//! The Test262 production embedding, with fresh cell machines over private pages.
use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail, ensure};
use lash_kernel_dialect::{CellEnvironment, Environment, Mode, NamedLibrary, Namespaces};
use lash_kernel_doc::heap::{HeapFault, PageGrant, PageId, PageRef};
use lash_kernel_doc::{Datum, ErrorDatum, Handle};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, PreparedLibrary, Program, Step, Target,
};
use lash_vocabulary::package::{Fill, Link};

use super::{Sample, Scope, elapsed};

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
struct Console(Vec<Datum>);
impl Host for Console {
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
    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        Err(ErrorDatum {
            kind: "type_error".into(),
            message: "compare has no projection".into(),
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

pub(super) struct Kernel {
    registry: Arc<lash_kernel_doc::FunctionRegistry>,
    library: PreparedLibrary,
    pub setup_ns: f64,
}
impl Kernel {
    pub fn new() -> Result<Self> {
        let at = std::time::Instant::now();
        let embedding =
            lash_vm_worker::built_embedding(&Default::default()).map_err(|e| anyhow!("{e}"))?;
        let registry = Arc::clone(embedding.registry().map_err(|e| anyhow!("{e}"))?);
        let library = PreparedLibrary::with_helpers(
            Arc::clone(&registry),
            Arc::clone(
                &lash_vm_releases::helper_release()
                    .map_err(|e| anyhow!("{e}"))?
                    .helpers,
            ),
        );
        Ok(Self {
            registry,
            library,
            setup_ns: elapsed(at),
        })
    }
    pub fn prepare(&self, source: &str, python: bool) -> Result<(Program, BTreeMap<String, f64>)> {
        let mut phases = BTreeMap::new();
        let release = lash_vm_releases::helper_release().map_err(|e| anyhow!("{e}"))?;
        let library =
            NamedLibrary::resolving(&self.registry, |id| release.resolver_roots().contains(id))
                .map_err(|e| anyhow!("{e:?}"))?;
        let namespaces = Namespaces::new(BTreeMap::new(), Default::default(), &|_| false)
            .map_err(|e| anyhow!("{e:?}"))?;
        let environment = Environment::new(&library, &namespaces);
        let mode = Mode::Cell(CellEnvironment::first());
        let document = {
            let _scope = Scope::new("admission_lowering", &mut phases);
            if python {
                lash_dialect_python::lower(source, &environment, mode)
                    .map(|l| Arc::clone(l.document.shared()))
                    .map_err(|e| anyhow!("{e:?}"))?
            } else {
                lash_dialect_typescript::lower(source, &environment, mode)
                    .map(|l| Arc::clone(l.document.shared()))
                    .map_err(|e| anyhow!("{e:?}"))?
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
                        bail!("unexpected guest import")
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
        .map_err(|e| anyhow!("{e}"))?;
        drop(_scope);
        Ok((program, phases))
    }
}

pub(super) fn execute(
    program: &Program,
    checkpoint: bool,
) -> Result<(Sample, BTreeMap<String, f64>)> {
    let mut phases = BTreeMap::new();
    let began = std::time::Instant::now();
    let mut machine = {
        let _scope = Scope::new("machine_construction", &mut phases);
        KernelMachine::start(
            program.clone(),
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
        .map_err(|e| anyhow!("{e}"))?
    };
    // Diagnostic only: the ordinary population never seals before execution.
    if checkpoint {
        let _scope = Scope::new("first_checkpoint", &mut phases);
        machine.checkpoint().map_err(|e| anyhow!("{e}"))?;
    }
    let mut console = Console::default();
    loop {
        match machine
            .run(&mut console, u64::MAX)
            .map_err(|e| anyhow!("{e}"))?
        {
            Step::Slice => {}
            Step::Ended(End::Finished(_)) => break,
            other => bail!("workload did not finish: {other:?}"),
        }
    }
    let ns = elapsed(began);
    ensure!(
        console.0.len() == 1,
        "workload must print exactly one result"
    );
    let Datum::Text(text) = &console.0[0] else {
        bail!("workload must print JSON text: {:?}", console.0)
    };
    let result = serde_json::from_str(text)?;
    Ok((Sample { ns, result }, phases))
}
