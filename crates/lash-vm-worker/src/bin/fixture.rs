#[expect(
    clippy::disallowed_methods,
    reason = "fault-injection executable deliberately aborts and probes process inheritance"
)]
fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    let fd = args.get(2).and_then(|arg| arg.parse::<i32>().ok());
    let mut started = false;
    let mut hook = |message: &lash_vm_protocol::ParentMessage| {
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
