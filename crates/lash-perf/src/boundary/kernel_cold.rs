//! Contract §6: one cold point read and one edit on 200,000-element collections.
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, sync::Arc, time::Instant};

use anyhow::{Context, Result, anyhow, ensure};
use lash_kernel_doc::{Datum, ErrorDatum, heap::*};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageKind, PageSource, SealBudget};
use lash_kernel_value::{OperationUse, package::Fill, package::Link, sites::*};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, HostFault, KernelMachine, Machine, Outcome, PreparedLibrary,
    Program, Request, Step, Target,
};
use serde_json::json;
use sha2::{Digest, Sha256};

include!("kernel_cold_workload.rs");
const ELEMENTS: usize = 200_000;
const INDEX: i64 = 100_003;
const BUDGET: SealBudget = SealBudget {
    pages: u64::MAX,
    bytes: u64::MAX,
};

fn store_node(kind: PageKind) -> bool {
    matches!(
        kind,
        PageKind::SEQUENCE_INDEX
            | PageKind::SEQUENCE_LEAF
            | PageKind::KEYED_INDEX
            | PageKind::KEYED_LEAF
    )
}

#[derive(Default)]
struct Storage {
    pages: BTreeMap<PageId, PageAnswer>,
    reads: u64,
    writes: u64,
    bytes: u64,
    total_reads: u64,
    total_writes: u64,
}
#[derive(Clone, Default)]
struct Pages(Rc<RefCell<Storage>>);
impl Pages {
    fn geometry(&self, map: bool) -> (u64, usize) {
        fn depth(page: &PageAnswer, pages: &BTreeMap<PageId, PageAnswer>, index: PageKind) -> u64 {
            if page.kind != index {
                return 0;
            }
            1 + page
                .children
                .iter()
                .filter_map(|child| pages.get(&child.id))
                .map(|child| depth(child, pages, index))
                .max()
                .unwrap_or(0)
        }
        let storage = self.0.borrow();
        let index = if map {
            PageKind::KEYED_INDEX
        } else {
            PageKind::SEQUENCE_INDEX
        };
        let leaf = if map {
            PageKind::KEYED_LEAF
        } else {
            PageKind::SEQUENCE_LEAF
        };
        let height = storage
            .pages
            .values()
            .map(|page| depth(page, &storage.pages, index))
            .max()
            .unwrap_or(0);
        let leaves = storage
            .pages
            .values()
            .filter(|page| page.kind == leaf)
            .count();
        (height, leaves)
    }
    fn reset(&self) {
        let mut storage = self.0.borrow_mut();
        storage.reads = 0;
        storage.writes = 0;
        storage.bytes = 0;
        storage.total_reads = 0;
        storage.total_writes = 0;
    }
}
impl PageSource for Pages {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        let mut storage = self.0.borrow_mut();
        let answers = pages
            .iter()
            .map(|p| {
                storage
                    .pages
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect::<Vec<_>>();
        storage.total_reads += pages.len() as u64;
        storage.reads += answers
            .iter()
            .filter(|answer| answer.as_ref().is_ok_and(|page| store_node(page.kind)))
            .count() as u64;
        Ok(answers)
    }

    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let encoded = lash_core_store::checkpoint_graph::node::frame_semantic(
            lash_core_store::checkpoint_graph::registry::NodeKind(body.kind.0),
            &body.tokens,
            u64::MAX,
        )
        .map_err(|error| HeapFault::Malformed(error.to_string()))?
        .named()
        .into_node();
        let page = PageRef {
            id: PageId(encoded.id.0),
            grant: PageGrant::NONE,
        };
        let mut storage = self.0.borrow_mut();
        storage.total_writes += 1;
        storage.writes += u64::from(store_node(body.kind));
        storage.bytes += encoded.bytes.len() as u64;
        storage.pages.insert(
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
        _: &OperationUse,
        _: &[Datum],
        _: &PositionLabel,
        _: &Site,
    ) -> Result<Result<Datum, ErrorDatum>, HostFault> {
        Err(HostFault::new("cold workload operations must park"))
    }
    fn print(&mut self, value: &Datum) {
        self.0.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
fn park(machine: &mut KernelMachine, console: &mut Console) -> Result<lash_kernel_vm::WaitId> {
    match machine.run(console, u64::MAX)? {
        Step::Parked(park) if park.requests.len() == 1 => {
            let Request::Effect(request) = &park.requests[0];
            Ok(request.wait)
        }
        other => Err(anyhow!("cold workload did not park: {other:?}")),
    }
}

fn program(map: bool) -> Result<Program> {
    let embedding =
        lash_vm_worker::built_embedding(&Default::default()).map_err(|error| anyhow!("{error}"))?;
    let registry = Arc::clone(embedding.registry().map_err(|error| anyhow!("{error}"))?);
    let name = if map { "map.get" } else { "list.get" };
    let id = lash_kernel_dialect::Library::resolve(embedding.library(), name)
        .ok_or_else(|| anyhow!("missing {name}"))?;
    let native = registry
        .get(&id)
        .ok_or_else(|| anyhow!("missing {name} definition"))?;
    let declaration = format!(
        "native @{id}\ndefinition {}\n",
        serde_json::to_string(&lash_kernel_doc::print_definition(native.definition()))?
    );
    let source =
        SOURCE
            .replace("POINT_GET", name)
            .replacen("main {", &format!("{declaration}main {{"), 1);
    let document = Arc::new(lash_kernel_doc::parse_document(&source)?);
    lash_kernel_check::admit(
        &document,
        &lash_kernel_check::Environment::new(registry.as_ref()),
    )?;
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let lash_kernel_value::ImportKind::Operation { signature } = &import.kind else {
                return Err(anyhow!("unexpected cold workload import"));
            };
            Ok((
                *slot,
                Fill::Operation(lash_kernel_value::OperationRequirement::new(
                    signature.clone(),
                    lash_kernel_value::ContractId::new("cold-gate"),
                )),
            ))
        })
        .collect::<Result<_>>()?;
    let link = Link {
        document: document.identity(),
        fills,
        helper_fills: Default::default(),
    };
    Ok(Program::prepare(
        link.identity(),
        BTreeMap::from([(document.identity(), document)]),
        &BTreeMap::from([(link.identity(), link)]),
        PageId([7; 32]),
        PreparedLibrary::new(registry),
        lash_kernel_doc::Selection::Fresh,
    )?)
}

fn population(program: &Program, map: bool) -> Result<serde_json::Value> {
    let pages = Pages::default();
    let bounds = Bounds {
        charge: lash_kernel_vm::Charge::Exact,
        memory_meter: lash_kernel_vm::MemoryMeter::On,
        charge_limit: 10_000_000_000,
        memory: 512 << 20,
        call_depth: 64,
        live_tasks: 16,
        requests_per_park: 16,
        join_members: 16,
    };
    let mut machine = KernelMachine::start(
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
        Box::new(pages.clone()),
    )?;
    let mut console = Console::default();
    let seed = park(&mut machine, &mut console)?;
    let values = (0..ELEMENTS).map(|i| Datum::Int((i as i64).into()));
    let value = if map {
        Datum::Map(values.map(|v| (v.clone(), v)).collect())
    } else {
        Datum::List(values.collect())
    };
    machine.deliver(seed, Outcome::Completed(value))?;
    let before_read = park(&mut machine, &mut console)?;
    let root = machine
        .seal(BUDGET)?
        .ok_or_else(|| anyhow!("cold seed seal incomplete"))?;
    let (path_depth, cold_leaf_pages) = pages.geometry(map);
    drop(machine);
    pages.reset();
    let mut machine = KernelMachine::open(
        program.clone(),
        bounds,
        &root.checkpoint(),
        Box::new(pages.clone()),
    )?;
    machine.deliver(before_read, Outcome::Completed(Datum::Int(0.into())))?;
    let before_edit = park(&mut machine, &mut console).context("cold point read")?;
    let reads = pages.0.borrow().reads;
    let total_reads = pages.0.borrow().total_reads;
    ensure!(
        console.0 == [Datum::Int(INDEX.into())],
        "cold point read returned the wrong value"
    );
    let root = machine
        .seal(BUDGET)?
        .ok_or_else(|| anyhow!("pre-edit seal incomplete"))?;
    drop(machine);
    pages.reset();
    let mut machine = KernelMachine::open(
        program.clone(),
        bounds,
        &root.checkpoint(),
        Box::new(pages.clone()),
    )?;
    machine.deliver(before_edit, Outcome::Completed(Datum::Int(0.into())))?;
    let after_edit = park(&mut machine, &mut console).context("cold point edit")?;
    let began = Instant::now();
    let root = machine
        .seal(BUDGET)?
        .ok_or_else(|| anyhow!("point edit seal incomplete"))?;
    let seal_ns = began.elapsed().as_nanos();
    let edit_reads = pages.0.borrow().reads;
    let total_edit_reads = pages.0.borrow().total_reads;
    let total_writes = pages.0.borrow().total_writes;
    let writes = pages.0.borrow().writes;
    let bytes_written = pages.0.borrow().bytes;
    drop(machine);
    let mut machine = KernelMachine::open(
        program.clone(),
        bounds,
        &root.checkpoint(),
        Box::new(pages.clone()),
    )?;
    machine.deliver(after_edit, Outcome::Completed(Datum::Int(0.into())))?;
    match machine.run(&mut console, u64::MAX)? {
        Step::Ended(End::Finished(end)) => ensure!(
            end.result == Datum::Int((-1).into()),
            "cold edited result differs"
        ),
        other => return Err(anyhow!("cold edited run did not finish: {other:?}")),
    }
    ensure!(
        console.0 == [Datum::Int(INDEX.into()), Datum::Int((-1).into())],
        "cold edited value differs after reopen"
    );
    Ok(
        json!({"status": "supported", "elements": ELEMENTS, "reads": reads, "total_reads": total_reads, "edit_reads": edit_reads, "total_edit_reads": total_edit_reads, "total_writes": total_writes, "path_depth": path_depth, "cold_leaf_pages": cold_leaf_pages, "writes": writes, "bytes_written": bytes_written, "seal_ns": seal_ns, "results": [INDEX, -1]}),
    )
}

pub(super) fn run(operations: usize, cap: usize) -> Result<super::Receipt> {
    let mut rows = BTreeMap::new();
    for (name, map) in [("list", false), ("map", true)] {
        let program = program(map)?;
        let samples = (0..operations)
            .map(|_| population(&program, map).unwrap_or_else(|error| json!({"status": "unsupported", "elements": ELEMENTS, "error": format!("{error:#}")})))
            .collect::<Vec<_>>();
        rows.insert(name, samples);
    }
    let meter = super::Meter::new(cap);
    Ok(super::Receipt::new(
        super::Case::KernelCold,
        "kernel",
        "in-memory-pages",
        operations,
        &meter,
        json!({"source_sha256": format!("{:x}", Sha256::digest(include_bytes!("kernel_cold_workload.rs"))), "charge": "exact", "memory_meter": "on", "rows": rows}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_gate_counts_store_nodes_and_retains_total_io() -> Result<()> {
        let mut pages = Pages::default();
        let mut tokens = lash_kernel_value::tokens::TokenWriter::new();
        tokens.unsigned(1);
        let leaf = pages.seal(PageBody {
            kind: PageKind::SEQUENCE_LEAF,
            tokens: tokens.into_bytes(),
            children: Vec::new(),
        })?;
        let mut tokens = lash_kernel_value::tokens::TokenWriter::new();
        tokens.child(lash_kernel_value::tokens::ChildRef(leaf.id.0));
        let index = pages.seal(PageBody {
            kind: PageKind::KEYED_INDEX,
            tokens: tokens.into_bytes(),
            children: vec![leaf],
        })?;
        let mut tokens = lash_kernel_value::tokens::TokenWriter::new();
        tokens.unsigned(2);
        let header = pages.seal(PageBody {
            kind: PageKind::CHECKPOINT_ROOT,
            tokens: tokens.into_bytes(),
            children: Vec::new(),
        })?;
        assert_eq!(pages.0.borrow().writes, 2);
        assert_eq!(pages.0.borrow().total_writes, 3);
        let answers = pages.read(&[leaf, index, header])?;
        assert!(answers.into_iter().all(|answer| answer.is_ok()));
        assert_eq!(pages.0.borrow().reads, 2);
        assert_eq!(pages.0.borrow().total_reads, 3);
        Ok(())
    }
}
