//! Native bootstrap, entered before the host initializes credentials.
use crate::PoolError;
use crate::process::inherited_pipe;
use crate::worker::Server;
use lash_vm_client::ipc::Bootstrap;
use lash_vm_protocol::*;

/// Register as the host binary's first action. Returns false for a normal
/// host invocation. The pool always execs with an empty environment.
pub fn worker_entry() -> Result<bool, PoolError> {
    worker_entry_with_frontend(&crate::frontend::TypeScriptFrontend::default())
}

/// Enter the credential-free worker with the host's compiled source frontend.
/// Call this before constructing the host runtime or credentials.
pub fn worker_entry_with_frontend(frontend: &dyn crate::Frontend) -> Result<bool, PoolError> {
    worker_entry_inner(frontend, None)
}

#[cfg(feature = "testing")]
pub fn worker_entry_with_hook(hook: &mut dyn FnMut(&ParentMessage)) -> Result<bool, PoolError> {
    worker_entry_inner(&crate::frontend::TypeScriptFrontend::default(), Some(hook))
}

#[expect(
    clippy::disallowed_methods,
    reason = "early host entry inspects only bootstrap argv and verifies its empty environment"
)]
fn worker_entry_inner(
    frontend: &dyn crate::Frontend,
    mut hook: Option<&mut dyn FnMut(&ParentMessage)>,
) -> Result<bool, PoolError> {
    let args = std::env::args().collect::<Vec<_>>();
    let Some(index) = args.iter().position(|arg| arg == "--lash-vm-worker") else {
        return Ok(false);
    };
    if std::env::vars_os().next().is_some() {
        return Err(PoolError::breach(BootstrapFault::EnvironmentNotEmpty));
    }
    #[cfg(feature = "dhat-heap")]
    crate::heap_profile::start(&args).map_err(PoolError::io)?;
    #[cfg(not(feature = "dhat-heap"))]
    if args.iter().any(|arg| arg == "--heap-profile-dir") {
        return Err(PoolError::io(std::io::Error::other(
            "--heap-profile-dir requires a dhat-heap worker",
        )));
    }
    let fd = args
        .get(index + 1)
        .ok_or_else(|| PoolError::breach(BootstrapFault::MissingDescriptor))?
        .parse::<i32>()
        .map_err(|_| PoolError::breach(BootstrapFault::InvalidDescriptor))?;
    // SAFETY: this is the early re-exec entry. The launcher transfers one
    // socket to this process. No host initialization has run.
    #[expect(
        unsafe_code,
        reason = "entry consumes the launcher's inherited IPC descriptor"
    )]
    let pipe = unsafe { inherited_pipe(fd)? };
    let bootstrap: Bootstrap = serde_json::from_str(
        args.get(index + 2)
            .ok_or_else(|| PoolError::breach(BootstrapFault::MissingBounds))?,
    )
    .map_err(|_| PoolError::breach(BootstrapFault::InvalidBounds))?;
    let codec = FrameCodec::new(DecodeLimits {
        max_frame_bytes: bootstrap.frame,
        max_depth: bootstrap.depth,
        max_nodes: bootstrap.nodes,
        max_allocation_bytes: bootstrap.allocation,
    });
    if args.iter().any(|arg| arg == "--lash-vm-measure") {
        run_server::<true>(pipe, codec, bootstrap, frontend, &mut hook)?;
    } else {
        run_server::<false>(pipe, codec, bootstrap, frontend, &mut hook)?;
    }
    Ok(true)
}

fn run_server<const MEASURE: bool>(
    pipe: std::os::unix::net::UnixStream,
    codec: FrameCodec,
    bootstrap: Bootstrap,
    frontend: &dyn crate::Frontend,
    hook: &mut Option<&mut dyn FnMut(&ParentMessage)>,
) -> Result<(), PoolError> {
    let mut server = Server::<MEASURE>::new(pipe, codec, bootstrap, frontend)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| server.run(hook)))
        .unwrap_or_else(|panic| {
            let reason = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            Err(PoolError::breach(ProtocolBreach::Panicked {
                detail: Detail::new(reason),
            }))
        });
    if let Err(error) = result {
        server.refuse(&error)?;
        return Err(error);
    }
    Ok(())
}
