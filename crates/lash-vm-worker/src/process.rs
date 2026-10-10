use crate::PoolError;
use lash_vm_protocol::BootstrapFault;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;

/// Called only in a freshly exec'd entry, before any host initialization.
#[expect(
    unsafe_code,
    reason = "early exec entry takes exclusive ownership of the inherited socket"
)]
pub(crate) unsafe fn inherited_pipe(fd: i32) -> Result<UnixStream, PoolError> {
    if fd < 3 {
        return Err(PoolError::breach(BootstrapFault::InvalidDescriptor));
    }
    let mut socket_type: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let valid = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if valid != 0 || socket_type != libc::SOCK_STREAM {
        return Err(PoolError::breach(BootstrapFault::InvalidDescriptor));
    }
    let core_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &core_limit) } != 0 {
        return Err(PoolError::io(std::io::Error::last_os_error()));
    }
    #[cfg(target_os = "linux")]
    {
        // close_range avoids a guessed descriptor ceiling, including handles
        // above a subsequently lowered RLIMIT_NOFILE. Linux 5.9 or newer.
        // SAFETY: the entry owns its process; only fd is preserved.
        let first = unsafe { libc::syscall(libc::SYS_close_range, 0_u32, (fd - 1) as u32, 0_u32) };
        let second =
            unsafe { libc::syscall(libc::SYS_close_range, (fd + 1) as u32, u32::MAX, 0_u32) };
        if first != 0 || second != 0 {
            return Err(PoolError::io(std::io::Error::last_os_error()));
        }
    }
    #[cfg(not(target_os = "linux"))]
    return Err(PoolError::UnsupportedPlatform);
    #[cfg(target_os = "linux")]
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

/// Confines this process before it serves (FIG-5858): an address-space
/// ceiling, then a seccomp filter that admits only the system calls a
/// serving worker makes. Both bind every thread, now and later, and neither
/// can be lifted. Called once the embedding is assembled and before the
/// worker reads its first frame, so no guest input meets an unconfined
/// worker.
#[cfg(target_os = "linux")]
pub(crate) fn confine(confinement: &lash_vm_client::WorkerConfinement) -> Result<(), PoolError> {
    let refused = || PoolError::breach(BootstrapFault::Confinement);
    let ceiling = libc::rlimit {
        rlim_cur: confinement.address_space_bytes,
        rlim_max: confinement.address_space_bytes,
    };
    let filter = syscall_filter(std::process::id()).ok_or_else(refused)?;
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).map_err(|_| refused())?,
        filter: filter.as_ptr().cast_mut(),
    };
    // SAFETY: each call reads only the values built above, which outlive
    // it; none of them changes memory this process owns.
    #[expect(
        unsafe_code,
        reason = "the worker lowers its own address-space ceiling and installs its seccomp filter"
    )]
    let installed = unsafe {
        libc::setrlimit(libc::RLIMIT_AS, &ceiling) == 0
            && libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1_u64, 0_u64, 0_u64, 0_u64) == 0
            && libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                libc::SECCOMP_FILTER_FLAG_TSYNC,
                &program,
            ) == 0
    };
    if installed { Ok(()) } else { Err(refused()) }
}

/// The audit architecture the filter admits; any other ABI (x32, or i386
/// through `int 0x80`) is killed.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: Option<u32> = Some(0xC000_003E);
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: Option<u32> = Some(0xC000_00B7);
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const AUDIT_ARCH: Option<u32> = None;

/// Offsets into `struct seccomp_data`: the call number, the architecture,
/// and the low word of each argument (both admitted targets are little
/// endian, and every argument checked below is a 32-bit value).
const NR: u32 = 0;
const ARCH: u32 = 4;
const fn arg(index: u32) -> u32 {
    16 + 8 * index
}

const fn load(offset: u32) -> libc::sock_filter {
    statement(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, offset)
}
const fn and(mask: u32) -> libc::sock_filter {
    statement(libc::BPF_ALU | libc::BPF_AND | libc::BPF_K, mask)
}
const fn ret(action: u32) -> libc::sock_filter {
    statement(libc::BPF_RET | libc::BPF_K, action)
}
const fn statement(code: u32, k: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    }
}
/// Falls through when the accumulator is `k`, else skips `skip` statements.
const fn unless_equal(k: u32, skip: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt: 0,
        jf: skip,
        k,
    }
}

const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;
const KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;
const fn errno(code: i32) -> u32 {
    libc::SECCOMP_RET_ERRNO | (code as u32 & libc::SECCOMP_RET_DATA)
}

