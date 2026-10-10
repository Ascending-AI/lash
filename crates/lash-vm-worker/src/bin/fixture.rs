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
    let mut hook = |message: &lash_vm_protocol::ParentMessage| {
        // `abort` dies in the middle of a run: at the first outcome the
        // parent hands back.
        if mode == "abort" && matches!(message, lash_vm_protocol::ParentMessage::Deliver { .. }) {
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
        // A run's first act is a system call the worker's confinement
        // forbids: a socket, or memory both writable and executable.
        if matches!(message, lash_vm_protocol::ParentMessage::Start(_)) {
            #[expect(
                unsafe_code,
                reason = "the fixture makes system calls its confinement must kill"
            )]
            match mode {
                "open_socket" => unsafe {
                    libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
                },
                "map_write_exec" => unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        4096,
                        libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    );
                },
                _ => {}
            }
        }
        // `panic_backtrace` panics as a worker started with RUST_BACKTRACE=1
        // would: the pool clears the environment, so the fixture sets it
        // here, once confined and before the panic hook first reads it.
        if mode == "panic_backtrace" && matches!(message, lash_vm_protocol::ParentMessage::Start(_))
        {
            // SAFETY: the serving thread is the fixture's only thread here.
            #[expect(
                unsafe_code,
                reason = "the fixture asks for a backtrace before it panics"
            )]
            unsafe {
                std::env::set_var("RUST_BACKTRACE", "1");
            }
            panic!("a panic with a backtrace requested");
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
