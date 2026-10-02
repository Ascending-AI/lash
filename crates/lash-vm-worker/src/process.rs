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
