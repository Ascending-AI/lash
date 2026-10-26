//! One-cell allocation, capacity-ledger, RSS and native-stack instrument.
#![expect(
    clippy::expect_used,
    clippy::disallowed_methods,
    reason = "explicit diagnostic executable arguments and receipts"
)]
use lash_kernel_dialect::{EffectControl, Environment, NamedLibrary};
use lash_kernel_doc::{Datum, ErrorDatum};
use lash_kernel_value::names::EffectName;
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, PreparedLibrary, Step, Target,
};
use lash_vm_worker::cell_memory;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

#[global_allocator]
static ALLOCATOR: cell_memory::Alloc<std::alloc::System> = cell_memory::Alloc(std::alloc::System);
#[path = "cell_mem/worker.rs"]
mod cell_mem_worker;
#[path = "../../lash-dialect-typescript/shared/gate_six.rs"]
#[expect(dead_code, reason = "shared tool signatures and scripted answers")]
mod gate_six;
#[path = "../../lash-dialect-typescript/src/tests/linked.rs"]
mod linked;
#[path = "../../lash-dialect-typescript/src/tests/linked_reads.rs"]
mod linked_reads;
#[path = "../../lash-dialect-typescript/src/tests/pages.rs"]
#[expect(dead_code, reason = "shared in-memory page source")]
mod pages;

struct Console;
impl Host for Console {
    fn print(&mut self, _: &Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
    fn perform(
        &mut self,
        use_: &lash_kernel_value::OperationUse,
        _: &[Datum],
        _: &lash_kernel_value::sites::PositionLabel,
        _: &lash_kernel_value::sites::Site,
    ) -> Result<Result<Datum, ErrorDatum>, lash_kernel_vm::HostFault> {
        linked_reads::read(use_, 0)
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if lash_vm_worker::worker_entry(&lash_vm_worker::built_dialects)? {
        return Ok(());
    }
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let file = args
        .first()
        .ok_or("cell_mem FILE OUT [--off] [--worker] [--stack]")?;
    let out = PathBuf::from(args.get(1).ok_or("missing output directory")?);
    std::fs::create_dir_all(&out)?;
    let active = !args.iter().any(|s| s == "--off");
    let stack = active && args.iter().any(|s| s == "--stack");
    let worker = args.iter().any(|s| s == "--worker");
    let cells = args
        .iter()
        .position(|arg| arg == "--cells")
        .map(|at| {
            args.get(at + 1)
                .ok_or("missing --cells count")?
                .parse::<u64>()
                .map_err(|_| "invalid --cells count")
        })
        .transpose()?
        .unwrap_or(1);
    if cells == 0 || (!worker && cells != 1) {
        return Err("--cells must be positive and requires --worker".into());
    }
    let embedding = lash_vm_worker::built_embedding(&Default::default())?;
    let registry = Arc::clone(embedding.registry()?);
    let named = NamedLibrary::resolving(&registry, |id| {
        lash_kernel_releases::helper_release()
            .expect("release")
            .resolver_roots()
            .contains(id)
    })?;
    let controls = BTreeMap::from([(
        EffectName::new("perf.finish")?,
        BTreeSet::from([EffectControl::Finish]),
    )]);
    let namespaces = linked::namespaces(&gate_six::effects(), &controls)?;
    let environment = Environment::new(&named, &namespaces);
    let source = std::fs::read_to_string(file)?;
    let mode = lash_kernel_dialect::Mode::Cell(lash_kernel_dialect::CellEnvironment::first());
    let document = if file.ends_with(".py") {
        lash_dialect_python::lower(&source, &environment, mode)
            .map_err(|e| format!("{e:?}"))?
            .document
    } else if file.ends_with(".ts") {
        lash_dialect_typescript::lower(&source, &environment, mode)
            .map_err(|e| format!("{e:?}"))?
            .document
    } else {
        return Err("expected .ts or .py".into());
    };
    let program = linked::program(
        Arc::clone(document.shared()),
        PreparedLibrary::new(registry)
            .with_helper_handle(Arc::clone(&lash_kernel_releases::helper_release()?.helpers)),
    );
    let bounds = Bounds {
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge: lash_kernel_vm::Charge::Exact,
        charge_limit: u64::MAX,
        memory: 1 << 30,
        call_depth: 10_000,
        live_tasks: 1024,
        requests_per_park: 256,
        join_members: 1024,
    };
    if worker {
        return cell_mem_worker::run(&program, file, &out, active, stack, bounds, cells);
    }
    let mut machine = KernelMachine::start(
        program.clone(),
        bounds,
        CellStart {
            owner: pages::OWNER,
            target: Target::Main,
            args: Vec::new(),
            session: None,
            modes: pages::modes(&program),
            host: Vec::new(),
        },
        Box::new(pages::Source::default()),
    )?;
    if stack {
        cell_memory::stack_start()?;
    }
    if active {
        cell_memory::begin();
        cell_memory::attach(&mut machine);
    }
    let before = cell_memory::requests();
    let mut result = Datum::Null;
    loop {
        match machine.run(&mut Console, u64::MAX)? {
            Step::Parked(park) => {
                for request in park.requests {
                    let lash_kernel_vm::Request::Effect(request) = request;
                    let name = linked::operation(&request.use_);
                    let argument = request.args.into_iter().next().unwrap_or(Datum::Null);
                    if name.as_str() == "perf.finish" {
                        result = argument.clone();
                    }
                    let outcome = gate_six::answer(name.as_str(), argument);
                    machine.deliver(request.wait, outcome)?;
                }
            }
            Step::Slice => continue,
            Step::Ended(End::Finished(_)) => break,
            Step::Ended(other) => return Err(format!("cell failed: {other:?}").into()),
        }
    }
    let after = cell_memory::requests();
    let report = if active {
        cell_memory::finish(&machine)
    } else {
        None
    };
    if let Some(report) = report {
        std::fs::write(out.join("cell-memory.json"), serde_json::to_vec(&report)?)?;
    }
    println!(
        "{}",
        json!({"result":format!("{result:?}"),"charged":machine.meters().charged,"observer":active,"worker":false,"requests":[after[0]-before[0],after[1]-before[1]]})
    );
    Ok(())
}
