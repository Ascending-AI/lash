//! Layer laws call natives on an arena bench as the machine calls them,
//! and run documents on the real machine over test-owned pages.
use lash_kernel_doc::heap::*;
use lash_kernel_doc::{
    Datum, ErrorDatum, Integer, NativeError, NativeFunction, Retry, Value, WorkCounter,
};
pub(crate) use lash_kernel_state::arena::Bench;
use lash_kernel_state::paged::*;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

#[derive(Clone, Default)]
pub(crate) struct Source {
    pages: Rc<RefCell<BTreeMap<PageId, PageAnswer>>>,
}
impl PageSource for Source {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        Ok(pages
            .iter()
            .map(|p| {
                self.pages
                    .borrow()
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
/// A fresh arena bench natives run on.
pub(crate) fn heap() -> Bench {
    Bench::new()
}
pub(crate) fn decode(bench: &mut Bench, datum: Datum) -> Value {
    bench.build(&datum).unwrap()
}
pub(crate) fn datum(bench: &Bench, value: &Value) -> Datum {
    bench.datum(value).unwrap()
}
pub(crate) fn text(bench: &Bench, value: &Value) -> String {
    bench.arena().semantics().text(value).unwrap().to_owned()
}
pub(crate) fn integer(bench: &Bench, value: &Value) -> Integer {
    bench.arena().semantics().integer(value).unwrap().unwrap()
}
/// Calls `native` as the machine does, with `room` to hold: the answer
/// the run sees.
pub(crate) fn call(
    bench: &mut Bench,
    native: &dyn NativeFunction,
    args: &[Value],
    room: u64,
    counter: &mut WorkCounter,
) -> Result<Value, NativeError> {
    bench.call(native, args, room, counter).answer
}
/// Whether `answer` is a refusal for want of room: the run ends at its
/// memory bound.
pub(crate) fn exhausted<T>(answer: &Result<T, NativeError>) -> bool {
    matches!(answer, Err(NativeError::Retry(Retry::Room { .. })))
}
/// A raised error as data: a refusal's data is null.
pub(crate) fn error_datum(bench: &Bench, error: &NativeError) -> Option<Datum> {
    match error {
        NativeError::Refused { kind, message } => Some(Datum::Error(Box::new(ErrorDatum {
            kind: (*kind).to_owned(),
            message: message.clone(),
            data: Datum::Null,
        }))),
        NativeError::Raised(error) => bench.datum(error).ok(),
        _ => None,
    }
}
/// What a raised error's kind and message read.
pub(crate) fn raised(
    bench: &Bench,
    answer: &Result<Value, NativeError>,
) -> Option<(String, String)> {
    match error_datum(bench, answer.as_ref().err()?)? {
        Datum::Error(error) => Some((error.kind, error.message)),
        _ => None,
    }
}

pub(crate) fn start(
    program: lash_kernel_vm::Program,
    bounds: lash_kernel_vm::Bounds,
    layout: lash_kernel_vm::kernel_internals::Layout,
) -> Result<lash_kernel_vm::KernelMachine, lash_kernel_vm::StartError> {
    lash_kernel_vm::kernel_internals::start_with_layout(
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
    )
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
