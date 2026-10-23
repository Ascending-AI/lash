//! FIG-6041: fresh registration uses the build's function tokens, with the
//! same identities.
#![cfg(target_os = "linux")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::Instant;

use lash_kernel_doc::document_work;
use lash_kernel_vm::Machine as _;
use lash_vm_client::WorkerTuning;

#[test]
fn a_fresh_embedding_parses_no_kernel_text_and_preserves_the_functions() {
    let _ = document_work::take_definition_parses();
    let instructions = Instructions::start();
    let before = allocations();
    let start = Instant::now();
    let embedding =
        lash_vm_worker::built_embedding(&WorkerTuning::standard()).expect("a fresh embedding");
    let _prepared = lash_kernel_vm::PreparedLibrary::new(std::sync::Arc::clone(
        embedding.registry().expect("the registered functions"),
    ));
    let elapsed = start.elapsed();
    let after = allocations();
    let instructions = instructions.map(Instructions::finish);
    eprintln!("fresh bootstrap instructions: {instructions:?}");
    let parses = document_work::take_definition_parses();
    eprintln!(
        "fresh bootstrap: parses={parses} allocations={} bytes={} wall_ns={}",
        after.0 - before.0,
        after.1 - before.1,
        elapsed.as_nanos()
    );

    // Owner churn and another embedding reuse the one installed immutable
    // process image; neither performs helper identity work again.
    let config =
        lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
    let first = config.worker_library().expect("the installed image");
    let _ = document_work::helper_work();
    let second = config.worker_library().expect("the same installed image");
    let resident =
        lash_vm_worker::built_embedding(&WorkerTuning::standard()).expect("a resident embedding");
    assert!(std::sync::Arc::ptr_eq(&first, &second));
    assert_eq!(first.helpers().identity(), second.helpers().identity());
    assert_eq!(
        document_work::helper_work().identity_hashes,
        0,
        "a new owner does not reinstall or rehash immutable helpers"
    );
    assert_eq!(
        resident.library_image().helpers().identity(),
        first.helpers().identity()
    );
    let registry = embedding.registry().expect("the worker registry");
    // Installation includes both kernel versions in a synthetic-next build.
    let expected = first.registry().export_declarations();
    assert_eq!(
        registry.export_declarations(),
        expected,
        "exact FunctionIds and definitions"
    );
    assert_eq!(
        parses, 0,
        "a fresh worker must read no kernel text after the build"
    );
}

/// WORKER-NATIVE-IDENTITY: native bindings consume exactly declared identities,
/// and a complete host-authored Document runs with those bindings.
#[test]
fn a_host_native_library_binds_exact_declarations_and_runs_complete_documents() {
    use lash_kernel_doc::{Datum, Integer, parse_definition, parse_document};
    use lash_kernel_vm::Machine as _;
    use lash_vm_client::packages::WorkerLibrary;
    use std::sync::Arc;
    let release = lash_vm_releases::helper_release().unwrap();
    let width = parse_definition(
        "function host.width(row: List(Any)) -> Int\nkernel 1\ncharge 1\nnative\n",
    )
    .unwrap();
    let width_id = width.identity();
    let guest = parse_definition(
        "function host.guest() -> Null\nkernel 1\ncharge 1\nbody { return null }\n",
    )
    .unwrap();
    let guest_id = guest.identity();
    let mut registry = release.functions.clone();
    registry.register(width.clone(), None).unwrap();
    registry.register(guest, None).unwrap();
    let library = WorkerLibrary::new(
        &registry,
        release
            .resolver_roots()
            .into_iter()
            .chain([width_id, guest_id]),
        &Default::default(),
    )
    .unwrap();
    for id in [guest_id, lash_vocabulary::FunctionId::from_bytes([0; 32])] {
        let refusal = lash_vm_worker::Embedder::new()
            .native(id, Arc::new(Width))
            .and_then(|embedder| {
                embedder.dialect(lash_vm_worker::typescript(&WorkerTuning::standard())?)
            })
            .and_then(|embedder| embedder.finish(&library));
        assert!(
            matches!(refusal, Err(lash_vm_worker::EmbedError::Library(lash_protocol_types::engine_fault::WorkerLibraryRefusal::NativeUnused { function })) if function == id)
        );
    }
    let embedding = lash_vm_worker::Embedder::new()
        .native(width_id, Arc::new(Width))
        .and_then(|embedder| {
            embedder.dialect(lash_vm_worker::typescript(&WorkerTuning::standard())?)
        })
        .and_then(|embedder| embedder.finish(&library))
        .unwrap();
    let worker = Arc::clone(embedding.registry().unwrap());
    assert!(worker.get(&width_id).unwrap().native().is_some());
    assert_eq!(
        worker.export_declarations(),
        library.registry().export_declarations()
    );
    let document = Arc::new(parse_document(&format!(
        "kernel 1\nnumbers by_spelling\ndefinition {}\nnative @{width_id}\nmain {{ let n = invoke host.width([1, 2, 3]) return n }}\n",
        serde_json::to_string(&lash_kernel_doc::print_definition(&width)).unwrap(),
    )).unwrap());
    let link = lash_vocabulary::Link {
        document: document.identity(),
        fills: BTreeMap::new(),
    };
    let id = link.identity();
    let program = lash_kernel_vm::Program::prepare(
        id,
        BTreeMap::from([(document.identity(), document)]),
        &BTreeMap::from([(id, link)]),
        lash_kernel_doc::heap::PageId([8; 32]),
        lash_kernel_vm::PreparedLibrary::new(worker),
        lash_kernel_doc::Selection::Fresh,
    )
    .unwrap();
    let mut machine = lash_kernel_vm::KernelMachine::start(
        program,
        BOUNDS,
        lash_kernel_vm::CellStart {
            owner: lash_kernel_doc::heap::PageId([9; 32]),
            target: lash_kernel_vm::Target::Main,
            args: vec![],
            session: None,
            modes: Default::default(),
            host: vec![],
        },
        Box::<Pages>::default(),
    )
    .unwrap();
    loop {
        match machine.run(&mut World, u64::MAX).unwrap() {
            lash_kernel_vm::Step::Slice => {}
            lash_kernel_vm::Step::Ended(lash_kernel_vm::End::Finished(done)) => {
                assert_eq!(done.result, Datum::Int(Integer::from(3)));
                break;
            }
            other => panic!("native run: {other:?}"),
        }
    }
}

