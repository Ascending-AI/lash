//! FIG-6041: fresh registration uses the build's function tokens, with the
//! same identities.
#![cfg(target_os = "linux")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::Instant;

use lash_kernel_doc::document_work;
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

    let registry = embedding.registry().expect("the worker registry");
    let expected = lash_vm_releases::lash_packages()
        .map(|packages| std::sync::Arc::clone(packages.functions()))
        .expect("the complete interpreted release declarations")
        .export_declarations();
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

    let lash = lash_vm_releases::built().expect("lash's packages");
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

    // Native bindings are identities, and a body-only member cannot consume one.
    let body_only = dataframe
        .function("dataframe.col")
        .expect("body-only member");
    for function in [body_only, lash_kernel_doc::FunctionId::from_bytes([0; 32])] {
        let refusal = lash_vm_worker::Embedder::new()
            .native(function, Arc::new(Width))
            .and_then(|embedder| {
                embedder.dialect(lash_vm_worker::typescript(&WorkerTuning::standard())?)
            })
            .and_then(|embedder| embedder.finish(&held));
        assert!(
            matches!(refusal, Err(lash_vm_worker::EmbedError::Package(lash_kernel_package::PackageRefusal::NativeUnused { function: refused })) if refused == function)
        );
    }

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
        closure: lash_kernel_doc::heap::PageId([8; 32]),
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

