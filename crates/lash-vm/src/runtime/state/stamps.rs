use lash_core_execution::{FleetFormat, surface_format};

use super::LASH_VM_SNAPSHOT_VERSION;

/// The snapshot version selected by the fleet writer or recorded wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotStamps {
    pub(crate) snapshot: u32,
}

impl SnapshotStamps {
    pub(crate) fn for_fleet(fleet_format: FleetFormat) -> Self {
        Self {
            snapshot: fleet_format.writer_version(surface_format!(LASH_VM_SNAPSHOT_VERSION)),
        }
    }

    pub(crate) fn recorded(snapshot: u32) -> Self {
        Self { snapshot }
    }
}
