//! Native bootstrap, entered before the host initializes credentials.
use crate::PoolError;
use crate::embedding::{EmbedError, Embedder, Embedding};
use crate::process::inherited_pipe;
use crate::worker::Server;
use lash_vm_client::ipc::{Bootstrap, read_frame, write_frame};
use lash_vm_protocol::*;
use std::time::Instant;

/// Enter the credential-free worker with the host's configuration: the
/// dialects it names and the natives of its own packages, assembled once
/// the process knows it is a worker and bound to the packages its parent
/// holds. Register it as the host binary's first action, before
/// constructing the host runtime or credentials. Returns false for a normal
/// host invocation. The pool always execs with an empty environment.
///
/// There is no embedding a host gets without choosing (FIG-5990): `embed`
/// configures one with [`crate::Embedder`], naming each dialect. It names
/// no package (V1).
pub fn worker_entry(embed: &Embed) -> Result<bool, PoolError> {
    worker_entry_inner(embed, None)
}

/// Configures the worker's natives and dialects, never its packages.
pub type Embed = dyn Fn(&lash_vm_client::WorkerTuning) -> Result<Embedder, EmbedError>;

#[cfg(feature = "testing")]
pub fn worker_entry_with_hook(
    embed: &Embed,
    hook: &mut dyn FnMut(&ParentMessage),
) -> Result<bool, PoolError> {
    worker_entry_inner(embed, Some(hook))
}

#[expect(
    clippy::disallowed_methods,
    reason = "early host entry inspects only bootstrap argv and verifies its empty environment"
)]
fn worker_entry_inner(
    embed: &Embed,
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
    let _witness = crate::perf_witness::install(&args)?;
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
    let mut pipe = unsafe { inherited_pipe(fd)? };
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
    })
    .with_detail_limit(bootstrap.tuning.diagnostic_bytes);
    // A registry that cannot be assembled is a defect of this build: the
    // worker never becomes ready, and the pool reports its bootstrap.
    let embedder = embed(&bootstrap.tuning).map_err(|error| {
        PoolError::breach(ProtocolBreach::Machine {
            detail: Detail::new(error),
        })
    })?;
    #[cfg(target_os = "linux")]
    crate::process::confine(&bootstrap.confinement)?;
    let mut fence = MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0));
    let ready = codec.encode_worker(&WorkerFrame {
        header: fence.next_header(),
        message: WorkerMessage::Ready {
            protocol_version: WORKER_PROTOCOL_VERSION,
            crate_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
    })?;
    write_frame(&mut pipe, &ready, Instant::now() + bootstrap.serialization)?;
    let deadline = Instant::now() + bootstrap.tuning.parent_wait;
    let mut parts = PoolStartParts::default();
    let holdings = loop {
        let bytes = read_frame(&mut pipe, &codec, deadline)?;
        if let Some(holdings) = parts.push(codec.decode_pool_start(&bytes)?)? {
            break holdings;
        }
    };
    let held = lash_vm_client::packages::read(holdings);
    let embedding = held.and_then(|held| {
        embedder.finish(&held).map_err(|error| match error {
            EmbedError::Package(refusal) => PoolError::Package(refusal),
            other => PoolError::breach(ProtocolBreach::Machine {
                detail: Detail::new(other),
            }),
        })
    });
    let embedding = match embedding {
        Ok(embedding) => embedding,
        Err(PoolError::Package(refusal)) => {
            let reply = lash_vm_client::packages::result(Err(refusal.clone()))?;
            let bytes = codec.encode_pool_started(&reply)?;
            write_frame(&mut pipe, &bytes, Instant::now() + bootstrap.serialization)?;
            return Err(PoolError::Package(refusal));
        }
        Err(error) => return Err(error),
    };
    run_server(pipe, codec, bootstrap, &embedding, &mut hook)?;
    Ok(true)
}

fn run_server(
    pipe: std::os::unix::net::UnixStream,
    codec: FrameCodec,
    bootstrap: Bootstrap,
    embedding: &Embedding,
    hook: &mut Option<&mut dyn FnMut(&ParentMessage)>,
) -> Result<(), PoolError> {
    let mut server = Server::new(pipe, codec, bootstrap, embedding)?;
    server.installed()?;
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
