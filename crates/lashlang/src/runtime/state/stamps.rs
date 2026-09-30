use lash_core_execution::{FleetFormat, surface_format};

use super::{LASHLANG_SNAPSHOT_VERSION, SnapshotDecodeError};

/// The version stamps a canonical snapshot carries: its own generation and
/// the size schedule its heap was charged under. A heapless snapshot writes
/// no schedule stamp.
///
/// Both are chosen where the snapshot is encoded: a writer stamps what its
/// store's `F` assigns each surface, and the fixed-point read re-encodes at
/// what the wire recorded (FIG-3796, FIG-4262).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotStamps {
    pub(crate) snapshot: u32,
    pub(crate) heap_schedule: u32,
}

impl SnapshotStamps {
    /// What the fleet's writers emit under `fleet_format`.
    pub(crate) fn for_fleet(fleet_format: FleetFormat) -> Self {
        Self {
            snapshot: fleet_format.writer_version(surface_format!(LASHLANG_SNAPSHOT_VERSION)),
            heap_schedule: crate::runtime::heap::size_schedule_writer(fleet_format),
        }
    }

    /// The stamps a stored wire recorded: its snapshot version, already
    /// admitted by the caller, and its heap's size schedule, admitted here
    /// against the schedule surface's read window under `fleet_format`.
    pub(crate) fn recorded(
        snapshot: u32,
        heap_schedule: Option<u32>,
        fleet_format: FleetFormat,
    ) -> Result<Self, SnapshotDecodeError> {
        let heap_schedule = match heap_schedule {
            Some(version) => {
                crate::runtime::heap::admit_size_schedule(version, fleet_format)
                    .map_err(SnapshotDecodeError::InvalidEncoding)?;
                version
            }
            None => crate::runtime::heap::size_schedule_writer(fleet_format),
        };
        Ok(Self {
            snapshot,
            heap_schedule,
        })
    }
}