/// The host's native row length.
struct Width;

impl lash_kernel_doc::NativeFunction for Width {
    fn call(
        &self,
        call: lash_kernel_doc::NativeCall<'_, '_>,
    ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
        let refused = || lash_kernel_doc::NativeError::Refused {
            kind: "type_error",
            message: "a row is a list".into(),
        };
        let row = call.args.get(0).ok_or_else(refused)?;
        let length = call.args.sequence(row)?.ok_or_else(refused)?.len();
        let length = i64::try_from(length).map_err(|_| refused())?;
        Ok(call.new.integer(lash_kernel_doc::Integer::from(length)))
    }
}

const BOUNDS: lash_kernel_vm::Bounds = lash_kernel_vm::Bounds {
    memory_meter: lash_kernel_vm::MemoryMeter::On,
    charge: lash_kernel_vm::Charge::Exact,
    charge_limit: 10_000_000,
    memory: 64 << 20,
    call_depth: 100,
    live_tasks: 100,
    requests_per_park: 100,
    join_members: 100,
};

struct World;

impl lash_kernel_vm::Host for World {
    fn read(
        &mut self,
        _: &lash_kernel_doc::Handle,
        _: &lash_kernel_doc::Datum,
    ) -> Result<lash_kernel_doc::Datum, lash_kernel_doc::ErrorDatum> {
        unreachable!("the document reads nothing")
    }
    fn print(&mut self, _: &lash_kernel_doc::Datum) {}
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// Pages held in memory, each by its content's digest.
#[derive(Default)]
struct Pages {
    pages: BTreeMap<lash_kernel_doc::heap::PageId, lash_kernel_state::paged::PageAnswer>,
}

impl lash_kernel_state::paged::PageSource for Pages {
    fn read(
        &mut self,
        pages: &[lash_kernel_doc::heap::PageRef],
    ) -> Result<
        Vec<Result<lash_kernel_state::paged::PageAnswer, lash_kernel_doc::heap::HeapFault>>,
        lash_kernel_doc::heap::HeapFault,
    > {
        Ok(pages
            .iter()
            .map(|page| {
                self.pages
                    .get(&page.id)
                    .cloned()
                    .ok_or(lash_kernel_doc::heap::HeapFault::Missing { page: page.id })
            })
            .collect())
    }
    fn seal(
        &mut self,
        body: lash_kernel_state::paged::PageBody,
    ) -> Result<lash_kernel_doc::heap::PageRef, lash_kernel_doc::heap::HeapFault> {
        let mut tokens = body.kind.0.to_be_bytes().to_vec();
        tokens.extend_from_slice(&body.tokens);
        let page = lash_kernel_doc::heap::PageRef {
            id: lash_kernel_doc::heap::PageId(lash_kernel_doc::heap_values::key_digest(&tokens)),
            grant: lash_kernel_doc::heap::PageGrant::NONE,
        };
        self.pages.insert(
            page.id,
            lash_kernel_state::paged::PageAnswer {
                page,
                kind: body.kind,
                tokens: body.tokens.into(),
                children: body.children,
            },
        );
        Ok(page)
    }
}

/// Per-thread user instructions, excluding the kernel and hypervisor.
struct Instructions(i32);

#[expect(
    unsafe_code,
    reason = "the measurement reads Linux perf instructions:u"
)]
impl Instructions {
    fn start() -> Result<Self, String> {
        let mut attr = [0u64; 8];
        attr[0] = 64u64 << 32; // hardware event, perf_event_attr size
        attr[1] = 1; // PERF_COUNT_HW_INSTRUCTIONS
        attr[5] = 1 | (1 << 5) | (1 << 6); // disabled, exclude kernel/hypervisor
        let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0, -1, -1, 0) };
        let fd = i32::try_from(fd).map_err(|error| error.to_string())?;
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let counter = Self(fd);
        if unsafe { libc::ioctl(fd, 0x2400, 0) } != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(counter)
    }

    fn finish(self) -> Result<u64, String> {
        let mut count = 0u64;
        if unsafe { libc::ioctl(self.0, 0x2401, 0) } != 0
            || unsafe { libc::read(self.0, (&raw mut count).cast(), 8) } != 8
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(count)
    }
}

