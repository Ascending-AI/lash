//! FIG-6041: fresh registration uses the build's function tokens, with the
//! same identities and current manifests.
#![cfg(target_os = "linux")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::Instant;

use lash_kernel_doc::document_work;
use lash_vm_client::WorkerTuning;

#[test]
fn a_fresh_embedding_parses_no_kernel_text_and_preserves_the_release() {
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

    let registry = embedding.registry().expect("the worker registry");
    // The parent catalog holds retained releases plus changed helpers and
    // every successor kernel declaration this feature resolution interprets.
    let expected = lash_vm_library::lash_packages()
        .map(|packages| std::sync::Arc::clone(packages.functions()))
        .expect("the complete interpreted release declarations")
        .export_declarations();
    assert_eq!(
        registry.export_declarations(),
        expected,
        "exact FunctionIds and definitions"
    );
    let parent = lash_vm_client::graph::release_manifests().expect("parent manifests");
    for dialect in lash_vm_releases::DIALECTS {
        let held = dialect.held_releases().expect("retained manifests");
        for release in held.retained {
            let (_, manifest) = parent
                .manifests
                .of_dialect(dialect.dialect)
                .find(|(_, manifest)| manifest.ordinal == release.ordinal)
                .expect("a retained manifest");
            assert_eq!(manifest.release, release.release);
            assert_eq!(manifest.writes, release.writes);
            let manifest_functions: BTreeMap<_, _> = manifest
                .functions
                .iter()
                .map(|f| (f.function, f.native))
                .collect();
            let released_functions: BTreeMap<_, _> = release
                .functions
                .iter()
                .map(|f| (f.function, f.definition.has_native()))
                .collect();
            assert_eq!(manifest_functions, released_functions);
            let index = release.index();
            let worker = embedding
                .helper_releases()
                .find(|r| r.ordinal == index.ordinal)
                .expect("the worker holds this release");
            assert!(index.functions.is_subset(&worker.functions));
            assert!(index.writes.is_subset(&worker.writes));
        }
    }
    assert_eq!(
        parses, 0,
        "a fresh worker must read no kernel text after the build"
    );
}

/// FIG-6917: a host package installs once, beside lash's own, and its one
/// native reaches the worker by identity alone. A canonical document
/// calling its members is admitted against the parent's registry and runs
/// on the registry the single-binary worker derives from the same
/// registration.
#[test]
fn a_host_package_installs_once_and_runs_in_a_single_binary_worker() {
    use std::sync::Arc;

    use lash_kernel_doc::{Datum, Integer, parse_document};
    use lash_kernel_package::{HeldPackages, testing};
    use lash_kernel_vm::Machine as _;

    let lash = lash_vm_library::built().expect("lash's packages");
    let kernel = lash
        .iter()
        .find(|package| package.path().to_string() == "kernel")
        .expect("the kernel package");
    let dataframe = testing::dataframe(kernel).expect("the fixture builds");
    let width = dataframe
        .function("dataframe.width")
        .expect("its native function");
    let held = HeldPackages::install(lash.iter().cloned().chain([dataframe.clone()]))
        .expect("one registration");

    let embedding = lash_vm_worker::Embedder::new()
        .native(width, Arc::new(Width))
        .and_then(|embedder| {
            embedder.dialect(lash_vm_worker::typescript(&WorkerTuning::standard())?)
        })
        .and_then(|embedder| embedder.finish(&held))
        .expect("the worker binds to the parent's registration");
    let worker = Arc::clone(embedding.registry().expect("the worker registry"));
    assert!(
        worker
            .get(&width)
            .and_then(|function| function.native())
            .is_some(),
        "the host's native is installed by identity"
    );
    assert_eq!(
        worker.export_declarations(),
        held.functions().export_declarations(),
        "the worker holds exactly what the parent registered"
    );

    let mut text = String::from("kernel 1\nnumbers by_spelling\n");
    for name in [
        "dataframe.width",
        "dataframe.col",
        "dataframe.expr.name",
        "dataframe.data_quanta.read_text",
        "dataframe.data_quanta.filter",
        "dataframe.data_quanta.limit",
    ] {
        let function = dataframe.function(name).expect("a fixture member");
        text.push_str(&format!("use {name} = @{function}\n"));
    }
    text.push_str(
        "main {\n  let row = [1, 2, 3]\n  let n = invoke dataframe.width(row)\n  let e = invoke dataframe.col(\"value\")\n  let name = invoke dataframe.expr.name(e)\n  let q = invoke dataframe.data_quanta.read_text(\"a b\")\n  let q2 = invoke dataframe.data_quanta.filter(q, e)\n  let q3 = invoke dataframe.data_quanta.limit(q2, 5)\n  return (n, name, q3.plan[1].step)\n}\n",
    );
    let document = Arc::new(parse_document(&text).expect("a canonical kernel document"));
    lash_kernel_doc::validate_document(&document, held.functions().as_ref())
        .expect("the parent admits it");

    let program = lash_kernel_vm::Program {
        release: lash_kernel_doc::heap::PageId([8; 32]),
        document,
        library: lash_kernel_vm::PreparedLibrary::new(worker),
    };
    let start = lash_kernel_vm::CellStart {
        owner: lash_kernel_doc::heap::PageId([9; 32]),
        target: lash_kernel_vm::Target::Main,
        args: Vec::new(),
        session: None,
        modes: Default::default(),
        host: Vec::new(),
    };
    let mut machine = <lash_kernel_vm::KernelMachine as lash_kernel_vm::Machine>::start(
        program,
        BOUNDS,
        start,
        Box::<Pages>::default(),
    )
    .expect("the worker starts the run");
    let end = loop {
        match machine.run(&mut World, u64::MAX).expect("a slice") {
            lash_kernel_vm::Step::Ended(end) => break end,
            lash_kernel_vm::Step::Slice => {}
            lash_kernel_vm::Step::Parked(park) => panic!("the document never waits: {park:?}"),
        }
    };
    let lash_kernel_vm::End::Finished(done) = end else {
        panic!("the run finishes: {end:?}");
    };
    assert_eq!(
        done.result,
        Datum::Tuple(vec![
            Datum::Int(Integer::from(3)),
            Datum::Text("value".into()),
            Datum::Text("limit".into()),
        ])
    );
}

/// The fixture's `dataframe.width`, natively: a row's length.
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
    fn clock(&mut self) -> lash_kernel_doc::Timestamp {
        lash_kernel_doc::Timestamp {
            nanoseconds: 0.into(),
        }
    }
    fn random(&mut self) -> u64 {
        0
    }
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
