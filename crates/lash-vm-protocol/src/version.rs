//! Live worker compatibility. Crate versions are diagnostic only.
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Wire shapes change in place at version 1 until the 1.0 freeze ends.
///
/// version_guard(
///     shapes(
///         path = "crates/lash-vm-protocol/src/*.rs",
///         cover(
///             MessageHeader, Start, EffectRequest, EffectResponse, ParentMessage, WorkerMessage,
///             ParentFrame, WorkerFrame, VmContract,
///         ),
///     ),
/// )
#[cfg(not(feature = "synthetic-next"))]
pub const WORKER_PROTOCOL_VERSION: u32 = 1;
/// Acceptance builds advertise N+1 before admitting any guest work.
#[cfg(feature = "synthetic-next")]
pub const WORKER_PROTOCOL_VERSION: u32 = 2;
/// Oldest worker wire version the parent can read.
pub const MIN_SUPPORTED_WORKER_PROTOCOL_VERSION: u32 = WORKER_PROTOCOL_VERSION;

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_equal_despite_different_crate_versions() {
        assert_eq!(
            check_worker_protocol_version(WORKER_PROTOCOL_VERSION, "0.0.0-dev", "9.8.7"),
            Ok(())
        );
    }
    #[test]
    fn synthetic_next_moves_the_entire_admission_window() {
        assert_eq!(
            WORKER_PROTOCOL_VERSION,
            1 + u32::from(cfg!(feature = "synthetic-next"))
        );
        assert_eq!(
            MIN_SUPPORTED_WORKER_PROTOCOL_VERSION,
            WORKER_PROTOCOL_VERSION
        );
        let other = if cfg!(feature = "synthetic-next") {
            1
        } else {
            2
        };
        assert!(check_worker_protocol_version(other, "parent", "worker").is_err());
    }
    #[test]
    fn accepts_every_version_within_a_supported_range() {
        for worker in 2..=4 {
            assert_eq!(check_range(2, 4, worker, "parent", "worker"), Ok(()));
        }
    }
    #[test]
    fn refuses_below_minimum_with_both_versions() {
        let refusal = check_worker_protocol_version(
            MIN_SUPPORTED_WORKER_PROTOCOL_VERSION - 1,
            "parent",
            "worker",
        )
        .expect_err("below minimum");
        assert_eq!(refusal.parent_version, WORKER_PROTOCOL_VERSION);
        assert_eq!(
            refusal.minimum_supported_version,
            MIN_SUPPORTED_WORKER_PROTOCOL_VERSION
        );
        assert_eq!(
            refusal.worker_version,
            MIN_SUPPORTED_WORKER_PROTOCOL_VERSION - 1
        );
        assert_eq!(refusal.parent_crate_version, "parent");
        assert_eq!(refusal.worker_crate_version, "worker");
    }
    #[test]
    fn refuses_above_current_with_both_versions() {
        let refusal =
            check_worker_protocol_version(WORKER_PROTOCOL_VERSION + 1, "parent", "worker")
                .expect_err("above current");
        assert_eq!(refusal.parent_version, WORKER_PROTOCOL_VERSION);
        assert_eq!(refusal.worker_version, WORKER_PROTOCOL_VERSION + 1);
        assert!(
            refusal
                .to_string()
                .contains("parent crate parent, worker crate worker")
        );
    }
}
