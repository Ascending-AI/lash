//! Native bootstrap, entered before the host initializes credentials.
use crate::PoolError;
use crate::process::{Bootstrap, inherited_pipe};
use crate::worker::Server;
use lash_vm_protocol::*;

/// Register as the host binary's first action. Returns false for a normal
/// host invocation. Worker argv never supplies the build identity: `build`
/// belongs to the compiled host. The pool always execs with an empty env.
pub fn worker_entry(build: BuildIdentity) -> Result<bool, PoolError> {
    worker_entry_inner(build, None)
}

#[cfg(feature = "testing")]
pub fn worker_entry_with_hook(
    build: BuildIdentity,
    hook: &mut dyn FnMut(&ParentMessage),
) -> Result<bool, PoolError> {
    worker_entry_inner(build, Some(hook))
}

#[expect(
    clippy::disallowed_methods,
    reason = "early host entry inspects only bootstrap argv and verifies its empty environment"
)]
fn worker_entry_inner(
    build: BuildIdentity,
    mut hook: Option<&mut dyn FnMut(&ParentMessage)>,
) -> Result<bool, PoolError> {
    let args = std::env::args().collect::<Vec<_>>();
    let Some(index) = args.iter().position(|arg| arg == "--lash-vm-worker") else {
        return Ok(false);
    };
    if std::env::vars_os().next().is_some() {
        return Err(PoolError::protocol("worker environment is not empty"));
    }
    let fd = args
        .get(index + 1)
        .ok_or_else(|| PoolError::protocol("missing IPC descriptor"))?
        .parse::<i32>()
        .map_err(PoolError::protocol)?;
    // SAFETY: this is the early re-exec entry. The launcher transfers one
    // socket to this process. No host initialization has run.
    #[expect(
        unsafe_code,
        reason = "entry consumes the launcher's inherited IPC descriptor"
    )]
    let pipe = unsafe { inherited_pipe(fd)? };
    let bootstrap: Bootstrap = serde_json::from_str(
        args.get(index + 2)
            .ok_or_else(|| PoolError::protocol("missing IPC bounds"))?,
    )
    .map_err(PoolError::protocol)?;
    let codec = FrameCodec::new(
        build.clone(),
        DecodeLimits {
            max_frame_bytes: bootstrap.frame,
            max_depth: bootstrap.depth,
            max_nodes: bootstrap.nodes,
            max_allocation_bytes: bootstrap.allocation,
        },
    );
    let mut server = Server::new(pipe, codec, bootstrap, build)?;
    server.run(&mut hook)?;
    Ok(true)
}