#[expect(unsafe_code, reason = "the measurement owns its perf descriptor")]
impl Drop for Instructions {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

thread_local! {
    /// The bytes this thread asked the allocator for.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static ALLOCATED: Cell<u64> = const { Cell::new(0) };
}

/// The system allocator, counting what each thread asks of it.
struct Counting;

fn allocations() -> (u64, u64) {
    (ALLOCATIONS.with(Cell::get), ALLOCATED.with(Cell::get))
}

fn count(bytes: usize) {
    let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
    let _ = ALLOCATED.try_with(|allocated| allocated.set(allocated.get() + bytes as u64));
}

// SAFETY: every call forwards to the system allocator unchanged.
#[expect(
    unsafe_code,
    reason = "the law counts allocations by forwarding to the system allocator"
)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// FIG-6985: a run whose code a worker lacks runs from the definitions
/// the store keeps under exactly the charges, their origins and the
/// outcome of the build that wrote it: with closed helpers answered by
/// their generated plans (layout 0) and with every body run (layout 3),
/// on a library assembled cold and on the same library cached.
#[test]
fn a_run_from_stored_definitions_charges_exactly_as_the_writers_build() {
    use std::sync::Arc;

    use lash_kernel_doc::parse_document;

    use lash_kernel_vm::kernel_internals::{Layout, start_with_layout};
    use lash_vm_client::packages::WorkerLibrary;
    let release = lash_vm_releases::helper_release().unwrap();
    let parse_int = release
        .functions
        .named("ts.number.parse_integer")
        .next()
        .unwrap();
    let definition = |step| {
        lash_kernel_doc::parse_definition(&format!(
        "function w.go(s: Text) -> Any\nkernel 1\ncharge 2\nuse ts.number.parse_integer = @{parse_int}\nbody {{ let n = invoke ts.number.parse_integer(s, 10, false) let out = [n, {step}] return out }}\n"
    )).unwrap()
    };
    let (w1, w2) = (definition(1), definition(2));
    let go = w1.identity();
    let install = |definition: lash_kernel_doc::FunctionDefinition| {
        let id = definition.identity();
        let mut registry = release.functions.clone();
        registry.register(definition, None).unwrap();
        WorkerLibrary::new(
            &registry,
            release.resolver_roots().into_iter().chain([id]),
            &Default::default(),
        )
        .unwrap()
    };
    let (writer, reader) = (install(w1), install(w2));
    let embed = |library: &WorkerLibrary| {
        lash_vm_worker::Embedder::new()
            .dialect(lash_vm_worker::typescript(&WorkerTuning::standard()).unwrap())
            .unwrap()
            .finish(library)
            .unwrap()
    };
    let (writes, reads) = (embed(&writer), embed(&reader));
    let closure = lash_vm_broker::kernel::closure::derive(
        [go],
        writer.registry(),
        &BTreeMap::new(),
        &lash_kernel_doc::BuildHelpers::empty(),
        &Default::default(),
    )
    .unwrap();
    let mut text = String::from("kernel 1\nnumbers float\n");
    for function in closure.body().identities() {
        let definition = writer.registry().get(&function).unwrap();
        text.push_str(&format!(
            "definition {}\n",
            serde_json::to_string(&lash_kernel_doc::print_definition(definition.definition()))
                .unwrap()
        ));
        if definition.definition().has_native() {
            text.push_str(&format!("native @{function}\n"));
        }
    }
    text.push_str("main { let a = invoke w.go(\"123456789\") return a }\n");
    let document = Arc::new(parse_document(&text).unwrap());
    let stored: Vec<(lash_vocabulary::names::FunctionId, Vec<u8>)> = closure
        .body()
        .identities()
        .filter(|function| reader.registry().get(function).is_none())
        .map(|function| {
            let definition = writer.registry().get(&function).expect("held");
            (
                function,
                lash_vocabulary::tokens::encode_body(definition.definition().as_ref()),
            )
        })
        .collect();
    assert_eq!(
        stored
            .iter()
            .map(|(function, _)| *function)
            .collect::<Vec<_>>(),
        vec![go],
        "the reader lacks release 1's w.go alone"
    );
    let definitions: Vec<(lash_vocabulary::names::FunctionId, &[u8])> = stored
        .iter()
        .map(|(function, body)| (*function, body.as_slice()))
        .collect();
    let limits = lash_vocabulary::KernelDataLimits::standard();
    let library = |embedding: &lash_vm_worker::Embedding, definitions| {
        lash_vm_worker::core_support::library_for(
            embedding,
            &document,
            definitions,
            limits,
            &lash_kernel_doc::Selection::Fresh,
        )
        .expect("a prepared frame")
        .expect("the run's library assembles")
    };

    let run = |(library, closure): (
        lash_kernel_vm::PreparedLibrary,
        lash_vm_broker::kernel::closure::Closure,
    ),
               layout: Layout| {
        let link = lash_vocabulary::Link {
            document: document.identity(),
            fills: BTreeMap::new(),
        };
        let id = link.identity();
        let program = lash_kernel_vm::Program::prepare(
            id,
            BTreeMap::from([(document.identity(), document.clone())]),
            &BTreeMap::from([(id, link)]),
            lash_kernel_doc::heap::PageId(closure.reference().0),
            library,
            lash_kernel_doc::Selection::Fresh,
        )
        .unwrap();
        let start = lash_kernel_vm::CellStart {
            owner: lash_kernel_doc::heap::PageId([9; 32]),
            target: lash_kernel_vm::Target::Main,
            args: Vec::new(),
            session: None,
            modes: Default::default(),
            host: Vec::new(),
        };
        let mut machine =
            start_with_layout(program, BOUNDS, start, layout, Box::<Pages>::default())
                .expect("the run starts");
        let tape = lash_kernel_vm::ChargeTape::new();
        machine.record_charges(tape.clone());
        let end = loop {
            match machine.run(&mut World, u64::MAX).expect("a slice") {
                lash_kernel_vm::Step::Ended(end) => break end,
                lash_kernel_vm::Step::Slice => {}
                lash_kernel_vm::Step::Parked(park) => panic!("the run never waits: {park:?}"),
            }
        };
        let lash_kernel_vm::End::Finished(done) = end else {
            panic!("the run finishes: {end:?}");
        };
        (done.result, tape.steps())
    };

    // Assembled once, cold: nothing compiled, nothing cached.
    let cold = library(&reads, &definitions);
    assert!(
        cold.0
            .registry()
            .get(&parse_int)
            .and_then(|function| function.plan())
            .is_some(),
        "the assembled library keeps the planned helper's plan"
    );
    assert_eq!(cold.1.reference(), closure.reference());
    for layout in [Layout(0), Layout(3)] {
        let all_a = run(library(&writes, &[]), layout);
        assert!(!all_a.1.is_empty(), "the run charges");
        assert_eq!(run(cold.clone(), layout), all_a, "cold, layout {layout:?}");
        // Answered from the worker's cache of assembled libraries.
        let warm = run(library(&reads, &definitions), layout);
        assert_eq!(warm, all_a, "warm, layout {layout:?}");
    }
}
