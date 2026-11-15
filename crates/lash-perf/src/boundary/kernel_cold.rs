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
    #[cfg(test)]
    read_pages: Vec<(PageKind, PageId)>,
}
/// What a sealed container occupies: its chunks under their chunk index,
/// and the object directory holding its row.
#[derive(Debug, PartialEq, Eq)]
struct Geometry {
    chunk_pages: usize,
    index_pages: usize,
    index_depth: u64,
    directory_depth: u64,
}
#[derive(Clone, Default)]
struct Pages(Rc<RefCell<Storage>>);
impl Pages {
    /// A list and a map are both sealed as SEQUENCE_LEAF chunks under
    /// SEQUENCE_INDEX pages, with the container row in the KEYED directory.
    /// The chunk-index depth counts index levels; the directory depth d
    /// counts KEYED_INDEX levels and the leaf (contract section 6).
    fn geometry(&self) -> Geometry {
        fn depth(
            page: &PageAnswer,
            pages: &BTreeMap<PageId, PageAnswer>,
            index: PageKind,
            leaf: PageKind,
        ) -> u64 {
            if page.kind == leaf {
                return u64::from(leaf == PageKind::KEYED_LEAF);
            }
            if page.kind != index {
                return 0;
            }
            1 + page
                .children
                .iter()
                .filter_map(|child| pages.get(&child.id))
                .map(|child| depth(child, pages, index, leaf))
                .max()
                .unwrap_or(0)
        }
        let storage = self.0.borrow();
        let height = |index, leaf| {
            storage
                .pages
                .values()
                .map(|page| depth(page, &storage.pages, index, leaf))
                .max()
                .unwrap_or(0)
        };
        let count = |kind| {
            storage
                .pages
                .values()
                .filter(|page| page.kind == kind)
                .count()
        };
        Geometry {
            chunk_pages: count(PageKind::SEQUENCE_LEAF),
            index_pages: count(PageKind::SEQUENCE_INDEX),
            index_depth: height(PageKind::SEQUENCE_INDEX, PageKind::SEQUENCE_LEAF),
            directory_depth: height(PageKind::KEYED_INDEX, PageKind::KEYED_LEAF),
        }
    }
    fn reset(&self) {
        let mut storage = self.0.borrow_mut();
        storage.reads = 0;
        storage.writes = 0;
        storage.bytes = 0;
        storage.total_reads = 0;
        storage.total_writes = 0;
        #[cfg(test)]
        storage.read_pages.clear();
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
        #[cfg(test)]
        storage.read_pages.extend(
            answers
                .iter()
                .filter_map(|answer| answer.as_ref().ok())
                .map(|page| (page.kind, page.page.id)),
        );
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
    let geometry = pages.geometry();
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
        json!({"status": "supported", "elements": ELEMENTS, "reads": reads, "total_reads": total_reads, "edit_reads": edit_reads, "total_edit_reads": total_edit_reads, "total_writes": total_writes, "path_depth": geometry.index_depth, "cold_leaf_pages": geometry.chunk_pages, "index_pages": geometry.index_pages, "directory_depth": geometry.directory_depth, "writes": writes, "bytes_written": bytes_written, "seal_ns": seal_ns, "results": [INDEX, -1]}),
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

    fn seal(
        pages: &mut Pages,
        kind: PageKind,
        tag: u64,
        children: Vec<PageRef>,
    ) -> Result<PageRef> {
        let mut tokens = lash_kernel_value::tokens::TokenWriter::new();
        tokens.unsigned(tag);
        for child in &children {
            tokens.child(lash_kernel_value::tokens::ChildRef(child.id.0));
        }
        Ok(pages.seal(PageBody {
            kind,
            tokens: tokens.into_bytes(),
            children,
        })?)
    }

    /// A paged map is stored as a list is: chunks under a chunk index, its
    /// row in a keyed directory. Geometry describes those pages, not only
    /// the directory.
    #[test]
    fn geometry_describes_a_paged_map_by_its_chunks_index_and_directory() -> Result<()> {
        let mut pages = Pages::default();
        let chunks = vec![
            seal(&mut pages, PageKind::SEQUENCE_LEAF, 1, Vec::new())?,
            seal(&mut pages, PageKind::SEQUENCE_LEAF, 2, Vec::new())?,
        ];
        let index = seal(&mut pages, PageKind::SEQUENCE_INDEX, 3, chunks)?;
        seal(&mut pages, PageKind::KEYED_LEAF, 4, vec![index])?;
        let expected = Geometry {
            chunk_pages: 2,
            index_pages: 1,
            index_depth: 1,
            directory_depth: 1,
        };
        assert_eq!(pages.geometry(), expected);
        Ok(())
    }
}

#[cfg(test)]
mod point_reads {
    use super::*;
    use lash_kernel_dialect::{
        CellEnvironment, Environment, Mode, NamespaceKind, NamespaceMember, Namespaces,
        OperationSignature,
    };
    use lash_kernel_value::{Signature, Type};

