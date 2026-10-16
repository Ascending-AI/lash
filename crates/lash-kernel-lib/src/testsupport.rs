//! Layer laws use the actual paged heap and allocation census.
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{Datum, ErrorDatum, Integer, Name, Value};
use lash_kernel_state::paged::census_impl::Census;
use lash_kernel_state::paged::*;
use num_bigint::{BigInt, Sign};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Clone, Default)]
pub(crate) struct Source {
    pages: Rc<RefCell<BTreeMap<PageId, PageAnswer>>>,
    fault: Rc<RefCell<Option<HeapFault>>>,
    reads: Rc<std::cell::Cell<usize>>,
}
impl Source {
    /// The pages read through this source so far.
    pub(crate) fn pages_read(&self) -> usize {
        self.reads.get()
    }
    pub(crate) fn fail_reads(&self, fault: HeapFault) {
        *self.fault.borrow_mut() = Some(fault);
    }
}
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<PageAnswer>, HeapFault> {
        if let Some(fault) = self.fault.borrow().clone() {
            return Err(fault);
        }
        self.reads.set(self.reads.get() + pages.len());
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
pub(crate) fn heap() -> PagedHeap {
    heap_with_source().0
}
pub(crate) fn heap_with_source() -> (PagedHeap, Source) {
    let source = Source::default();
    let mut heap = PagedHeap::fresh(Box::new(source.clone()), EditEpoch(1));
    heap.set_census(Box::new(
        Census::restore(heap.certificate(), LogicalBytes(1 << 30)).unwrap(),
    ))
    .unwrap();
    (heap, source)
}
pub(crate) fn hold_with_extra(heap: &mut PagedHeap, args: &[Value], extra: u64) {
    heap.set_execution_roots(
        tree::Tree::new(false),
        tree::Tree::new(false),
        args,
        &[],
        &[],
    )
    .unwrap();
    let certificate = heap.certificate();
    heap.set_census(Box::new(
        Census::restore(certificate, LogicalBytes(certificate.upper.0 + extra)).unwrap(),
    ))
    .unwrap();
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
        Datum::FunctionValue(_) => panic!("a function value never enters a run"),
    }
}
pub(crate) fn text(heap: &dyn NativeHeap, text: &TextRef) -> String {
    let mut out = String::new();
    heap.payload_visit(
        &Value::Text(text.clone()),
        0..text.summary.utf8_bytes,
        &mut |chunk| {
            let Chunk::Text(chunk) = chunk else {
                panic!("text chunk")
            };
            out.push_str(chunk);
            ControlFlow::Continue(())
        },
    )
    .unwrap();
    out
}
pub(crate) fn integer(heap: &dyn NativeHeap, value: &IntRef) -> Integer {
    match value {
        IntRef::Inline(value) => value.clone(),
        IntRef::Paged { summary, .. } => {
            let mut digits = Vec::new();
            heap.payload_visit(
                &Value::Int(value.clone()),
                0..summary.bits.div_ceil(8),
                &mut |chunk| {
                    let Chunk::Bytes(chunk) = chunk else {
                        panic!("integer chunk")
                    };
                    digits.extend_from_slice(chunk);
                    ControlFlow::Continue(())
                },
            )
            .unwrap();
            Integer::new(BigInt::from_bytes_le(
                if summary.negative {
                    Sign::Minus
                } else {
                    Sign::Plus
                },
                &digits,
            ))
        }
    }
}
pub(crate) fn datum(heap: &dyn NativeHeap, value: &Value) -> Datum {
    match value {
        Value::Null => Datum::Null,
        Value::Absent => Datum::Absent,
        Value::Bool(v) => Datum::Bool(*v),
        Value::Int(v) => Datum::Int(integer(heap, v)),
        Value::Float(v) => Datum::Float(*v),
        Value::Text(v) => Datum::Text(text(heap, v)),
        Value::Function(v) => Datum::Function(Name::new(text(heap, v))),
        Value::Bytes(v) => {
            let mut out = Vec::new();
            heap.payload_visit(value, 0..v.len, &mut |chunk| {
                let Chunk::Bytes(chunk) = chunk else {
                    panic!("bytes chunk")
                };
                out.extend_from_slice(chunk);
                ControlFlow::Continue(())
            })
            .unwrap();
            Datum::Bytes(lash_kernel_doc::Bytes::new(out))
        }
        Value::Timestamp(v) => Datum::Timestamp(v.clone()),
        Value::Handle(v) => Datum::Handle((**v).clone()),
        Value::Tuple(v) => {
            let mut out = Vec::new();
            heap.tuple_visit(v, &mut |v| {
                out.push(datum(heap, v));
                ControlFlow::Continue(())
            })
            .unwrap();
            Datum::Tuple(out)
        }
        Value::Error(v) => Datum::Error(Box::new(ErrorDatum {
            kind: text(heap, &v.kind),
            message: text(heap, &v.message),
            data: datum(heap, &heap.error_data(v).unwrap()),
        })),
        Value::List(id) | Value::Set(id) | Value::Map(id) | Value::Record(id) => {
            let (mut items, mut entries, mut fields) = (Vec::new(), Vec::new(), Vec::new());
            heap.visit(*id, &mut |entry| {
                match entry {
                    Element::Item(v) => items.push(datum(heap, v)),
                    Element::Entry { key, value } => {
                        entries.push((datum(heap, key), datum(heap, value)))
                    }
                    Element::Field { name, value } => {
                        fields.push((text(heap, name), datum(heap, value)))
                    }
                };
                ControlFlow::Continue(())
            })
            .unwrap();
            match value {
                Value::List(_) => Datum::List(items),
                Value::Set(_) => Datum::Set(items),
                Value::Map(_) => Datum::Map(entries),
                _ => Datum::Record(fields),
            }
        }
        _ => panic!("a corpus result has no Datum projection"),
    }
}

pub(crate) fn start(
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    layout: lash_kernel_vm::Layout,
) -> Result<lash_kernel_vm::KernelMachine, lash_kernel_vm::StartError> {
    let certificate = heap().certificate();
    lash_kernel_vm::KernelMachine::start_with_layout(
        program,
        bounds,
        lash_kernel_vm::CellStart {
            owner: PageId([9; 32]),
            target: lash_kernel_vm::Target::Main,
            args: Vec::new(),
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        },
        layout,
        Box::<Source>::default(),
        Box::new(Census::restore(certificate, LogicalBytes(bounds.memory)).unwrap()),
    )
}

pub(crate) fn reopen(heap: &mut PagedHeap, source: &Source) -> PagedHeap {
    heap.release_statement_roots().unwrap();
    heap.seal(SealBudget {
        pages: u64::MAX,
        bytes: u64::MAX,
    })
    .unwrap()
    .unwrap();
    let mut restored = PagedHeap::open_header(
        heap.sealed_root().unwrap(),
        Box::new(source.clone()),
        EditEpoch(2),
    )
    .unwrap();
    restored
        .set_census(Box::new(
            Census::restore(restored.certificate(), LogicalBytes(1 << 30)).unwrap(),
        ))
        .unwrap();
    restored
}

#[derive(Default)]
pub(crate) struct World {
    pub(crate) prints: Vec<Datum>,
}
impl lash_kernel_vm::Host for World {
    fn clock(&mut self) -> lash_kernel_doc::Timestamp {
        lash_kernel_doc::Timestamp {
            nanoseconds: 0.into(),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
    fn read(&mut self, _: &lash_kernel_doc::Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        unreachable!("self-contained corpus read")
    }
    fn print(&mut self, value: &Datum) {
        self.prints.push(value.clone());
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}
