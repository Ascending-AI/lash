//! Development-only, exact allocation-site windows. Bookkeeping allocations
//! bypass the observer, but remain in the process outstanding-byte counter.
#![expect(
    unsafe_code,
    reason = "allocator forwarding, native unwinding and Linux stack painting"
)]
#![expect(
    clippy::disallowed_methods,
    reason = "explicit development profile files and process measurements"
)]

use lash_kernel_vm::perf_witness::{GuestContext, Profiling as _};
use serde::Serialize;
use serde_json::{Value, json};
use std::alloc::{GlobalAlloc, Layout};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

const _: () = assert!(matches!(env!("CARGO_PKG_VERSION").as_bytes(), b"0.0.0-dev"));
const FRAMES: usize = 32;
static OUTSTANDING: AtomicI64 = AtomicI64::new(0);

/// Wraps the executable's existing allocator. Inactive and active modes use
/// exactly the same executable and allocator forwarding.
pub struct Alloc<A>(pub A);

// SAFETY: all allocation operations forward the caller's exact layout and
// pointer contract to A. Observation never changes the returned allocation.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Alloc<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { self.0.alloc(layout) };
        if !pointer.is_null() {
            OUTSTANDING.fetch_add(layout.size() as i64, Ordering::Relaxed);
            record(pointer as usize, layout.size(), false);
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { self.0.alloc_zeroed(layout) };
        if !pointer.is_null() {
            OUTSTANDING.fetch_add(layout.size() as i64, Ordering::Relaxed);
            record(pointer as usize, layout.size(), false);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        forget(pointer as usize);
        OUTSTANDING.fetch_sub(layout.size() as i64, Ordering::Relaxed);
        unsafe { self.0.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { self.0.realloc(pointer, layout, size) };
        if !next.is_null() {
            forget(pointer as usize);
            OUTSTANDING.fetch_add(size as i64 - layout.size() as i64, Ordering::Relaxed);
            record(next as usize, size, true);
        }
        next
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct Key {
    frames: [usize; FRAMES],
    guest: (u32, u32, u64),
}
#[derive(Default, Serialize)]
struct Site {
    count: u64,
    allocations: u64,
    reallocations: u64,
    bytes: u64,
    live: u64,
    peak_live: u64,
    at_guest_peak: u64,
}
struct Capture {
    context: Option<Arc<GuestContext>>,
    keys: HashMap<Key, usize>,
    sites: Vec<(Key, Site)>,
    live: HashMap<usize, (usize, usize)>,
    total_live: u64,
    peak_live: u64,
    guest_peak: [u64; 4],
    peak_process: Value,
    baseline: Value,
    rss_peak_reset: bool,
}
thread_local! {
    static BUSY: Cell<bool> = const { Cell::new(false) };
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
    static DIRECTORY: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static REQUESTS: Cell<[u64; 2]> = const { Cell::new([0; 2]) };
    static WINDOW_REQUESTS: Cell<[u64; 2]> = const { Cell::new([0; 2]) };
    static ORDINAL: Cell<u64> = const { Cell::new(0) };
    static STACK: RefCell<Option<Stack>> = const { RefCell::new(None) };
}
struct Guard;
impl Guard {
    fn enter() -> Option<Self> {
        BUSY.try_with(|busy| (!busy.replace(true)).then(|| Self))
            .ok()
            .flatten()
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = BUSY.try_with(|busy| busy.set(false));
    }
}

fn record(pointer: usize, bytes: usize, reallocated: bool) {
    let Some(_guard) = Guard::enter() else {
        return;
    };
    let _ = REQUESTS.try_with(|requests| {
        let mut counts = requests.get();
        counts[usize::from(reallocated)] += 1;
        requests.set(counts);
    });
    let _ = CAPTURE.try_with(|capture| {
        let mut capture = capture.borrow_mut();
        let Some(capture) = capture.as_mut() else {
            return;
        };
        let mut key = Key {
            frames: [0; FRAMES],
            guest: (u32::MAX, u32::MAX, 0),
        };
        if let Some(context) = &capture.context {
            key.guest = context.allocation_context().unwrap_or(key.guest);
        }
        let mut depth = 0;
        // The observer guard excludes nested allocator observations.
        backtrace::trace(|frame| {
            if depth == FRAMES {
                return false;
            }
            key.frames[depth] = frame.ip() as usize;
            depth += 1;
            true
        });
        let index = if let Some(index) = capture.keys.get(&key) {
            *index
        } else {
            let index = capture.sites.len();
            capture.sites.push((key, Site::default()));
            capture.keys.insert(key, index);
            index
        };
        let site = &mut capture.sites[index].1;
        site.count += 1;
        site.allocations += u64::from(!reallocated);
        site.reallocations += u64::from(reallocated);
        site.bytes += bytes as u64;
        site.live += bytes as u64;
        site.peak_live = site.peak_live.max(site.live);
        capture.live.insert(pointer, (index, bytes));
        capture.total_live += bytes as u64;
        capture.peak_live = capture.peak_live.max(capture.total_live);
    });
}
fn forget(pointer: usize) {
    let Some(_guard) = Guard::enter() else {
        return;
    };
    let _ = CAPTURE.try_with(|capture| {
        if let Some(capture) = capture.borrow_mut().as_mut()
            && let Some((index, bytes)) = capture.live.remove(&pointer)
        {
            capture.sites[index].1.live -= bytes as u64;
            capture.total_live -= bytes as u64;
        }
    });
}

/// Opens a fresh window on this thread. Preexisting allocations are excluded
/// from site live bytes and disclosed by the baseline process snapshot.
pub fn begin() {
    let _guard = Guard::enter();
    let baseline = process();
    let rss_peak_reset = std::fs::write("/proc/self/clear_refs", b"5").is_ok();
    CAPTURE.with(|capture| {
        *capture.borrow_mut() = Some(Capture {
            context: None,
            keys: HashMap::new(),
            sites: Vec::new(),
            live: HashMap::new(),
            total_live: 0,
            peak_live: 0,
            guest_peak: [0; 4],
            peak_process: Value::Null,
            baseline,
            rss_peak_reset,
        })
    });
    lash_kernel_vm::perf_witness::observe_memory(Some(memory));
}
/// Joins allocations to the machine's current guest operation context.
pub fn attach(machine: &mut lash_kernel_vm::KernelMachine) {
    let _guard = Guard::enter();
    CAPTURE.with(|capture| {
        if let Some(capture) = capture.borrow_mut().as_mut()
            && capture.context.is_none()
        {
            let context = GuestContext::new();
            machine.publish_context(Arc::clone(&context));
            machine.count_work();
            capture.context = Some(context);
        }
    });
}
fn memory(ledger: [u64; 4]) {
    let Some(_guard) = Guard::enter() else {
        return;
    };
    CAPTURE.with(|capture| {
        if let Some(capture) = capture.borrow_mut().as_mut()
            && ledger.iter().sum::<u64>() > capture.guest_peak.iter().sum::<u64>()
        {
            capture.guest_peak = ledger;
            for (_, site) in &mut capture.sites {
                site.at_guest_peak = site.live;
            }
            capture.peak_process = process_peak();
        }
    });
}

/// Closes before machine destruction, keeping retained guest values visible.
pub fn finish(machine: &lash_kernel_vm::KernelMachine) -> Option<Value> {
    memory(machine.memory_ledger());
    let _guard = Guard::enter();
    lash_kernel_vm::perf_witness::observe_memory(None);
    let capture = CAPTURE.with(|capture| capture.borrow_mut().take())?;
    let end_process = process();
    let stack = STACK.with(|stack| stack.borrow().as_ref().map(Stack::scan));
    let symbols = machine.guest_symbols();
    let work = machine.work();
    let mut frames = HashMap::<usize, Vec<String>>::new();
    let sites = capture.sites.iter().map(|(key, site)| {
        for ip in key.frames.iter().copied().filter(|ip| *ip != 0) {
            frames.entry(ip).or_insert_with(|| {
                let mut inline = Vec::new();
                backtrace::resolve(ip as *mut _, |symbol| {
                    inline.push(format!("{} {}:{}", symbol.name().map(|s| s.to_string()).unwrap_or_default(),
                        symbol.filename().map(|p| p.display().to_string()).unwrap_or_default(), symbol.lineno().unwrap_or(0)));
                });
                inline
            });
        }
        let (code, statement, phase) = key.guest;
        let guest = if phase & 0xff == 2 || phase & 0xff == 3 {
            symbols.libs.iter().find(|s| s.token == (phase >> 32) as u32).map(|s| s.name.clone())
        } else { symbols.codes.iter().find(|s| s.token == code).map(|s| s.name.clone()) };
        let operations = work.as_ref().and_then(|work| {
            if phase & 0xff == 2 || phase & 0xff == 3 {
                work.natives.iter().find(|row| row.symbol.token == (phase >> 32) as u32).map(|row| row.counts.calls)
            } else {
                work.codes.iter().find(|row| row.symbol.token == code).map(|row| row.counts.statements + row.counts.expressions)
            }
        });
        json!({"stats":site, "frames": key.frames.iter().filter(|ip| **ip != 0).collect::<Vec<_>>(),
            "guest":guest,"code":code,"statement":statement,"phase":phase,
            "guest_operations":operations, "allocations_per_guest_operation": operations.filter(|n| *n > 0).map(|n| site.count as f64 / n as f64)})
    }).collect::<Vec<_>>();
    Some(
        json!({"kind":"lash.cell-memory", "process_id":std::process::id(), "completed":true,
        "scope":"calling_thread_allocations_since_window_start_before_machine_destruction",
        "reallocations":"counted as one allocation of the new requested size",
        "baseline":capture.baseline, "rss_peak_reset":capture.rss_peak_reset, "guest_peak_ledger":capture.guest_peak,
        "guest_peak_process":capture.peak_process,"end_ledger":machine.memory_ledger(),
        "end_process":end_process,"peak_live":capture.peak_live,"end_live":capture.total_live,
        "sites":sites,"frame_table":frames,"stack":stack,
        "stack_frames_at_peak":null,
        "guest_peak_resolution":"statement verdict points; transient allocations within a native operation may peak between verdicts",
        "stack_trace_depth_limit":FRAMES}),
    )
}

fn process_peak() -> Value {
    let requested = OUTSTANDING.load(Ordering::Relaxed);
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let resident = statm
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok());
    let info = unsafe { libc::mallinfo2() };
    json!({"requested_process_bytes":requested,
        "rss_bytes":resident.map(|pages| pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64),
        "glibc_in_use_bytes":info.uordblks + info.hblkhd,"glibc_free_arena_bytes":info.fordblks})
}
/// Non-observer allocation and reallocation requests on the calling thread.
pub fn requests() -> [u64; 2] {
    REQUESTS.get()
}

fn process() -> Value {
    let requested = OUTSTANDING.load(Ordering::Relaxed);
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let hwm = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<u64>().ok());
    let maps = std::fs::read_to_string("/proc/self/smaps").unwrap_or_default();
    let mut rss = [0u64; 4];
    let mut group = 3;
    for line in maps.lines() {
        if line.as_bytes().first().is_some_and(u8::is_ascii_hexdigit) && line.contains('-') {
            group = if line.contains("[stack") {
                2
            } else if line.contains('/') {
                1
            } else {
                0
            };
        } else if let Some(kib) = line
            .strip_prefix("Rss:")
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<u64>().ok())
        {
            rss[group] += kib * 1024;
        }
    }
    // SAFETY: glibc's allocator statistics have no pointer arguments and no
    // ownership transfer. This collector is Linux/glibc only.
    let allocator = unsafe { libc::mallinfo2() };
    json!({"requested_process_bytes":requested,"rss_bytes":(!maps.is_empty()).then(||rss.iter().sum::<u64>()), "rss_high_water_bytes":hwm.map(|kib|kib*1024),
        "anonymous_rss_bytes":(!maps.is_empty()).then_some(rss[0]),"file_rss_bytes":(!maps.is_empty()).then_some(rss[1]),"stack_rss_bytes":(!maps.is_empty()).then_some(rss[2]),
        "other_rss_bytes":(!maps.is_empty()).then_some(rss[3]),"glibc_in_use_bytes":allocator.uordblks + allocator.hblkhd,
        "glibc_free_arena_bytes":allocator.fordblks,"glibc_arena_bytes":allocator.arena,
        "glibc_mmap_bytes":allocator.hblkhd,
        "reconciliation":"RSS is resident mappings; requested bytes and glibc capacity include nonresident pages. Free arenas are allocator slack, file RSS includes executable and libraries; observer bookkeeping is in process requested bytes but excluded from sites."})
}