    fn authored(dialect: &str, operation: &str) -> Result<Program> {
        let embedding =
            lash_vm_worker::built_embedding(&Default::default()).map_err(|e| anyhow!("{e}"))?;
        let signature = Signature {
            params: Vec::new(),
            result: Type::Any,
        };
        let seed = lash_kernel_value::QualifiedOperation::from_effect(
            &lash_kernel_value::EffectName::new("host.seed")?,
        )?;
        let pause = lash_kernel_value::QualifiedOperation::from_effect(
            &lash_kernel_value::EffectName::new("host.pause")?,
        )?;
        let packages = BTreeMap::from([(
            seed.package,
            BTreeMap::from([
                (
                    seed.name,
                    NamespaceMember {
                        visibility: lash_kernel_value::Visibility::Public,
                        kind: NamespaceKind::Operation(OperationSignature {
                            signature: signature.clone(),
                            controls: Default::default(),
                            mode: lash_kernel_value::OperationMode::Waited,
                        }),
                    },
                ),
                (
                    pause.name,
                    NamespaceMember {
                        visibility: lash_kernel_value::Visibility::Public,
                        kind: NamespaceKind::Operation(OperationSignature {
                            signature,
                            controls: Default::default(),
                            mode: lash_kernel_value::OperationMode::Waited,
                        }),
                    },
                ),
            ]),
        )]);
        let namespaces = Namespaces::new(packages, Default::default(), &|_| false)?;
        let environment = Environment::new(embedding.library(), &namespaces);
        let source = if dialect == "python" {
            format!("xs = await host.seed()\nawait host.pause()\n{operation}\nawait host.pause()\n")
        } else {
            format!(
                "let xs = await host.seed();\nawait host.pause();\n{operation}\nawait host.pause();\n"
            )
        };
        let document = Arc::new(
            embedding
                .lower(
                    dialect,
                    &source,
                    &environment,
                    Mode::Cell(CellEnvironment::first()),
                )
                .ok_or_else(|| anyhow!("missing dialect {dialect}"))?
                .map_err(|e| anyhow!("{e}"))?
                .document
                .into_document(),
        );
        let library = embedding
            .prepared_library()
            .map_err(|e| anyhow!("{e}"))?
            .clone();
        prepare(document, library)
    }

    fn prepare(
        document: Arc<lash_kernel_doc::Document>,
        library: PreparedLibrary,
    ) -> Result<Program> {
        let fills = document
            .imports
            .iter()
            .map(|(slot, import)| {
                let lash_kernel_value::ImportKind::Operation { signature } = &import.kind else {
                    return Err(anyhow!("unexpected import"));
                };
                Ok((
                    *slot,
                    Fill::Operation(lash_kernel_value::OperationRequirement::new(
                        signature.clone(),
                        lash_kernel_value::ContractId::new("cold-point-law"),
                    )),
                ))
            })
            .collect::<Result<_>>()?;
        let helper_fills = lash_kernel_doc::helper_imports(&document, library.helpers().as_ref())?
            .into_iter()
            .map(|(id, import)| {
                let lash_kernel_value::ImportKind::Operation { signature } = &import.kind else {
                    return Err(anyhow!("unexpected helper import"));
                };
                let fill = Fill::Operation(lash_kernel_value::OperationRequirement::new(
                    signature.clone(),
                    lash_kernel_value::ContractId::new("cold-point-law"),
                ));
                Ok((id, lash_kernel_value::HelperFill { import, fill }))
            })
            .collect::<Result<_>>()?;
        let link = Link {
            document: document.identity(),
            fills,
            helper_fills,
        };
        Ok(Program::prepare(
            link.identity(),
            BTreeMap::from([(document.identity(), document)]),
            &BTreeMap::from([(link.identity(), link)]),
            PageId([7; 32]),
            library,
        )?)
    }

