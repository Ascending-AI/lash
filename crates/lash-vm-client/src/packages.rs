//! Immutable pool-start holdings. This is parent-to-worker transport, not a
//! stored artifact format: packages and set identities are recomputed on read.
use crate::PoolError;
use lash_kernel_package::{HeldPackages, PackageRefusal};
use lash_vm_protocol::{EncodedPayload, FrameCodec, PoolStarted};

/// Reconstructs metadata over the parent's trusted image, its pool-start
/// parts joined. Installation did all definition validation, sorting and
/// reservation work before this worker ran.
pub fn read(holdings: std::sync::Arc<[u8]>) -> Result<HeldPackages, PoolError> {
    HeldPackages::from_frame(holdings).map_err(PoolError::Package)
}

/// The refusal remains a package refusal at the parent boundary.
pub fn result(result: Result<(), PackageRefusal>) -> Result<PoolStarted, PoolError> {
    Ok(PoolStarted {
        result: EncodedPayload(rmp_serde::to_vec_named(&result).map_err(package_payload)?),
    })
}

pub(crate) fn installed(reply: PoolStarted, codec: &FrameCodec) -> Result<(), PoolError> {
    codec.check_payload(&reply.result.0)?;
    let result: Result<(), PackageRefusal> =
        rmp_serde::from_slice(&reply.result.0).map_err(package_payload)?;
    result.map_err(PoolError::Package)
}

fn package_payload(error: impl std::fmt::Display) -> PoolError {
    PoolError::breach(lash_vm_protocol::ProtocolBreach::Machine {
        detail: lash_vm_protocol::Detail::new(error),
    })
}

/// Reads the typed refusal retained by a runtime's `PoolFault::Package`.
/// Hosts can classify `NativeMissing` without parsing a rendered message.
pub fn refusal(bytes: &[u8]) -> Result<PackageRefusal, PoolError> {
    rmp_serde::from_slice(bytes).map_err(package_payload)
}
