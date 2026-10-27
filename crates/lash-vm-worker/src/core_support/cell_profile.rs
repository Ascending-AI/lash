//! Linux x86-64 CPU samples with the interrupted thread's guest context.
//!
//! The timer targets the executing thread. Its handler copies into preallocated
//! storage, without allocating, locking, symbolizing or doing file I/O. Native
//! frames and guest tokens belong to that same interrupt, including native work.
#![expect(
    unsafe_code,
    reason = "thread-directed POSIX timer, signal context and bounded native stack reads"
)]

use std::alloc::{GlobalAlloc, Layout};
use std::cell::UnsafeCell;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lash_kernel_value::sites::SourceLocations;
use lash_kernel_vm::perf_witness::{GuestContext, GuestPhase, GuestSample, Profiling};
use serde::Serialize;

use super::instructions::InstructionCounter;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Decorates the executable's allocator with allocation-call counting.
pub struct CountingAllocator<A>(pub A);
// SAFETY: every operation forwards the unchanged pointer/layout to its allocator.
unsafe impl<A: GlobalAlloc> GlobalAlloc for CountingAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded GlobalAlloc contract.
        unsafe { self.0.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded GlobalAlloc contract.
        unsafe { self.0.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded GlobalAlloc contract.
        unsafe { self.0.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded GlobalAlloc contract.
        unsafe { self.0.dealloc(ptr, layout) }
    }
}

const NATIVE_DEPTH: usize = 128;
const SAMPLE_LIMIT: usize = 12_000;

struct Sample {
    guest: [u64; GuestContext::DEPTH],
    guest_len: usize,
    native: [u64; NATIVE_DEPTH],
    native_len: usize,
    truncated_guest: bool,
    truncated_native: bool,
    phase: GuestPhase,
}
struct State {
    context: Arc<GuestContext>,
    scratch: GuestSample,
    samples: Vec<Sample>,
    stack_end: usize,
    refused: u64,
    overflow: u64,
    overruns: u64,
    timer: libc::timer_t,
    counter: InstructionCounter,
    handler_instructions: u64,
    counter_failures: u64,
}

struct Errno(i32);
impl Drop for Errno {
    fn drop(&mut self) {
        // SAFETY: libc's errno pointer is thread-local and valid for this thread.
        unsafe {
            *libc::__errno_location() = self.0;
        }
    }
}

extern "C" fn interrupt(_: i32, info: *mut libc::siginfo_t, registers: *mut libc::c_void) {
    // SAFETY: only our thread-directed timer installs this handler. Its sigval
    // points to State until the timer is deleted and pending signals are drained.
    // The handler cannot reenter itself; the owning thread never accesses State
    // while the signal is unblocked. Scratch and samples already have capacity.
    unsafe {
        let _errno = Errno(*libc::__errno_location());
        let state = &mut *((*info).si_value().sival_ptr.cast::<State>());
        let before = state.counter.try_snapshot();
        capture_interrupt(state, registers);
        match (before, state.counter.try_snapshot()) {
            (Some(before), Some(after))
                if after.time_enabled > before.time_enabled
                    && (after.time_running - before.time_running) as f64
                        / (after.time_enabled - before.time_enabled) as f64
                        >= 0.99 =>
            {
                state.handler_instructions += after.instructions - before.instructions;
            }
            _ => state.counter_failures += 1,
        }
    }
}

fn capture_interrupt(state: &mut State, registers: *mut libc::c_void) {
    // SAFETY: called only by interrupt with its live signal context and exclusive
    // preallocated state. All pointers below follow the handler's bounds checks.
    unsafe {
        state.overruns += libc::timer_getoverrun(state.timer).max(0) as u64;
        if state.samples.len() == state.samples.capacity() {
            state.overflow += 1;
            return;
        }
        if state.context.read(&mut state.scratch).is_err() {
            state.refused += 1;
            return;
        }
        if matches!(state.scratch.phase, GuestPhase::Inactive | GuestPhase::Park) {
            state.refused += 1;
            return;
        }
        let regs = &*(registers.cast::<libc::ucontext_t>());
        let mut sample = Sample {
            guest: [0; GuestContext::DEPTH],
            guest_len: state.scratch.frames.len(),
            native: [0; NATIVE_DEPTH],
            native_len: 1,
            truncated_guest: state.scratch.truncated(),
            truncated_native: false,
            phase: state.scratch.phase,
        };
        for (slot, frame) in sample.guest.iter_mut().zip(&state.scratch.frames) {
            *slot = u64::from(frame.code) | (u64::from(frame.statement) << 32);
        }
        sample.native[0] = regs.uc_mcontext.gregs[libc::REG_RIP as usize] as u64;
        let mut frame = regs.uc_mcontext.gregs[libc::REG_RBP as usize] as usize;
        let sp = regs.uc_mcontext.gregs[libc::REG_RSP as usize] as usize;
        while frame >= sp && frame.is_multiple_of(8) && frame <= state.stack_end.saturating_sub(16)
        {
            let mut pair = [0usize; 2];
            let local = libc::iovec {
                iov_base: pair.as_mut_ptr().cast(),
                iov_len: 16,
            };
            let remote = libc::iovec {
                iov_base: frame as *mut libc::c_void,
                iov_len: 16,
            };
            // The kernel bounds-checks this copy. Invalid frame chains terminate
            // rather than dereferencing arbitrary interrupted stack contents.
            if libc::syscall(
                libc::SYS_process_vm_readv,
                libc::getpid(),
                &local,
                1,
                &remote,
                1,
                0,
            ) != 16
            {
                sample.truncated_native = true;
                break;
            }
            if sample.native_len == NATIVE_DEPTH {
                sample.truncated_native = true;
                break;
            }
            if pair[1] == 0 {
                break;
            }
            sample.native[sample.native_len] = pair[1].saturating_sub(1) as u64;
            sample.native_len += 1;
            if pair[0] == 0 {
                frame = 0;
                break;
            }
            if pair[0] <= frame {
                sample.truncated_native = true;
                break;
            }
            frame = pair[0];
        }
        if frame != 0
            && (frame < sp
                || frame > state.stack_end.saturating_sub(16)
                || !frame.is_multiple_of(8))
        {
            sample.truncated_native = true;
        }
        state.samples.push(sample);
    }
}

struct Sampler {
    state: Box<UnsafeCell<State>>,
    timer: Option<libc::timer_t>,
    signal: i32,
    old_action: libc::sigaction,
    old_mask: libc::sigset_t,
}
fn os_check(result: i32) -> Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}
impl Sampler {
    fn start(context: Arc<GuestContext>) -> Result<Self> {
        if ACTIVE.swap(true, Ordering::SeqCst) {
            return Err("cell profiler already active".into());
        }
        let result = Self::install(context);
        if result.is_err() {
            ACTIVE.store(false, Ordering::SeqCst);
        }
        result
    }
    fn install(context: Arc<GuestContext>) -> Result<Self> {
        // SAFETY: initialized OS structs, checked return codes and owner-thread
        // signal masking. Error paths restore every installed resource.
        unsafe {
            let signal = libc::SIGRTMAX() - 4;
            let mut attr: libc::pthread_attr_t = std::mem::zeroed();
            os_check(libc::pthread_getattr_np(
                libc::pthread_self(),
                &raw mut attr,
            ))?;
            let mut stack = std::ptr::null_mut();
            let mut size = 0;
            let status = libc::pthread_attr_getstack(&attr, &raw mut stack, &raw mut size);
            libc::pthread_attr_destroy(&raw mut attr);
            os_check(status)?;
            let state = Box::new(UnsafeCell::new(State {
                context,
                scratch: GuestSample::new(),
                samples: Vec::with_capacity(SAMPLE_LIMIT),
                stack_end: stack as usize + size,
                refused: 0,
                overflow: 0,
                overruns: 0,
                timer: std::ptr::null_mut(),
                counter: InstructionCounter::open_inherited()?,
                handler_instructions: 0,
                counter_failures: 0,
            }));
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, signal);
            let mut old_mask = std::mem::zeroed();
            os_check(libc::pthread_sigmask(
                libc::SIG_BLOCK,
                &set,
                &raw mut old_mask,
            ))?;
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = interrupt as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            libc::sigemptyset(&raw mut action.sa_mask);
            let mut old_action = std::mem::zeroed();
            if let Err(error) = os_check(libc::sigaction(signal, &action, &raw mut old_action)) {
                libc::pthread_sigmask(libc::SIG_SETMASK, &old_mask, std::ptr::null_mut());
                return Err(error);
            }
            let mut event: libc::sigevent = std::mem::zeroed();
            event.sigev_notify = libc::SIGEV_THREAD_ID;
            event.sigev_signo = signal;
            event.sigev_notify_thread_id = libc::syscall(libc::SYS_gettid) as i32;
            event.sigev_value.sival_ptr = state.get().cast();
            let mut timer = std::mem::zeroed();
            if let Err(error) = os_check(libc::timer_create(
                libc::CLOCK_THREAD_CPUTIME_ID,
                &mut event,
                &raw mut timer,
            )) {
                libc::sigaction(signal, &old_action, std::ptr::null_mut());
                libc::pthread_sigmask(libc::SIG_SETMASK, &old_mask, std::ptr::null_mut());
                return Err(error);
            }
            (*state.get()).timer = timer;
            let sampler = Self {
                state,
                timer: Some(timer),
                signal,
                old_action,
                old_mask,
            };
            let interval = libc::timespec {
                tv_sec: 0,
                tv_nsec: 10_000_000,
            };
            let spec = libc::itimerspec {
                it_interval: interval,
                it_value: interval,
            };
            os_check(libc::timer_settime(timer, 0, &spec, std::ptr::null_mut()))?;
            os_check(libc::pthread_sigmask(
                libc::SIG_UNBLOCK,
                &set,
                std::ptr::null_mut(),
            ))?;
            Ok(sampler)
        }
    }
    fn stop(&mut self) {
        let Some(timer) = self.timer.take() else {
            return;
        };
        // SAFETY: same thread owns timer and signal disposition. Block before
        // deleting, drain queued timer signals, then restore disposition/mask.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, self.signal);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            libc::timer_delete(timer);
            let zero = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            while libc::sigtimedwait(&set, std::ptr::null_mut(), &zero) >= 0 {}
            libc::sigaction(self.signal, &self.old_action, std::ptr::null_mut());
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.old_mask, std::ptr::null_mut());
        }
        ACTIVE.store(false, Ordering::SeqCst);
    }
}
impl Drop for Sampler {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Serialize)]
struct Captured<'a> {
    guest: &'a [u64],
    native: &'a [u64],
    phase: String,
    truncated_guest: bool,
    truncated_native: bool,
}