    fn native(operation: &str) -> Result<Program> {
        native_program(operation, false)
    }
    fn native_program(operation: &str, session: bool) -> Result<Program> {
        let embedding =
            lash_vm_worker::built_embedding(&Default::default()).map_err(|e| anyhow!("{e}"))?;
        let registry = Arc::clone(embedding.registry().map_err(|e| anyhow!("{e}"))?);
        let mut source = String::from(
            "kernel 1\nnumbers by_spelling\nimport 0 operation host.seed() -> Any\nimport 1 operation host.pause() -> Any\n",
        );
        for name in [
            "map.get",
            "record.get",
            "map.contains",
            "set.contains",
            "record.contains",
            "list.at",
            "tuple.at",
        ] {
            let id = lash_kernel_dialect::Library::resolve(embedding.library(), name)
                .ok_or_else(|| anyhow!("missing {name}"))?;
            let function = registry.get(&id).ok_or_else(|| anyhow!("missing native"))?;
            source.push_str(&format!(
                "native @{id}\ndefinition {}\n",
                serde_json::to_string(&lash_kernel_doc::print_definition(function.definition()))?
            ));
        }
        let seed = if session {
            ""
        } else {
            "let xs = perform 0() as Any"
        };
        source.push_str(&format!("main {{ {seed} let pause = perform 1() as Any {operation} let done = perform 1() as Any return null }}"));
        let document = Arc::new(lash_kernel_doc::parse_document(&source)?);
        prepare(document, PreparedLibrary::new(registry))
    }

