#![expect(
    clippy::expect_used,
    reason = "fault-injection fixture requires its declared bootstrap inputs"
)]
#[expect(
    clippy::disallowed_methods,
    reason = "fault-injection executable deliberately aborts and probes process inheritance"
)]
fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    let fd = args.get(2).and_then(|arg| arg.parse::<i32>().ok());
    if matches!(
        mode,
        "protocol_below" | "protocol_above" | "opposite_generation" | "crate_version"
    ) {
        use lash_vm_protocol::*;
        use std::io::Write;
        use std::os::fd::FromRawFd;
        let index = args
            .iter()
            .position(|arg| arg == "--lash-vm-worker")
            .expect("worker entry");
        let socket = args[index + 1].parse::<i32>().expect("socket");
        #[expect(
            unsafe_code,
            reason = "fixture consumes the socket passed by its owning launcher"
        )]
        let mut pipe = unsafe { std::os::unix::net::UnixStream::from_raw_fd(socket) };
        let protocol_version = match mode {
            "protocol_below" => MIN_SUPPORTED_WORKER_PROTOCOL_VERSION - 1,
            "protocol_above" => WORKER_PROTOCOL_VERSION + 1,
            "opposite_generation" => {
                if cfg!(feature = "synthetic-next") {
                    1
                } else {
                    2
                }
            }
            _ => WORKER_PROTOCOL_VERSION,
        };
        let mut fence = MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0));
        let frame = WorkerFrame {
            header: fence.next_header(),
            message: WorkerMessage::Ready {
                protocol_version,
                crate_version: "9.8.7-diagnostic-only".into(),
            },
        };
        let bytes = FrameCodec::new(DecodeLimits::standard())
            .encode_worker(&frame)
            .expect("frame");
        pipe.write_all(&bytes).expect("handshake");
        return;
    }
    let mut started = false;
    let mut cpu_ceiling = None;
    let mut hook = |message: &lash_vm_protocol::ParentMessage| {
        #[cfg(target_os = "linux")]
        if let lash_vm_protocol::ParentMessage::EffectResponse(response) = message {
            let phase = match (mode, &response.outcome) {
                (
                    "native_oom_compute",
                    lash_vm_protocol::EffectOutcome::Checkpoint { cancelled: false },
                ) => Some("compute_before_effect_dispatch"),
                ("native_oom_recorded", lash_vm_protocol::EffectOutcome::Value(_)) => {
                    Some("recorded_effect_before_delivery")
                }
                _ => None,
            };
            if let Some(phase) = phase {
                assert!(
                    started,
                    "allocation failure belongs to a running computation"
                );
                native_allocation_failure(args.get(2).expect("allocation witness path"), phase);
            }
        }
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
    match lash_vm_worker::worker_entry_with_hook(&mut hook) {
        Ok(true) => {}
        _ => std::process::exit(1),
    }
}

#[cfg(target_os = "linux")]
#[expect(
    clippy::disallowed_methods,
    reason = "the child fault fixture records its allocator refusal before aborting"
)]
fn native_allocation_failure(path: &str, phase: &str) {
    use std::alloc::{Layout, handle_alloc_error};
    use std::io::Write;

    let mut witness = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return,
        Err(error) => panic!("allocation witness: {error}"),
    };
    let layout = Layout::from_size_align(16 * 1024 * 1024, 16).expect("bounded allocation");
    let receipt = format!(
        "{{\"phase\":\"{phase}\",\"pid\":{},\"requested_bytes\":{},\"address_space_ceiling_bytes\":0,\"allocation_failed\":true,\"errno\":{}}}",
        std::process::id(),
        layout.size(),
        libc::ENOMEM,
    );
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: this is the disposable child. The ceiling prevents new mappings,
    // preserving existing memory, and cannot change the parent's limits.
    #[expect(
        unsafe_code,
        reason = "test-only memory ceiling forces a bounded child allocation to fail without exhausting the host"
    )]
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_AS, &mut limit), 0);
        limit.rlim_cur = 0;
        assert_eq!(libc::setrlimit(libc::RLIMIT_AS, &limit), 0);
        let pointer = libc::mmap(
            std::ptr::null_mut(),
            layout.size(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        let errno = std::io::Error::last_os_error().raw_os_error();
        if pointer != libc::MAP_FAILED {
            libc::munmap(pointer, layout.size());
            panic!("the child ceiling did not refuse its native allocation");
        }
        assert_eq!(errno, Some(libc::ENOMEM));
    }
    witness
        .write_all(receipt.as_bytes())
        .expect("allocation failure evidence");
    witness
        .sync_all()
        .expect("retain allocation failure evidence");
    handle_alloc_error(layout);
}
