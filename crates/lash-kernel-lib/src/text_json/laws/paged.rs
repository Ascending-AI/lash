//! Text natives run on the machine over cold pages: a call faults only the
//! chunks of a paged text it addresses (FIG-6193, FIG-6726).
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use lash_kernel_doc::heap::{HeapFault, PageId, PageRef};
use lash_kernel_doc::{Datum, FunctionRegistry};

use lash_kernel_state::paged::{PageAnswer, PageBody, PageSource, SealBudget};
use lash_kernel_vm::{
    Bounds, CellStart, KernelMachine, Machine, Outcome, Program, Request, Step, Target,
    register_machine_functions,
};

use crate::testsupport::{Source, World};

/// The pages a source has read, the payload leaves among them, and a page
/// it answers with another's content.
#[derive(Clone, Default)]
struct Counting {
    inner: Source,
    reads: Rc<Cell<u64>>,
    leaves: Rc<std::cell::RefCell<Vec<PageId>>>,
    swap: Rc<Cell<Option<(PageId, PageId)>>>,
}

impl PageSource for Counting {
    fn read(&mut self, pages: &[PageRef]) -> Result<Vec<Result<PageAnswer, HeapFault>>, HeapFault> {
        self.reads.set(self.reads.get() + pages.len() as u64);
        let asked: Vec<PageRef> = pages
            .iter()
            .map(|page| match self.swap.get() {
                Some((from, to)) if page.id == from => PageRef { id: to, ..*page },
                _ => *page,
            })
            .collect();
        let mut answers = self.inner.read(&asked)?;
        for (page, answer) in pages.iter().zip(&mut answers) {
            if let Ok(answer) = answer {
                // A substituted answer names the page it was asked for.
                answer.page = *page;
            }
            if let Ok(answer) = answer
                && answer.kind == lash_kernel_state::paged::PageKind::PAYLOAD_LEAF
            {
                self.leaves.borrow_mut().push(page.id);
            }
        }
        Ok(answers)
    }
    fn seal(&mut self, body: PageBody) -> Result<PageRef, HeapFault> {
        self.inner.seal(body)
    }
}

const BOUNDS: Bounds = Bounds {
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    call_depth: 100,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

/// Each slice, and a statement that reads no text, runs between two
/// parks, so each can run cold.
const ENTRY: &str = "definition \"function f(t: Text) -> Any\\nkernel 1\\nmetering statements\\nimport 0 operation host.echo(x: Any) -> Any\\nbody { let a = perform 0(0) as Any let x = text.utf16_slice(t, 0, 4096) let b = perform 0(1) as Any let y = text.utf16_slice(t, 7, 9) let c = perform 0(2) as Any let z = 1 let d = perform 0(3) as Any print x print y return z }\"\nmain { }\n";

fn program(operation: &str) -> Program {
    let mut registry = FunctionRegistry::new();
    register_machine_functions(&mut registry).unwrap();
    super::super::register_text_json(&mut registry).unwrap();
    let registry = Arc::new(registry);
    let mut text = String::from(
        "kernel 1\nnumbers by_spelling\nimport 0 operation host.echo(x: Any) -> Any\n",
    );
    let entry = ENTRY.replace("text.utf16_slice", operation);
    // A bytes operation takes its bytes as the entry's argument.
    let entry = if operation.starts_with("bytes.") {
        entry.replace("t: Text", "t: Bytes")
    } else {
        entry
    };
    text.push_str(&entry);
    let mut document = crate::testsupport::document(&text, &registry);
    let (id, definition) = document
        .functions
        .iter()
        .find(|(_, f)| f.name.as_str() == "f")
        .unwrap();
    document.exports.insert(
        lash_kernel_value::Name::new("f"),
        lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
            function: *id,
            signature: definition.signature.clone(),
            convention: lash_kernel_doc::StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    crate::testsupport::program(
        Arc::new(document),
        lash_kernel_vm::PreparedLibrary::new(registry),
    )
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
    let Request::Effect(effect) = request;
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

/// FIG-7692: a byte slice of cold paged bytes faults only the payload path
/// to the bytes it addresses, each node checked against the identity its
/// edge commits to, with the same ceiling as a text slice.
#[test]
fn a_byte_slice_of_cold_paged_bytes_reads_only_the_pages_it_addresses() {
    cold_slice_reads_only_addressed_pages("bytes.slice");
}

fn cold_slice_reads_only_addressed_pages(operation: &str) {
    let text: String = (0..20_000).map(|i| format!("{i:05}")).collect();
    let datum = |part: &str| {
        if operation.starts_with("bytes.") {
            Datum::Bytes(lash_kernel_doc::Bytes::new(part.as_bytes().to_vec()))
        } else {
            Datum::Text(part.to_string())
        }
    };
    let source = Counting::default();
    let mut machine = <lash_kernel_vm::KernelMachine as lash_kernel_vm::Machine>::start(
        program(operation),
        BOUNDS,
        CellStart {
            owner: PageId([9; 32]),
            target: Target::Entry("f".into()),
            args: vec![datum(&text)],
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        },
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
    assert_eq!(world.prints, [datum(&text[..4096]), datum(&text[7..9])]);
    let [wide, narrow, none] = reads[..] else {
        unreachable!()
    };
    assert!(
        wide <= none + 4 && narrow <= none + 4,
        "pages read: {wide} for 0..4096, {narrow} for 7..9, {none} for no text"
    );
}

/// Contract §6: a cold slice reads its payload node by node, each checked
/// against the identity its parent's edge commits to. A store that answers
/// the first leaf with another text's first leaf, equal in every count, is
/// refused when the slice reaches it, and no byte of it is exposed.
#[test]
fn a_cold_slice_refuses_a_leaf_that_is_not_the_identity_its_edge_commits_to() {
    let source = Counting::default();
    let operation = "text.utf16_slice";
    let start = |text: String| {
        let mut machine = <KernelMachine as Machine>::start(
            program(operation),
            BOUNDS,
            CellStart {
                owner: PageId([9; 32]),
                target: Target::Entry("f".into()),
                args: vec![Datum::Text(text)],
                session: None,
                modes: Default::default(),
                host: Vec::new(),
            },
            Box::new(source.clone()),
        )
        .unwrap();
        let mut world = World::default();
        let request = park(&mut machine, &mut world);
        reopen(&mut machine, &source, operation);
        (machine, world, request)
    };
    // The first leaf a slice of each text reads.
    let first_leaf = |text: String| {
        let (mut machine, mut world, request) = start(text);
        source.leaves.borrow_mut().clear();
        deliver(&mut machine, &request);
        park(&mut machine, &mut world);
        source.leaves.borrow()[0]
    };
    let honest: String = (0..20_000).map(|i| format!("{i:05}")).collect();
    let other: String = (1..20_001).map(|i| format!("{i:05}")).collect();
    let (leaf, foreign) = (first_leaf(honest.clone()), first_leaf(other));
    assert_ne!(leaf, foreign);
    let (mut machine, mut world, request) = start(honest);
    source.swap.set(Some((leaf, foreign)));
    deliver(&mut machine, &request);
    let fault = loop {
        match machine.run(&mut world, u64::MAX) {
            Ok(Step::Slice) => (),
            Ok(step) => panic!("a substituted leaf was read: {step:?}"),
            Err(error) => break format!("{error:?}"),
        }
    };
    assert!(fault.contains("payload leaf identity mismatch"), "{fault}");
    assert!(world.prints.is_empty());
}