    #[derive(Clone, Copy)]
    enum Fixture {
        Map,
        Set,
        Record,
        List,
        Tuple,
    }
    fn datum(kind: Fixture) -> Datum {
        let values = (0..ELEMENTS).map(|i| Datum::Int((i as i64).into()));
        match kind {
            Fixture::Map => Datum::Map(values.map(|v| (v.clone(), v)).collect()),
            Fixture::Set => Datum::Set(values.collect()),
            Fixture::Record => Datum::Record(
                (0..ELEMENTS)
                    .map(|i| (format!("k{i}"), Datum::Int((i as i64).into())))
                    .collect(),
            ),
            Fixture::List => Datum::List(values.collect()),
            Fixture::Tuple => Datum::Tuple(values.collect()),
        }
    }
    fn bounds() -> Bounds {
        Bounds {
            charge: lash_kernel_vm::Charge::Exact,
            memory_meter: lash_kernel_vm::MemoryMeter::On,
            charge_limit: u64::MAX,
            memory: 512 << 20,
            call_depth: 100,
            live_tasks: 16,
            requests_per_park: 16,
            join_members: 16,
        }
    }
    fn seeded(
        program: &Program,
        kind: Fixture,
    ) -> Result<(KernelMachine, Pages, Console, lash_kernel_vm::WaitId)> {
        let pages = Pages::default();
        let mut machine = KernelMachine::start(
            program.clone(),
            bounds(),
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
        machine.deliver(seed, Outcome::Completed(datum(kind)))?;
        let pause = park(&mut machine, &mut console)?;
        Ok((machine, pages, console, pause))
    }
    fn measure(program: &Program, kind: Fixture) -> Result<(u64, u64, Vec<Datum>)> {
        let (mut machine, pages, mut console, pause) = seeded(program, kind)?;
        let root = machine
            .seal(BUDGET)?
            .ok_or_else(|| anyhow!("seed seal incomplete"))?;
        // The paired run starts at the same park and uses the same source program.
        machine.deliver(pause, Outcome::Completed(Datum::Null))?;
        park(&mut machine, &mut console)?;
        let warm = console.0.clone();
        let charged = machine.meters().charged;
        drop(machine);
        pages.reset();
        let mut machine = KernelMachine::open(
            program.clone(),
            bounds(),
            &root.checkpoint(),
            Box::new(pages.clone()),
        )?;
        machine.deliver(pause, Outcome::Completed(Datum::Null))?;
        console.0.clear();
        park(&mut machine, &mut console)?;
        ensure!(
            console.0 == warm,
            "warm/cold results differ: {:?} / {:?}",
            warm,
            console.0
        );
        ensure!(
            machine.meters().charged == charged,
            "warm/cold charges differ"
        );
        let reads = pages.0.borrow().reads;
        Ok((
            reads,
            charged.ok_or_else(|| anyhow!("charge meter off"))?,
            warm,
        ))
    }

    // This is a measurement command for the lane inventory, not an acceptance test.
    #[test]
    #[ignore = "explicit cold point-operation inventory"]
    fn cold_point_operation_inventory() -> Result<()> {
        use Fixture::*;
        let cases = [
            ("python", Map, "print(xs.get(100003))"),
            ("python", Map, "print(xs.get(200003))"),
            ("python", Map, "print(xs.get(200003, -1))"),
            ("python", Map, "print(xs[100003])"),
            ("python", Map, "print(100003 in xs)"),
            ("python", Map, "xs[100003] = -1"),
            ("python", Map, "del xs[100003]"),
            ("python", Map, "print(xs.pop(100003))"),
            ("python", Map, "print(xs.setdefault(100003, -1))"),
            ("python", Map, "print(xs.setdefault(200003, -1))"),
            ("python", Set, "print(100003 in xs)"),
            ("python", Set, "xs.add(100003)"),
            ("python", Set, "xs.remove(100003)"),
            ("python", Set, "xs.discard(100003)"),
            ("python", Record, "print(xs.k100003)"),
            ("python", Record, "print(xs['k100003'])"),
            ("python", Record, "print(xs.get('k100003'))"),
            ("python", Record, "print('k100003' in xs)"),
            ("python", List, "print(xs[100003])"),
            ("python", List, "xs[100003] = -1"),
            ("python", List, "print(xs.pop())"),
            ("python", Tuple, "print(xs[100003])"),
            ("typescript", Map, "console.log(xs.get(100003));"),
            ("typescript", Map, "console.log(xs.get(200003));"),
            ("typescript", Map, "console.log(xs.has(100003));"),
            ("typescript", Map, "xs.set(100003, -1);"),
            ("typescript", Map, "console.log(xs.delete(100003));"),
            ("typescript", Set, "console.log(xs.has(100003));"),
            ("typescript", Set, "xs.add(100003);"),
            ("typescript", Set, "console.log(xs.delete(100003));"),
            ("typescript", Record, "console.log(xs.k100003);"),
            ("typescript", Record, "console.log(xs['k100003']);"),
            ("typescript", Record, "console.log('k100003' in xs);"),
            ("typescript", Record, "xs.k100003 = -1;"),
            ("typescript", Record, "xs['k100003'] = -1;"),
            ("typescript", Record, "console.log(delete xs.k100003);"),
            ("typescript", List, "console.log(xs[100003]);"),
            ("typescript", List, "xs[100003] = -1;"),
            ("typescript", List, "console.log(xs.pop());"),
            ("typescript", Tuple, "console.log(xs[100003]);"),
        ];
        for (dialect, kind, operation) in cases {
            match authored(dialect, operation).and_then(|p| measure(&p, kind)) {
                Ok((reads, charge, _)) => {
                    println!("POINT | {dialect} | {operation} | {reads} | {charge}")
                }
                Err(e) => println!("POINT | {dialect} | {operation} | ERROR {e:#}"),
            }
        }
        for (kind, operation) in [
            (Map, "print map.get(xs, 100003)"),
            (Record, "print record.get(xs, \"k100003\")"),
            (Map, "print map.contains(xs, 100003)"),
            (Set, "print set.contains(xs, 100003)"),
            (Record, "print record.contains(xs, \"k100003\")"),
            (List, "print list.at(xs, 100003)"),
            (Tuple, "print tuple.at(xs, 100003)"),
        ] {
            match measure(&native(operation)?, kind) {
                Ok((reads, charge, _)) => {
                    println!("POINT | kernel | {operation} | {reads} | {charge}")
                }
                Err(e) => println!("POINT | kernel | {operation} | ERROR {e:#}"),
            }
        }
        Ok(())
    }
    fn get_law(key: i64, default: bool) -> Result<()> {
        // Contract §6: d is the directory depth, not the chunk-index height.
        // These integer-only maps have one directory leaf, so d = 1.
        let d = 1;
        let py = if default {
            format!("print(xs.get({key}, -1))")
        } else {
            format!("print(xs.get({key}))")
        };
        let ts = if default {
            format!("console.log(xs.get({key}) ?? -1);")
        } else {
            format!("console.log(xs.get({key}));")
        };
        let mut violations = Vec::new();
        for (dialect, source) in [("python", py), ("typescript", ts)] {
            let (reads, charge, result) = measure(&authored(dialect, &source)?, Fixture::Map)?;
            let expected = Datum::Text(if key < ELEMENTS as i64 {
                key.to_string()
            } else if default {
                "-1".into()
            } else if dialect == "python" {
                "None".into()
            } else {
                "undefined".into()
            });
            ensure!(
                result == [expected],
                "wrong {dialect} get result: {result:?}"
            );
            println!("{dialect}: {source}: {reads} pages, {charge} charge units");
            if reads > d + 2 {
                violations.push(format!("{dialect}: {reads} > d + 2 = {}", d + 2));
            }
        }
        // The kernel native is also a public keyed read, with a strict
        // missing-key error rather than either dialect's default result.
        if !default {
            let operation =
                format!("try {{ print map.get(xs, {key}) }} catch e {{ print \"missing\" }}");
            let (reads, charge, result) = measure(&native(&operation)?, Fixture::Map)?;
            let expected = if key < ELEMENTS as i64 {
                Datum::Int(key.into())
            } else {
                Datum::Text("missing".into())
            };
            ensure!(result == [expected], "wrong native get result: {result:?}");
            println!("kernel: {reads} pages, {charge} charge units");
            if reads > d + 2 {
                violations.push(format!("kernel: {reads} > d + 2 = {}", d + 2));
            }
        }
        ensure!(violations.is_empty(), "{}", violations.join("; "));
        Ok(())
    }
    #[test]
    fn a_cold_present_map_get_loads_one_chunk_with_the_warm_result_and_charge() -> Result<()> {
        get_law(INDEX, false)
    }
    #[test]
    fn a_cold_absent_map_get_loads_one_chunk_with_the_warm_result_and_charge() -> Result<()> {
        get_law(200_003, false)
    }
    #[test]
    fn a_cold_defaulted_map_get_loads_one_chunk_with_the_warm_result_and_charge() -> Result<()> {
        get_law(200_003, true)
    }

    fn refused_chunk(operation: &str) -> Result<(lash_kernel_vm::Bound, Vec<PageId>)> {
        let program = native_program(operation, true)?;
        let seed_program = native("")?;
        let (mut seed, pages, mut console, first) = seeded(&seed_program, Fixture::Map)?;
        seed.deliver(first, Outcome::Completed(Datum::Null))?;
        let second = park(&mut seed, &mut console)?;
        seed.deliver(second, Outcome::Completed(Datum::Null))?;
        let Step::Ended(End::Finished(finished)) = seed.run(&mut console, u64::MAX)? else {
            return Err(anyhow!("seed cell did not finish"));
        };
        let session = finished
            .carry
            .ok_or_else(|| anyhow!("seed cell has no carry"))?;
        drop(seed);
        // Each bound is a fresh cold run. Reopening a checkpoint preserves
        // its stored bounds, so changing open()'s fallback would not test
        // a different memory bound.
        for memory in (128..=512_000).step_by(128) {
            let Ok(mut machine) = KernelMachine::start(
                program.clone(),
                Bounds { memory, ..bounds() },
                CellStart {
                    target: Target::Main,
                    args: Vec::new(),
                    session: Some(session),
                    owner: PageId([9; 32]),
                    modes: Default::default(),
                    host: Vec::new(),
                },
                Box::new(pages.clone()),
            ) else {
                continue;
            };
            let mut before = Console::default();
            let Ok(pause) = park(&mut machine, &mut before) else {
                continue;
            };
            pages.reset();
            machine.deliver(pause, Outcome::Completed(Datum::Null))?;
            let mut console = Console::default();
            let step = machine.run(&mut console, u64::MAX)?;
            let leaves = pages
                .0
                .borrow()
                .read_pages
                .iter()
                .filter(|(kind, _)| *kind == PageKind::SEQUENCE_LEAF)
                .map(|(_, id)| *id)
                .collect::<Vec<_>>();
            if let Step::Ended(End::Error(lash_kernel_vm::RunError::Bound(exceeded))) = step
                && exceeded.bound == lash_kernel_vm::Bound::Memory
                && !leaves.is_empty()
            {
                ensure!(console.0.is_empty(), "refused get printed a result");
                println!(
                    "{operation}: chunk refused at memory={memory}, leaves={}",
                    leaves.len()
                );
                return Ok((exceeded.bound, leaves));
            }
        }
        Err(anyhow!(
            "no chunk-load memory refusal for {operation}; last read kinds: {:?}",
            pages
                .0
                .borrow()
                .read_pages
                .iter()
                .map(|(kind, _)| kind)
                .collect::<Vec<_>>()
        ))
    }
    #[test]
    fn a_cold_get_refuses_the_same_chunk_load_as_a_subscript_at_the_memory_bound() -> Result<()> {
        let subscript = refused_chunk("print xs[100003]")?;
        let get = refused_chunk("print map.get(xs, 100003)")?;
        ensure!(
            get == subscript,
            "get refused a different chunk load: {get:?} / {subscript:?}"
        );
        Ok(())
    }
}