/// Profile one file, preparing outside the measured interval, into raw.json.
///
/// # Errors
/// Source admission, non-successful execution, unavailable PMU or signal timer,
/// insufficient PMU running time, and output I/O failures.
#[expect(
    clippy::disallowed_methods,
    reason = "developer profile entry reads caller-selected source and creates caller-selected output"
)]
pub fn run(file: &Path, out: &Path) -> Result<()> {
    let source = std::fs::read_to_string(file)?;
    let kernel = super::compare::Kernel::new()?;
    let (program, _) = kernel.prepare(&source, file.extension().is_some_and(|s| s == "py"))?;
    std::fs::create_dir_all(out)?;
    super::compare::profile(&program, file, &source, out)
}

pub(super) struct Source<'a> {
    pub file: &'a Path,
    pub text: &'a str,
    pub map: &'a lash_kernel_doc::SourceMap,
    pub document: &'a lash_kernel_doc::Document,
    pub written: &'a std::collections::BTreeMap<
        lash_kernel_value::sites::Site,
        lash_kernel_dialect::Written,
    >,
}

#[expect(
    clippy::disallowed_methods,
    reason = "developer profile writes evidence and records its executable and Linux mappings"
)]
pub(super) fn capture(
    machine: &mut lash_kernel_vm::KernelMachine,
    source: Source<'_>,
    out: &Path,
    drive: impl FnOnce(&mut lash_kernel_vm::KernelMachine) -> Result<Vec<lash_kernel_doc::Datum>>,
) -> Result<()> {
    let Source {
        file,
        text: source,
        map: source_map,
        document,
        written,
    } = source;
    use lash_kernel_vm::Machine;
    let context = GuestContext::new();
    machine.publish_context(Arc::clone(&context));
    let mut sampler = Sampler::start(context)?;
    let counter = InstructionCounter::open_inherited()?;
    let before = counter.snapshot()?;
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let at = std::time::Instant::now();
    let result = drive(machine);
    let wall = at.elapsed().as_secs_f64();
    COUNTING.store(false, Ordering::Relaxed);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - allocations;
    let instructions = counter.snapshot()?.since(before)?;
    sampler.stop();
    let result = result?;
    std::fs::write(out.join("result.txt"), format!("{result:?}\n"))?;
    let symbols = machine.guest_symbols();
    let codes = symbols
        .codes
        .iter()
        .map(|symbol| {
            let start = source_start(source_map, &symbol.site);
            let line = start.map(|at| source[..at].bytes().filter(|b| *b == b'\n').count() + 1);
            let name = authored_name(&symbol.site, written, document);
        serde_json::json!({"token":symbol.token,"name":symbol.name,"authored_name":name,"site":symbol.site,
                           "offset":start,"line":line})
        })
        .collect::<Vec<_>>();
    let statements = symbols
        .statements
        .iter()
        .map(|site| {
            site.as_ref()
                .and_then(|s| source_map.source_start(s))
                .map(|at| source[..at].bytes().filter(|b| *b == b'\n').count() + 1)
        })
        .collect::<Vec<_>>();
    // SAFETY: stop blocked, deleted and drained the timer before this read.
    // UnsafeCell keeps the signal's mutation separate from the owner's Box.
    let state = unsafe { &*sampler.state.get() };
    let samples = state
        .samples
        .iter()
        .map(|s| Captured {
            guest: &s.guest[..s.guest_len],
            native: &s.native[..s.native_len],
            phase: format!("{:?}", s.phase),
            truncated_guest: s.truncated_guest,
            truncated_native: s.truncated_native,
        })
        .collect::<Vec<_>>();
    let libs = symbols
        .libs
        .iter()
        .map(|s| serde_json::json!({"token":s.token,"name":s.name}))
        .collect::<Vec<_>>();
    let raw = serde_json::json!({"file":file,"source":source,"source_map":source_map,
        "pid":std::process::id(),"executable":std::env::current_exe()?,"maps":std::fs::read_to_string("/proc/self/maps")?,
        "sample_hz":100,"window":"run to successful terminal; prepare/start/drop excluded",
        "instructions":instructions,"running_fraction":instructions.running_fraction(),
        "allocations":allocations,"charge":machine.meters().charged,"wall_seconds":wall,
        "codes":codes,"statements":statements,"libs":libs,"samples":samples,
        "refused":state.refused,"overflow":state.overflow,"overruns":state.overruns,
        "sampler_instructions":state.handler_instructions,"sampler_counter_failures":state.counter_failures,
        "guest_accuracy":"frame transitions in quiet register execution; statements in source continuations"});
    std::fs::write(out.join("raw.json"), serde_json::to_vec(&raw)?)?;
    if state.counter_failures > 0 {
        return Err(
            "sampler instruction counter read or PMU residency failed; inspect raw.json".into(),
        );
    }
    if state.overflow > 0 || state.overruns > 0 {
        return Err("CPU sample capacity/schedule lost samples; inspect raw.json".into());
    }
    if state.samples.is_empty() {
        return Err("no accepted CPU samples; use a longer cell".into());
    }
    Ok(())
}

