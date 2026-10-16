//! FIG-6041: fresh registration uses the build's function tokens, with the
//! same identities, manifests and recorded native answers.
#![cfg(target_os = "linux")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::Instant;

use lash_kernel_doc::document_work;
use lash_vm_client::WorkerTuning;

#[expect(
    dead_code,
    reason = "the worker replays the corpus, but never records it"
)]
#[expect(
    clippy::expect_used,
    reason = "the shared corpus serializes typed recorded JSON fixtures infallibly"
)]
#[path = "../../lash-vm-releases/src/build/corpus.rs"]
mod corpus;
#[path = "../../lash-vm-releases/src/build/probe.rs"]
mod probe;
#[path = "../../lash-vm-releases/src/build/corpus_data/mod.rs"]
mod recorded;

#[test]
fn a_fresh_embedding_parses_no_kernel_text_and_preserves_the_release() {
    let _ = document_work::take_definition_parses();
    let instructions = Instructions::start();
    let before = allocations();
    let start = Instant::now();
    let embedding =
        lash_vm_worker::built_dialects(&WorkerTuning::standard()).expect("a fresh embedding");
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
    let mut corpus = corpus::Corpus {
        releases: serde_json::from_str(recorded::RELEASES).expect("recorded releases"),
        ..Default::default()
    };
    for shard in recorded::BLOBS {
        let mut decoder = serde_json::Deserializer::from_str(shard);
        decoder.disable_recursion_limit();
        let blobs: BTreeMap<String, serde_json::Value> =
            serde::Deserialize::deserialize(&mut decoder).expect("recorded inputs");
        decoder.end().expect("one shard");
        corpus.blobs.extend(blobs);
    }
    for dialect in lash_vm_releases::DIALECTS {
        let held = dialect.held_releases().expect("built releases");
        for release in held.retained {
            let fingerprints = corpus
                .verify_release(registry, dialect.dialect, &release.release)
                .expect("the worker returns every recorded native answer");
            for function in release.functions {
                assert_eq!(
                    function.fingerprint.as_ref(),
                    fingerprints.get(&function.function)
                );
            }
        }
    }
    // The parent catalog holds retained releases plus changed helpers and
    // every successor kernel declaration this feature resolution interprets.
    let expected = lash_vm_library::standard_functions()
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
            assert_eq!(manifest.sealed, release.sealed);
            assert_eq!(manifest.writes, release.writes);
            let manifest_functions: BTreeMap<_, _> = manifest
                .functions
                .iter()
                .map(|f| (f.function, f.fingerprint.clone()))
                .collect();
            let released_functions: BTreeMap<_, _> = release
                .functions
                .iter()
                .map(|f| (f.function, f.fingerprint.clone()))
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