struct Stack {
    low: usize,
    top: usize,
    painted_end: usize,
}
impl Stack {
    fn paint() -> std::io::Result<Self> {
        let mut attr = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
        let mut address = std::ptr::null_mut();
        let mut length = 0;
        // SAFETY: pthread initializes attr for this thread; all outputs are
        // valid for the duration of these calls. Its stack grows down on x86_64.
        unsafe {
            if libc::pthread_getattr_np(libc::pthread_self(), attr.as_mut_ptr()) != 0 {
                return Err(std::io::Error::other("pthread_getattr_np failed"));
            }
            let mut attr = attr.assume_init();
            let result = libc::pthread_attr_getstack(&attr, &mut address, &mut length);
            libc::pthread_attr_destroy(&mut attr);
            if result != 0 {
                return Err(std::io::Error::other("pthread_attr_getstack failed"));
            }
        }
        let sp: usize;
        // SAFETY: read the current stack pointer without changing it.
        unsafe {
            std::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags));
        }
        let low = address as usize;
        let top = low + length;
        let end = sp.saturating_sub(4096);
        if !(low < end && end < top) {
            return Err(std::io::Error::other("unsupported stack layout"));
        }
        // SAFETY: paint only this thread's unused stack, below its current
        // frames and a 4096-byte red-zone/call margin. No Rust object lives
        // there. No call is made while painting. Linux grows the main mapping.
        unsafe {
            std::arch::asm!("rep stosb", inout("rdi") low => _, inout("rcx") end-low => _, in("al") 0xa5u8, options(nostack));
        }
        Ok(Self {
            low,
            top,
            painted_end: end,
        })
    }
    fn scan(&self) -> Value {
        let mut at = self.low;
        // SAFETY: same-thread byte reads in the initialized, painted mapping.
        // These addresses remain mapped throughout the window.
        unsafe {
            while at < self.painted_end && (at as *const u8).read_volatile() == 0xa5 {
                at += 1;
            }
        }
        json!({"thread_id":unsafe { libc::syscall(libc::SYS_gettid) },
            "peak_written_bytes":self.top-at,"painted_bytes":self.painted_end-self.low,
            "initial_unpainted_bytes":self.top-self.painted_end,
            "scope":"original native thread stack; includes observer frames; stacker alternate mappings excluded",
            "resolution_bytes":1,"saturated":at == self.low})
    }
}