pub(super) fn authored_name<'a>(
    site: &lash_kernel_value::sites::Site,
    written: &'a std::collections::BTreeMap<
        lash_kernel_value::sites::Site,
        lash_kernel_dialect::Written,
    >,
    document: &lash_kernel_doc::Document,
) -> Option<&'a str> {
    let name = |written: &'a lash_kernel_dialect::Written| {
        written
            .signature_origin
            .as_ref()
            .and_then(|origin| origin.name.as_deref())
    };
    if let Some(found) = written.get(site).and_then(name) {
        return Some(found);
    }
    let lash_kernel_value::sites::Unit::Library(function) = &site.unit else {
        return None;
    };
    written.iter().find_map(|(creation, written)| {
        let lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Closure(closure)) =
            document.node(creation)?
        else {
            return None;
        };
        (closure.function == *function)
            .then(|| name(written))
            .flatten()
    })
}

pub(super) fn source_start(
    map: &lash_kernel_doc::SourceMap,
    site: &lash_kernel_value::sites::Site,
) -> Option<usize> {
    map.source_start(site)
        .or_else(|| {
            map.sites
                .iter()
                .filter(|entry| {
                    entry.site.unit == site.unit && entry.site.path.starts_with(&site.path)
                })
                .map(|entry| entry.location.start())
                .min()
        })
        .or_else(|| matches!(site.unit, lash_kernel_value::sites::Unit::Main).then_some(0))
}
