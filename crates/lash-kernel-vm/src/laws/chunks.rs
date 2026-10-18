//! A large list or table opened cold materializes by chunk over its base
//! pages: a point write reads and seals one chunk plus its index path, so
//! its page work does not grow with the object (FIG-6726).

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

use lash_kernel_doc::heap::{HeapFault, PageRef};
use lash_kernel_doc::{
    FunctionId, FunctionRegistry, NativeCall, NativeError, NativeFunction, NewObject, Value,
    parse_definition,
};
use lash_kernel_state::paged::{AllocationCensus as _, PageAnswer, PageBody, PageSource};
use std::sync::Arc;

use super::embedder::{Embedder, ROOMY, Setup, Source, int, result};
use crate::{Layout, Step};

/// The page work a source saw.
#[derive(Clone, Default)]
struct Counting {
    inner: Source,
    reads: Rc<Cell<u64>>,
    seals: Rc<Cell<u64>>,
    bytes: Rc<Cell<u64>>,
}

impl PageSource for Counting {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        self.reads.set(self.reads.get() + pages.len() as u64);
        self.inner.read(pages)
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        self.seals.set(self.seals.get() + 1);
        self.bytes.set(self.bytes.get() + body.tokens.len() as u64);
        self.inner.seal(body)
    }
}

/// `work.table(n)`: a map from each integer below `n` to itself.
struct Table;
impl NativeFunction for Table {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let [Value::Int(length)] = call.args.values() else {
            return Err(NativeError::refused(
                "type_error",
                "work.table takes an integer",
            ));
        };
        let pairs = (0..*length)
            .map(|i| (Value::Int(i), Value::Int(i)))
            .collect();
        Ok(call.new.allocate(NewObject::Map(pairs)))
    }
}

fn tables(
    registry: &mut FunctionRegistry,
    _: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let definition =
        parse_definition("function work.table(length: Any) -> Any\nkernel 1\ncharge 1\nnative\n")
            .unwrap();
    vec![(
        "work.table",
        registry
            .register(definition, Some(Arc::new(Table)))
            .unwrap(),
    )]
}

fn park(embedder: &mut Embedder) {
    loop {
        match embedder.run() {
            Step::Parked(_) => return,
            Step::Slice => (),
            Step::Ended(end) => panic!("the run ended before its park: {end:?}"),
        }
    }
}

/// The reads, seals and sealed bytes of one point write, near the middle,
/// to the object `build(length)` makes, opened cold: from the open through
/// the next checkpoint.
fn point_write(build: &str, length: u64) -> (u64, u64, u64) {
    let text = format!(
        "main {{ let xs = {build}({length}) let a = perform echo(1) as Any \
         set xs[{at}] = 7 let b = perform echo(2) as Any return xs[{at}] }}",
        at = length / 2 + 3,
    );
    let mut embedder = Embedder::with(
        &text,
        Setup {
            probes: Some(tables),
            ..Setup::default()
        },
    );
    park(&mut embedder);
    let (root, certificate) = embedder.checkpoint();
    let counting = Counting {
        inner: embedder.source.clone(),
        ..Counting::default()
    };
    let (program, library) = embedder.program(true);
    embedder.machine = crate::KernelMachine::open_with_census(
        program,
        ROOMY,
        &root,
        Vec::new(),
        Box::new(counting.clone()),
        Box::new(
            lash_kernel_state::kernel_internals::census_impl::Census::restore(certificate).unwrap(),
        ),
        Layout::default(),
    )
    .unwrap();
    embedder.library = library;
    embedder.deliver_next();
    park(&mut embedder);
    embedder.checkpoint();
    let work = (
        counting.reads.get(),
        counting.seals.get(),
        counting.bytes.get(),
    );
    embedder.deliver_next();
    assert_eq!(result(embedder.run_to_end(&[])), int(7));
    work
}

/// Asserts a point write's page work is small, and grows tenfold in the
/// object by at most two index levels.
fn bounded(build: &str) {
    let (small_reads, small_seals, _) = point_write(build, 20_000);
    let (reads, seals, bytes) = point_write(build, 200_000);
    assert!(
        reads <= small_reads + 2 && seals <= small_seals + 2,
        "{build}: the page work grew from ({small_reads}, {small_seals}) to ({reads}, {seals})"
    );
    assert!(reads <= 32, "{build}: {reads} page reads");
    assert!(seals <= 32, "{build}: {seals} page seals");
    assert!(bytes <= 64 << 10, "{build}: {bytes} sealed bytes");
}

/// A point write to a cold list of 200,000 items decodes one chunk and
/// seals one leaf plus its index path.
#[test]
fn a_point_write_to_a_cold_large_list_reads_and_seals_one_chunk_and_its_path() {
    bounded("work.fill");
}

/// A point write to an existing key of a cold map of 200,000 entries
/// resolves the key through the base lookup tree and decodes only the
/// chunk holding it.
#[test]
fn a_point_write_to_a_cold_large_table_reads_and_seals_one_chunk_and_its_path() {
    bounded("work.table");
}