/// Initializes a native stack watermark on the calling Linux x86_64 thread.
/// Paints before the cell, scans after it. Includes the initial call margin.
pub fn stack_start() -> std::io::Result<()> {
    let painted = Stack::paint()?;
    STACK.with(|stack| *stack.borrow_mut() = Some(painted));
    Ok(())
}

pub(crate) fn worker_install(args: &[String]) -> std::io::Result<()> {
    if let Some(at) = args.iter().position(|arg| arg == "--cell-memory-dir") {
        let directory = PathBuf::from(
            args.get(at + 1)
                .ok_or_else(|| std::io::Error::other("missing cell-memory directory"))?,
        );
        std::fs::create_dir_all(&directory)?;
        DIRECTORY.with(|slot| *slot.borrow_mut() = Some(directory));
        ENABLED.set(!args.iter().any(|arg| arg == "--cell-memory-off"));
        if args.iter().any(|arg| arg == "--cell-stack") {
            stack_start()?;
        }
    }
    Ok(())
}
pub(crate) fn worker_begin() -> std::io::Result<()> {
    if DIRECTORY.with(|directory| directory.borrow().is_some()) {
        WINDOW_REQUESTS.set(requests());
        if STACK.with(|stack| stack.borrow().is_some()) {
            stack_start()?;
        }
        if ENABLED.get() {
            begin();
        }
    }
    Ok(())
}
pub(crate) fn worker_finish(machine: &lash_kernel_vm::KernelMachine) -> std::io::Result<()> {
    let directory = DIRECTORY.with(|directory| directory.borrow().clone());
    if let Some(directory) = directory {
        let before = WINDOW_REQUESTS.get();
        let after = requests();
        let counts = [after[0] - before[0], after[1] - before[1]];
        let mut report = if ENABLED.get() {
            finish(machine)
                .ok_or_else(|| std::io::Error::other("missing cell allocation window"))?
        } else {
            json!({"kind":"lash.cell-memory-counts","process_id":std::process::id(),"completed":true})
        };
        report["requests"] = json!(counts);
        let ordinal = ORDINAL.get() + 1;
        ORDINAL.set(ordinal);
        std::fs::write(
            directory.join(format!(
                "vm-worker-{}-cell-{ordinal}.json",
                std::process::id()
            )),
            serde_json::to_vec(&report)?,
        )?;
    }
    Ok(())
}
