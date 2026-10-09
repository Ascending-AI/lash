//! Live worker compatibility. Crate versions are diagnostic only.
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Wire shapes change in place at version 1 until the 1.0 freeze ends.
///
/// version_guard(
///     shapes(
///         path = "crates/lash-vm-protocol/src/*.rs",
///         cover(
///             MessageHeader, Start, StartFrom, RunBounds, RunMeters, ParentMessage, WorkerMessage,
///             ParentFrame, WorkerFrame,
///         ),
///     ),
///     roots(path = "crates/lash-sansio/src/compat.rs", VersionRange),
///     items(
///         path = "crates/lash-vm-protocol/src/codec.rs", FRAME_MAGIC, FRAME_HEADER_BYTES,
///         encode_parent, encode_worker, decode_parent, decode_worker, frame_len,
///     ),
///     shapes(
///         path = "crates/lash-sansio/src/worker_limit.rs", cover(WorkerLimit, WorkerFrameKind),
///     ),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "coexist"
/// format_outside_manifest = "live worker admission; not a parked VM format"
pub const WORKER_PROTOCOL_VERSION: u32 = 1;
/// Acceptance builds advertise N+1 before admitting any guest work.
#[cfg(feature = "synthetic-next")]
/// version_surface = "coexist"
/// format_outside_manifest = "live worker admission; not a parked VM format"
pub const WORKER_PROTOCOL_VERSION: u32 = 2;
/// Oldest worker wire version the parent can read.
pub const MIN_SUPPORTED_WORKER_PROTOCOL_VERSION: u32 = WORKER_PROTOCOL_VERSION;

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize, schemars::JsonSchema)]
#[error(
    "worker protocol {worker_version} is outside parent range {minimum_supported_version}..={parent_version} (parent crate {parent_crate_version}, worker crate {worker_crate_version})"
)]
pub struct ProtocolVersionRefusal {
    pub parent_version: u32,
    pub minimum_supported_version: u32,
    pub worker_version: u32,
    pub parent_crate_version: String,
    pub worker_crate_version: String,
}

pub fn check_worker_protocol_version(
    worker_version: u32,
    parent_crate_version: &str,
    worker_crate_version: &str,
) -> Result<(), ProtocolVersionRefusal> {
    check_range(
        MIN_SUPPORTED_WORKER_PROTOCOL_VERSION,
        WORKER_PROTOCOL_VERSION,
        worker_version,
        parent_crate_version,
        worker_crate_version,
    )
}

fn check_range(
    minimum: u32,
    current: u32,
    worker: u32,
    parent_crate: &str,
    worker_crate: &str,
) -> Result<(), ProtocolVersionRefusal> {
    if (minimum..=current).contains(&worker) {
        Ok(())
    } else {
        Err(ProtocolVersionRefusal {
            parent_version: current,
            minimum_supported_version: minimum,
            worker_version: worker,
            parent_crate_version: parent_crate.to_owned(),
            worker_crate_version: worker_crate.to_owned(),
        })
    }
}