/// The worker's allowlist, measured by tracing workers through the pool
/// laws and Python-dialect workers (FIG-5858): the calls a serving worker
/// and its threads made after bootstrap, and nothing for files, sockets,
/// processes or credentials. A call that is not listed kills the process.
///
/// Two calls are refused rather than fatal, because the C library tries
/// them and has a fallback: `openat` (the allocator probes
/// `/proc/sys/vm/overcommit_memory` before trimming a thread's arena) and
/// `clone3` (thread creation falls back to `clone`, whose flags a filter can
/// read).
fn syscall_filter(pid: u32) -> Option<Vec<libc::sock_filter>> {
    let own_process = [load(arg(0)), unless_equal(pid, 1), ret(ALLOW), ret(KILL)];
    // Memory may be writable or executable, never both at once (W^X).
    let write_exec = (libc::PROT_WRITE | libc::PROT_EXEC) as u32;
    let not_write_exec = [
        load(arg(2)),
        and(write_exec),
        unless_equal(write_exec, 1),
        ret(KILL),
        ret(ALLOW),
    ];
    // A thread of this process, never a new process.
    let thread = (libc::CLONE_VM | libc::CLONE_THREAD) as u32;
    let new_thread = [
        load(arg(0)),
        and(thread),
        unless_equal(thread, 1),
        ret(ALLOW),
        ret(KILL),
    ];
    let thread_name = [
        load(arg(0)),
        unless_equal(libc::PR_SET_NAME as u32, 1),
        ret(ALLOW),
        ret(KILL),
    ];
    // The worker's own CPU ceiling, which it sets when it first computes.
    let cpu_ceiling = [
        load(arg(0)),
        unless_equal(0, 3),
        load(arg(1)),
        unless_equal(libc::RLIMIT_CPU, 1),
        ret(ALLOW),
        ret(KILL),
    ];
    let allow = [ret(ALLOW)];
    let eacces = [ret(errno(libc::EACCES))];
    let enosys = [ret(errno(libc::ENOSYS))];
    // An instrumented worker writes its heap profile when it ends.
    let open: &[libc::sock_filter] = if cfg!(feature = "dhat-heap") {
        &allow
    } else {
        &eacces
    };
    let rules: [(libc::c_long, &[libc::sock_filter]); 40] = [
        // The parent's socket.
        (libc::SYS_recvfrom, &allow),
        (libc::SYS_sendto, &allow),
        (libc::SYS_read, &allow),
        (libc::SYS_write, &allow),
        (libc::SYS_setsockopt, &allow),
        (libc::SYS_getsockopt, &allow),
        (libc::SYS_fcntl, &allow),
        (libc::SYS_close, &allow),
        (libc::SYS_openat, open),
        // Memory.
        (libc::SYS_futex, &allow),
        (libc::SYS_brk, &allow),
        (libc::SYS_mmap, &not_write_exec),
        (libc::SYS_munmap, &allow),
        (libc::SYS_mremap, &allow),
        (libc::SYS_mprotect, &not_write_exec),
        (libc::SYS_madvise, &allow),
        // Time, entropy and the CPU ceiling.
        (libc::SYS_clock_gettime, &allow),
        (libc::SYS_clock_getres, &allow),
        (libc::SYS_gettimeofday, &allow),
        (libc::SYS_clock_nanosleep, &allow),
        (libc::SYS_nanosleep, &allow),
        (libc::SYS_getrandom, &allow),
        (libc::SYS_prlimit64, &cpu_ceiling),
        // Threads: the parser's stack, and their ends.
        (libc::SYS_clone, &new_thread),
        (libc::SYS_clone3, &enosys),
        (libc::SYS_set_robust_list, &allow),
        (libc::SYS_rseq, &allow),
        (libc::SYS_prctl, &thread_name),
        (libc::SYS_sched_getaffinity, &allow),
        (libc::SYS_sched_yield, &allow),
        (libc::SYS_gettid, &allow),
        (libc::SYS_getpid, &allow),
        (libc::SYS_exit, &allow),
        (libc::SYS_exit_group, &allow),
        // Signals, and an abort's own.
        (libc::SYS_sigaltstack, &allow),
        (libc::SYS_rt_sigprocmask, &allow),
        (libc::SYS_rt_sigaction, &allow),
        (libc::SYS_rt_sigreturn, &allow),
        (libc::SYS_restart_syscall, &allow),
        (libc::SYS_tgkill, &own_process),
    ];
    let mut filter = vec![
        load(ARCH),
        unless_equal(AUDIT_ARCH?, 1),
        statement(libc::BPF_JMP | libc::BPF_JA, 1),
        ret(KILL),
        load(NR),
    ];
    for (call, body) in rules {
        filter.push(unless_equal(
            u32::try_from(call).ok()?,
            u8::try_from(body.len()).ok()?,
        ));
        filter.extend_from_slice(body);
    }
    filter.push(ret(KILL));
    Some(filter)
}
