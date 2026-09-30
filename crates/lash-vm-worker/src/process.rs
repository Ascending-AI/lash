use crate::PoolError;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;

/// Called only in a freshly exec'd entry, before any host initialization.
#[expect(
    unsafe_code,
    reason = "early exec entry takes exclusive ownership of the inherited socket"
)]
pub(crate) unsafe fn inherited_pipe(fd: i32) -> Result<UnixStream, PoolError> {
    if fd < 3 {
        return Err(PoolError::protocol("IPC descriptor must be above stdio"));
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
        return Err(PoolError::protocol(
            "IPC descriptor is not a valid stream socket",
        ));
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
    #[cfg(target_os = "macos")]
    {
        // PROC_PIDLISTFDS enumerates actual open handles, including descriptors
        // above a lowered RLIMIT_NOFILE. The early entry has no host threads.
        let pid = unsafe { libc::getpid() };
        let size =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        if size <= 0 {
            return Err(PoolError::io(std::io::Error::last_os_error()));
        }
        let width = std::mem::size_of::<libc::proc_fdinfo>();
        let mut slots = (size as usize / width).saturating_add(16);
        loop {
            let mut handles: Vec<libc::proc_fdinfo> = Vec::with_capacity(slots);
            let bytes = slots
                .checked_mul(width)
                .and_then(|n| i32::try_from(n).ok())
                .ok_or(PoolError::InvalidConfiguration)?;
            let read = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDLISTFDS,
                    0,
                    handles.as_mut_ptr().cast(),
                    bytes,
                )
            };
            if read <= 0 || read as usize % width != 0 {
                return Err(PoolError::protocol(
                    "cannot enumerate inherited descriptors",
                ));
            }
            if read == bytes {
                slots = slots
                    .checked_mul(2)
                    .ok_or(PoolError::InvalidConfiguration)?;
                continue;
            }
            unsafe {
                handles.set_len(read as usize / width);
            }
            for handle in handles {
                if handle.proc_fd != fd {
                    unsafe {
                        libc::close(handle.proc_fd);
                    }
                }
            }
            break;
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(PoolError::UnsupportedPlatform);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}
