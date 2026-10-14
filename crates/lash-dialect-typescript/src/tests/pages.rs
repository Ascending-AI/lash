#![allow(
    clippy::unwrap_used,
    reason = "scripted conformance fixture construction uses admitted pages; a refusal is a defect in the fixture"
)]
//! Test-owned page storage; execution and accounting are the real S4/S7 implementations.
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{Datum, Name, Value};
use lash_kernel_state::paged::*;
use lash_kernel_state::paged::{
    census_impl::Census,
    tree::{SealWork, Tree},
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::Arc,
};
#[derive(Clone, Default)]
pub(crate) struct Source {
    pages: Rc<RefCell<BTreeMap<PageId, PageAnswer>>>,
}
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        pages
            .iter()
            .map(|p| {
                self.pages
                    .borrow()
                    .get(&p.id)
                    .cloned()
                    .ok_or(HeapFault::Missing { page: p.id })
            })
            .collect()
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = PageRef {
            id: PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
            grant: PageGrant::NONE,
        };
        self.pages.borrow_mut().insert(
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

pub(crate) const RELEASE: PageId = PageId([7; 32]);
pub(crate) const OWNER: PageId = PageId([9; 32]);
pub(crate) const BUDGET: SealBudget = SealBudget {
    pages: u64::MAX,
    bytes: u64::MAX,
};
#[derive(Clone, Default)]
pub(crate) struct Session {
    pub(crate) root: Option<SessionRoot>,
    pub(crate) source: Source,
}
impl Session {
    pub(crate) fn names(&self) -> BTreeSet<Name> {
        let mut names = BTreeSet::new();
        if let Some(root) = &self.root {
            let heap = self.heap();
            let tree = Tree::<BindingRecord>::open(
                root.bindings.page,
                root.bindings.len,
                root.bindings.cost,
                false,
            );
            tree.visit(&heap.access, &mut |_, binding| {
                names.insert(binding.name.resolve(&heap.access).unwrap());
                true
            })
            .unwrap();
        }
        names
    }
    fn heap(&self) -> PagedHeap {
        match &self.root {
            Some(root) => {
                PagedHeap::open_header(root.heap, Box::new(self.source.clone()), EditEpoch(1))
                    .unwrap()
            }
            None => PagedHeap::fresh(Box::new(self.source.clone()), EditEpoch(1)),
        }
    }
    pub(crate) fn census(&self, bound: u64) -> Box<Census> {
        Box::new(Census::restore(self.heap().certificate(), LogicalBytes(bound)).unwrap())
    }
    pub(crate) fn seeded(values: impl IntoIterator<Item = (Name, Datum)>) -> Self {
        let mut session = Self::default();
        let mut heap = session.heap();
        heap.set_census(session.census(1 << 30)).unwrap();
        let mut table = Tree::new(false);
        for (name, datum) in values {
            let value = decode(&mut heap, datum);
            table
                .write(
                    lash_kernel_state::paged::metadata::metadata_key(&name),
                    BindingRecord {
                        name: NameRef::new(&name, &heap.access).unwrap(),
                        declared: SiteRef::new(
                            &lash_kernel_doc::Site {
                                unit: lash_kernel_doc::Unit::Main,
                                path: Vec::new(),
                            },
                            &heap.access,
                        )
                        .unwrap(),
                        value: BoundRecord::Value(value),
                    },
                    &heap.access,
                )
                .unwrap();
        }
        table
            .seal(&heap.access, &mut SealWork::new(BUDGET))
            .unwrap();
        let bindings = KeyedRoot {
            page: table.page(),
            len: table.len(),
            cost: LogicalBytes(table.cost()),
        };
        heap.set_session_root(table).unwrap();
        session.root = Some(
            heap.carry(CarryRoots {
                bindings,
                functions: KeyedRoot {
                    page: None,
                    len: 0,
                    cost: LogicalBytes(0),
                },
            })
            .unwrap(),
        );
        session
    }
}
pub(crate) fn decode(heap: &mut PagedHeap, datum: Datum) -> Value {
    match datum {
        Datum::Null => Value::Null,
        Datum::Absent => Value::Absent,
        Datum::Bool(v) => Value::Bool(v),
        Datum::Int(v) => Value::Int(heap.int(v).unwrap()),
        Datum::Float(v) => Value::Float(v),
        Datum::Text(v) => Value::Text(heap.text(&v).unwrap()),
        Datum::Bytes(v) => Value::Bytes(heap.bytes(v.as_slice()).unwrap()),
        Datum::Timestamp(v) => Value::Timestamp(v),
        Datum::Function(v) => Value::Function(heap.text(v.as_str()).unwrap()),
        Datum::Handle(v) => Value::Handle(Arc::new(v)),
        Datum::Tuple(v) => {
            let v = v.into_iter().map(|v| decode(heap, v)).collect();
            Value::Tuple(heap.tuple(v).unwrap())
        }
        Datum::List(v) => {
            let v = v.into_iter().map(|v| decode(heap, v)).collect();
            Value::List(heap.allocate(Object::List(v)).unwrap())
        }
        Datum::Set(v) => {
            let v = v.into_iter().map(|v| decode(heap, v)).collect();
            Value::Set(heap.allocate(Object::Set(v)).unwrap())
        }
        Datum::Map(v) => {
            let v = v
                .into_iter()
                .map(|(k, v)| (decode(heap, k), decode(heap, v)))
                .collect();
            Value::Map(heap.allocate(Object::Map(v)).unwrap())
        }
        Datum::Record(v) => {
            let v = v
                .into_iter()
                .map(|(k, v)| (heap.text(&k).unwrap(), decode(heap, v)))
                .collect();
            Value::Record(heap.allocate(Object::Record(v)).unwrap())
        }
        Datum::Error(v) => {
            let mut error = ErrorRef::new(&v.kind, &v.message);
            error.data = Payload::Resident(Arc::new(decode(heap, v.data)));
            heap.canonical_value(Value::Error(Arc::new(error))).unwrap()
        }
        Datum::Number(_) => panic!("corpus inputs have decoded numbers"),
    }
}

pub(crate) fn start(
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    start: lash_kernel_vm::CellStart,
) -> Result<lash_kernel_vm::KernelMachine, lash_kernel_vm::StartError> {
    start_with_layout(program, bounds, start, lash_kernel_vm::Layout::default())
}
pub(crate) fn start_with_layout(
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    start: lash_kernel_vm::CellStart,
    layout: lash_kernel_vm::Layout,
) -> Result<lash_kernel_vm::KernelMachine, lash_kernel_vm::StartError> {
    with_source(program, bounds, start, layout, &Source::default())
}
pub(crate) fn with_source(
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    start: lash_kernel_vm::CellStart,
    layout: lash_kernel_vm::Layout,
    source: &Source,
) -> Result<lash_kernel_vm::KernelMachine, lash_kernel_vm::StartError> {
    let session = Session {
        root: start.session,
        source: source.clone(),
    };
    lash_kernel_vm::KernelMachine::start_with_layout(
        program,
        bounds,
        start,
        layout,
        Box::new(source.clone()),
        session.census(bounds.memory),
    )
}
pub(crate) fn reopen(
    machine: &mut lash_kernel_vm::KernelMachine,
    program: &lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    source: &Source,
    layout: lash_kernel_vm::Layout,
) {
    use lash_kernel_vm::Machine;
    let sealed = machine.seal(BUDGET).unwrap().unwrap();
    let heap =
        PagedHeap::open_header(sealed.root.heap, Box::new(source.clone()), EditEpoch(2)).unwrap();
    let census = Census::restore(heap.certificate(), LogicalBytes(bounds.memory)).unwrap();
    *machine = lash_kernel_vm::KernelMachine::open_with_layout(
        program.clone(),
        bounds,
        &sealed.root,
        Vec::new(),
        Box::new(source.clone()),
        Box::new(census),
        layout,
    )
    .unwrap();
}