/// POOL-FRAME: a shipped native keeps the callee identities it was built
/// against even when a retained kernel defines the same callee name.
#[test]
fn a_shipped_native_uses_its_declared_callee_identity_among_retained_versions() {
    use lash_kernel_doc::{
        Datum, Float, FunctionRegistry, NativeCall, NativeError, NativeFunction, ReleasedFunctions,
        Value,
    };
    use lash_kernel_package::{HeldPackages, Package, PackageSet, Root};
    use lash_kernel_vm::Machine as _;
    use std::sync::Arc;

    struct RetainedAdd;
    impl NativeFunction for RetainedAdd {
        fn call(&self, _: NativeCall<'_, '_>) -> Result<Value, NativeError> {
            Ok(Value::Float(Float::new(999.0)))
        }
    }
    let shipped = lash_vm_releases::lash_packages().expect("shipped packages");
    let current = shipped
        .functions()
        .named("num.add")
        .next()
        .expect("current add");
    let mut definition = shipped
        .functions()
        .get(&current)
        .expect("current definition")
        .definition()
        .as_ref()
        .clone();
    // Put the retained identity first to expose a global first-name lookup.
    let retained = (definition.native_version + 1..definition.native_version + 10_000)
        .find_map(|version| {
            definition.native_version = version;
            let function = definition.identity();
            (function < current).then_some(function)
        })
        .expect("a retained identity preceding the current one");
    let artifact = Arc::new(
        Package::lash(
            lash_kernel_package::PackagePath::parse("kernel").expect("path"),
            "retained-native",
            [Root::new("num")],
            &[],
            |registry| {
                registry
                    .register(definition.clone(), None)
                    .map_err(|error| error.to_string())?;
                Ok(vec![definition.clone()])
            },
        )
        .expect("retained kernel"),
    );
    let old = PackageSet::new([artifact.as_ref()]).expect("retained set");
    let held = HeldPackages::new(
        shipped.artifacts().values().cloned().chain([artifact]),
        shipped.sets().values().cloned().chain([old]),
        shipped.own(),
    )
    .expect("both versions held");
    let embedding = lash_vm_worker::built_dialects(&WorkerTuning::standard())
        .and_then(|builder| builder.native(retained, Arc::new(RetainedAdd)))
        .and_then(|builder| builder.finish(&held))
        .expect("all native identities supplied");
    let bound = embedding.registry().expect("bound frame");
    let add = bound
        .named("ts.add")
        .next()
        .expect("shipped TypeScript add");
    // Exercise the constructor's native, independently of its optional plan.
    let mut natives = FunctionRegistry::declarations();
    natives
        .register_released(
            ReleasedFunctions::over_frame(held.frame_indexes().expect("frame").0).expect("image"),
        )
        .expect("same image");
    for (function, registered) in bound.iter() {
        if let Some(native) = registered.native() {
            natives
                .implement(*function, native.clone())
                .expect("one binding");
        }
    }
    let document = lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers by_spelling\nuse ts.add = @{add}\nmain {{ let sum = invoke ts.add(1.5, 2.0) return sum }}"
    )).expect("document");
    let program = lash_kernel_vm::Program {
        closure: lash_kernel_doc::heap::PageId([8; 32]),
        document: Arc::new(document),
        library: lash_kernel_vm::PreparedLibrary::new(Arc::new(natives)),
    };
    let start = lash_kernel_vm::CellStart {
        owner: lash_kernel_doc::heap::PageId([9; 32]),
        target: lash_kernel_vm::Target::Main,
        args: Vec::new(),
        session: None,
        modes: Default::default(),
        host: Vec::new(),
    };
    let mut machine =
        lash_kernel_vm::KernelMachine::start(program, BOUNDS, start, Box::<Pages>::default())
            .expect("start");
    let result = loop {
        match machine.run(&mut World, u64::MAX).expect("run") {
            lash_kernel_vm::Step::Ended(lash_kernel_vm::End::Finished(done)) => break done.result,
            lash_kernel_vm::Step::Slice => {}
            other => panic!("addition must finish: {other:?}"),
        }
    };
    assert_eq!(
        result,
        Datum::Float(Float::new(3.5)),
        "the helper uses its declared current callee, not the retained one"
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

/// FIG-6985: a run whose code a worker lacks runs from the definitions
/// the store keeps under exactly the charges, their origins and the
/// outcome of the build that wrote it: with closed helpers answered by
/// their generated plans (layout 0) and with every body run (layout 3),
/// on a library assembled cold and on the same library cached.
#[test]
fn a_run_from_stored_definitions_charges_exactly_as_the_writers_build() {
    use std::sync::Arc;

    use lash_kernel_doc::{FunctionName, parse_document};
    use lash_kernel_package::{HeldPackages, Package, PackageSources};
    use lash_kernel_vm::Machine as _;
    use lash_kernel_vm::kernel_internals::{Layout, start_with_layout};

    let lash = lash_vm_releases::built().expect("lash's packages");
    let ts = lash
        .iter()
        .find(|package| package.function("ts.number.parse_int").is_some())
        .expect("the package of a planned helper");
    let parse_int = ts
        .function("ts.number.parse_int")
        .expect("a planned helper");
    let w = |release: &str, step: i64| {
        Package::build(
            "w",
            release,
            &PackageSources {
                kernel: &format!(
                    "use ts.number.parse_int\n\nfunction w.go(s: Text) -> Any\nkernel 1\ncharge 2\nbody {{\n  let n = invoke ts.number.parse_int(s, 10, false)\n  let out = [n, {step}]\n  return out\n}}\n"
                ),
                surface: "function go(s: Text) -> Any = w.go\n",
                labels: "go\tgo\n",
            },
            &lash.iter().collect::<Vec<_>>(),
        )
        .expect("w builds")
    };
    let (w1, w2) = (w("1.0.0", 1), w("2.0.0", 2));
    let go = w1.function("w.go").expect("release 1's w.go");
    let install = |w: &Package| {
        HeldPackages::install(lash.iter().cloned().chain([w.clone()])).expect("one registration")
    };
    let (writer, reader) = (install(&w1), install(&w2));
    let embed = |held: &HeldPackages| {
        lash_vm_worker::Embedder::new()
            .dialect(lash_vm_worker::typescript(&WorkerTuning::standard()).expect("typescript"))
            .and_then(|embedder| embedder.finish(held))
            .expect("the worker binds")
    };
    let (writes, reads) = (embed(&writer), embed(&reader));

    let closure =
        lash_vm_broker::kernel::closure::derive([go], writer.functions(), &BTreeMap::new())
            .expect("the writer's closure");
    let mut document = parse_document(&format!(
        "kernel 1\nnumbers float\nuse w.go = @{go}\nmain {{\n  let a = invoke w.go(\"123456789\")\n  return a\n}}\n"
    ))
    .expect("a document");
    for function in closure.body().identities() {
        let name = writer
            .functions()
            .name_of(&function)
            .expect("a held function");
        document
            .manifest
            .functions
            .insert(function, FunctionName::new(name).expect("a name"));
    }
    let document = Arc::new(document);
    let stored: Vec<(lash_kernel_doc::FunctionId, Vec<u8>)> = closure
        .body()
        .identities()
        .filter(|function| reader.functions().get(function).is_none())
        .map(|function| {
            let definition = writer.functions().get(&function).expect("held");
            (
                function,
                lash_kernel_doc::graph_tokens::encode_body(definition.definition().as_ref()),
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
    let definitions: Vec<(lash_kernel_doc::FunctionId, &[u8])> = stored
        .iter()
        .map(|(function, body)| (*function, body.as_slice()))
        .collect();
    let limits = lash_kernel_doc::KernelDataLimits::standard();
    let library = |embedding: &lash_vm_worker::Embedding, definitions| {
        lash_vm_worker::core_support::library_for(embedding, &document, definitions, limits)
            .expect("a prepared frame")
            .expect("the run's library assembles")
    };

    let run = |(library, closure): (
        lash_kernel_vm::PreparedLibrary,
        lash_vm_broker::kernel::closure::Closure,
    ),
               layout: Layout| {
        let program = lash_kernel_vm::Program {
            closure: lash_kernel_doc::heap::PageId(closure.reference().0),
            document: Arc::clone(&document),
            library,
        };
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
