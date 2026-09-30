#[expect(
    clippy::disallowed_methods,
    reason = "fault-injection executable deliberately aborts and probes process inheritance"
)]
fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    let fd = args.get(2).and_then(|arg| arg.parse::<i32>().ok());
    let mut started = false;
    let mut cpu_ceiling = None;
    let mut hook = |message: &lash_vm_protocol::ParentMessage| {
        if mode == "cpu_ceiling"
            && matches!(message, lash_vm_protocol::ParentMessage::EffectResponse(_))
        {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            #[expect(unsafe_code, reason = "the fixture observes its own kernel CPU bound")]
            let result = unsafe { libc::getrlimit(libc::RLIMIT_CPU, &mut limit) };
            assert_eq!(result, 0);
            if let Some(previous) = cpu_ceiling {
                assert_eq!(
                    limit.rlim_cur, previous,
                    "effect responses must not renew the checkout CPU bound"
                );
            } else {
                cpu_ceiling = Some(limit.rlim_cur);
                let mut now = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                #[expect(
                    unsafe_code,
                    reason = "the fixture measures CPU it burns to cross a kernel limit second"
                )]
                unsafe {
                    assert_eq!(
                        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut now),
                        0
                    );
                }
                let start = now.tv_sec * 1_000_000_000 + now.tv_nsec;
                loop {
                    std::hint::black_box((0..10000).fold(0_u64, u64::wrapping_add));
                    #[expect(
                        unsafe_code,
                        reason = "the fixture observes its own process CPU clock"
                    )]
                    unsafe {
                        assert_eq!(
                            libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut now),
                            0
                        );
                    }
                    if now.tv_sec * 1_000_000_000 + now.tv_nsec - start > 1_100_000_000 {
                        break;
                    }
                }
            }
        }
        if mode == "abort" && matches!(message, lash_vm_protocol::ParentMessage::EffectResponse(_))
        {
            std::process::abort();
        }
        if matches!(message, lash_vm_protocol::ParentMessage::Reset) {
            if mode == "reset_abort" && started {
                std::process::abort();
            }
            started = false;
        }
        if matches!(message, lash_vm_protocol::ParentMessage::Start(_)) {
            started = true;
        }
        if mode == "hang" && matches!(message, lash_vm_protocol::ParentMessage::Start(_)) {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
        if mode == "probe" {
            assert!(std::env::vars_os().next().is_none());
            if let Some(fd) = fd {
                // SAFETY: F_GETFD only observes the sentinel descriptor.
                #[expect(
                    unsafe_code,
                    reason = "prove a non-CLOEXEC parent descriptor is absent after exec"
                )]
                let result = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                assert_eq!(result, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EBADF)
                );
            }
        }
    };
    match lash_vm_worker::worker_entry_with_hook(lash_vm_worker::build_identity(), &mut hook) {
        Ok(true) => {}
        _ => std::process::exit(1),
    }
}
