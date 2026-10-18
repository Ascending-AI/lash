//! Text natives run on the machine over cold pages: a call faults only the
//! chunks of a paged text it addresses (FIG-6193, FIG-6726).
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use lash_kernel_doc::heap::{HeapFault, PageId, PageRef};
use lash_kernel_doc::{Datum, FunctionRegistry, parse_document};
use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource, SealBudget};
use lash_kernel_vm::kernel_internals::{Layout, start_with_layout};
use lash_kernel_vm::{
    Bounds, CellStart, KernelMachine, Machine, Outcome, Program, Request, Step, Target,
    register_machine_functions,
};

use crate::testsupport::{Source, World};

/// The pages a source has read.
#[derive(Clone, Default)]
struct Counting {
    inner: Source,
    reads: Rc<Cell<u64>>,
}

impl PageSource for Counting {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        self.reads.set(self.reads.get() + pages.len() as u64);
        self.inner.read(pages)
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        self.inner.seal(body)
    }
}

const BOUNDS: Bounds = Bounds {
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    memory_limit: lash_kernel_vm::MemoryLimit::Recount,
    call_depth: 100,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

/// Each slice, and a statement that reads no text, runs between two
/// parks, so each can run cold.
const ENTRY: &str = "entry f(t: Text) -> Any\nfn f(t) {\n\
    let a = perform echo(0) as Any\n\
    let x = text.utf16_slice(t, 0, 4096)\n\
    let b = perform echo(1) as Any\n\
    let y = text.utf16_slice(t, 7, 9)\n\
    let c = perform echo(2) as Any\n\
    let z = 1\n\
    let d = perform echo(3) as Any\n\
    print x print y return z }\nmain { }\n";

fn program(operation: &str) -> Program {
    let mut registry = FunctionRegistry::new();
    register_machine_functions(&mut registry).unwrap();
    super::super::register_text_json(&mut registry).unwrap();
    let registry = Arc::new(registry);
    let mut text = String::from("kernel 1\nnumbers by_spelling\neffect echo(x: Any) -> Any\n");
    for (id, function) in registry.iter() {
        text.push_str(&format!("use {} = @{id}\n", function.definition().name));
    }
    text.push_str(&ENTRY.replace("text.utf16_slice", operation));
    Program {
        release: PageId([8; 32]),
        document: Arc::new(parse_document(&text).unwrap()),
        library: lash_kernel_vm::PreparedLibrary::new(registry),
    }
}

/// Runs to the next park and answers its one request.
fn park(machine: &mut KernelMachine, world: &mut World) -> Request {
    loop {
        match machine.run(world, u64::MAX).unwrap() {
            Step::Parked(mut park) => return park.requests.remove(0),
            Step::Slice => (),
            Step::Ended(end) => panic!("the run ended before its park: {end:?}"),
        }
    }
}

fn deliver(machine: &mut KernelMachine, request: &Request) {
    let Request::Effect(effect) = request else {
        panic!("{request:?}")
    };
    machine
        .deliver(effect.wait, Outcome::Completed(effect.args[0].clone()))
        .unwrap();
}

/// The machine sealed, and opened cold over `source`.
fn reopen(machine: &mut KernelMachine, source: &Counting, operation: &str) {
    let sealed = machine
        .seal(SealBudget {
            pages: u64::MAX,
            bytes: u64::MAX,
        })
        .unwrap()
        .unwrap();
    *machine = KernelMachine::open(
        program(operation),
        BOUNDS,
        &sealed.checkpoint(),
        Vec::new(),
        Box::new(source.clone()),
    )
    .unwrap();
}

/// FIG-6193: a UTF-16 slice of a cold paged text of 100 000 entries, each
/// leaf distinct, faults the chunks its range addresses and never decodes
/// the whole text: each call reads at most four pages more than a
/// statement that reads no text.
#[test]
fn a_utf16_slice_of_a_cold_paged_text_reads_only_the_pages_it_addresses() {
    cold_slice_reads_only_addressed_pages("text.utf16_slice");
}

/// FIG-6750: scalar-indexed slicing has the same cold-page economy as
/// UTF-16 slicing; retaining a large Python string never resends it whole.
#[test]
fn a_scalar_slice_of_a_cold_paged_text_reads_only_the_pages_it_addresses() {
    cold_slice_reads_only_addressed_pages("text.slice");
}

fn cold_slice_reads_only_addressed_pages(operation: &str) {
    let text: String = (0..20_000).map(|i| format!("{i:05}")).collect();
    let source = Counting::default();
    let mut machine = start_with_layout(
        program(operation),
        BOUNDS,
        CellStart {
            owner: PageId([9; 32]),
            target: Target::Entry("f".into()),
            args: vec![Datum::Text(text.clone())],
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        },
        Layout::default(),
        Box::new(source.clone()),
    )
    .unwrap();
    let mut world = World::default();
    let mut request = park(&mut machine, &mut world);
    let mut reads = Vec::new();
    for _ in 0..3 {
        reopen(&mut machine, &source, operation);
        let before = source.reads.get();
        deliver(&mut machine, &request);
        request = park(&mut machine, &mut world);
        reads.push(source.reads.get() - before);
    }
    deliver(&mut machine, &request);
    let end = loop {
        match machine.run(&mut world, u64::MAX).unwrap() {
            Step::Ended(end) => break end,
            Step::Slice => (),
            Step::Parked(park) => panic!("{park:?}"),
        }
    };
    assert!(matches!(end, lash_kernel_vm::End::Finished(_)), "{end:?}");
    assert_eq!(
        world.prints,
        [
            Datum::Text(text[..4096].to_string()),
            Datum::Text(text[7..9].to_string())
        ]
    );
    let [wide, narrow, none] = reads[..] else {
        unreachable!()
    };
    assert!(
        wide <= none + 4 && narrow <= none + 4,
        "pages read: {wide} for 0..4096, {narrow} for 7..9, {none} for no text"
    );
}
