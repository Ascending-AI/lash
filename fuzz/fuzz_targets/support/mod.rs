use lash_kernel_doc::heap::{LogicalBytes, PageGrant, PageId, PageRef};
use lash_kernel_state::paged::{
    CellTargetKind, CheckpointRoot, EndedRecord, FrozenRef, FrozenSummary, KeyedRoot, RootStatus,
    SavedLimits, SavedMeters, SequenceRoot,
};

pub fn page(byte: u8) -> PageRef {
    PageRef {
        id: PageId([byte; 32]),
        grant: PageGrant([byte; 48]),
    }
}

/// Populate every root-grant site, including the terminal frozen value. The
/// parked form exercises the same trees without the terminal branch.
pub fn root(ended: bool) -> CheckpointRoot {
    let keyed = |byte| KeyedRoot::NonEmpty {
        page: page(byte),
        len: 1,
        cost: LogicalBytes(16),
    };
    let sequence = |byte| SequenceRoot::NonEmpty {
        page: page(byte),
        len: 1,
        cost: LogicalBytes(16),
    };
    CheckpointRoot {
        limits: SavedLimits {
            memory: Some(4096),
            charge: 1000,
        },
        kernel: 1,
        document: lash_vocabulary::DocumentId::from_bytes([1; 32]),
        link: lash_vocabulary::LinkId::from_bytes([2; 32]),
        closure: PageId([3; 32]),
        helper_library: lash_vocabulary::HelperLibraryId::from_bytes([4; 32]),
        owner: PageId([5; 32]),
        target: CellTargetKind::Main,
        functions: keyed(6),
        status: if ended {
            RootStatus::Ended(EndedRecord::Returned(FrozenRef {
                root: page(7),
                summary: FrozenSummary {
                    entities: 1,
                    cost: LogicalBytes(16),
                    data_only: true,
                },
            }))
        } else {
            RootStatus::Parked
        },
        heap: page(8),
        tasks: keyed(9),
        ready: sequence(10),
        waits: keyed(11),
        joins: keyed(12),
        withdrawn: keyed(13),
        unreported: sequence(14),
        reads: keyed(15),
        session: keyed(16),
        host: vec![lash_vocabulary::names::Name::new("input")],
        meters: SavedMeters {
            charged: Some(1),
            waits_issued: 1,
            next_join: 1,
            live_tasks: 1,
        },
    }
}
