use super::*;

/// A root admitted under one drive fence settles only under the fence that
/// is live when it ends (FIG-3927). After a handoff the superseded fence's
/// final commit is refused `StaleDriveFence` with no durable write; the
/// successor re-admits the same root, reads back the recorded admission, and
/// its final commit settles the rows.
fn admission_after_handoff(
    name: CaseName,
    enqueue: StoreOperation,
    head: HeadKind,
) -> GeneratedCase {
    GeneratedCase {
        name,
        operations: vec![
            enqueue,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "first-owner",
            },
            StoreOperation::AdmitRoot {
                lease: LeaseSlot::First,
                head,
            },
            StoreOperation::ReleaseSessionLease {
                lease: LeaseSlot::First,
            },
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::Successor,
                owner: "successor-owner",
            },
            StoreOperation::EndRootCompleting {
                lease: LeaseSlot::First,
                expected_head_revision: 0,
            },
            StoreOperation::AdmitRoot {
                lease: LeaseSlot::Successor,
                head,
            },
            StoreOperation::EndRootCompleting {
                lease: LeaseSlot::Successor,
                expected_head_revision: 0,
            },
        ],
    }
}

/// The input half of the handoff law also drives `pending_turn_input`, the
/// keyed point read (FIG-3976): `Open` after the enqueue and `none` for an id
/// no case enqueued, `Admitted` naming the differential root once it is
/// admitted and still after the stale fence's refused settle, and `none` once
/// the successor's commit completes it.
pub(super) fn turn_input_admission_after_handoff() -> GeneratedCase {
    let pending_input = |known| StoreOperation::DriveSurface {
        method: SurfaceMethod::PendingTurnInput { known },
    };
    GeneratedCase {
        name: CaseName::TurnInputAdmissionAfterHandoff,
        operations: vec![
            StoreOperation::EnqueueNextTurnInput,
            pending_input(true),
            pending_input(false),
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "first-owner",
            },
            StoreOperation::AdmitRoot {
                lease: LeaseSlot::First,
                head: HeadKind::Input,
            },
            pending_input(true),
            StoreOperation::ReleaseSessionLease {
                lease: LeaseSlot::First,
            },
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::Successor,
                owner: "successor-owner",
            },
            StoreOperation::EndRootCompleting {
                lease: LeaseSlot::First,
                expected_head_revision: 0,
            },
            // The refused settle moved nothing: the row is still the root's.
            pending_input(true),
            StoreOperation::AdmitRoot {
                lease: LeaseSlot::Successor,
                head: HeadKind::Input,
            },
            StoreOperation::EndRootCompleting {
                lease: LeaseSlot::Successor,
                expected_head_revision: 0,
            },
            // The completing commit made the row terminal: the point read
            // hides it.
            pending_input(true),
        ],
    }
}

pub(super) fn queued_work_admission_after_handoff() -> GeneratedCase {
    admission_after_handoff(
        CaseName::QueuedWorkAdmissionAfterHandoff,
        StoreOperation::EnqueueAdmittableQueuedWork,
        HeadKind::Batch,
    )
}

/// A root that ends handing its admitted batch back leaves it open at its
/// own position, identically on every backend.
pub(super) fn queued_work_admission_released() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::QueuedWorkAdmissionReleased,
        operations: vec![
            StoreOperation::EnqueueAdmittableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "queued-work-owner",
            },
            StoreOperation::AdmitRoot {
                lease: LeaseSlot::First,
                head: HeadKind::Batch,
            },
            StoreOperation::EndRootReleasing {
                lease: LeaseSlot::First,
            },
        ],
    }
}

/// At most one unfinished root per session: a second root's admission is
/// refused identically on every backend, with no durable mutation.
pub(super) fn unfinished_root_refuses_rival() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::UnfinishedRootRefusesRival,
        operations: vec![
            StoreOperation::EnqueueAdmittableQueuedWork,
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "queued-work-owner",
            },
            StoreOperation::AdmitRoot {
                lease: LeaseSlot::First,
                head: HeadKind::Batch,
            },
            StoreOperation::AdmitRivalRoot {
                lease: LeaseSlot::First,
            },
        ],
    }
}

/// A settling commit's identity names its batches, and each backend mints
/// its own batch ids, so the stored `turn_commit_hash` differs by backend
/// for the same logical commit. Returns the hash `commit` will store paired
/// with the hash of the same commit whose batch ids are replaced, in
/// settlement order, by backend-neutral aliases; `None` when the commit
/// names no batch. The observation compares the neutral hash, and only for
/// a stored row whose hash is the one this commit computed.
#[expect(
    clippy::expect_used,
    reason = "test support: the harness's own commit must hash; a refusal panics the harness by design"
)]
pub(super) fn backend_neutral_commit_hash(commit: &RuntimeCommit) -> Option<(String, String)> {
    let mut neutral = commit.clone();
    let ingress = neutral.ingress.as_mut()?;
    let mut aliases = BTreeMap::<lash_core::BatchId, lash_core::BatchId>::new();
    let mut alias = |batch: &mut lash_core::BatchId| {
        let next = aliases.len();
        *batch = aliases
            .entry(batch.clone())
            .or_insert_with(|| lash_core::BatchId::from(format!("differential-batch#{next}")))
            .clone();
    };
    for completion in &mut ingress.completed_batches {
        completion.batch_ids.iter_mut().for_each(&mut alias);
    }
    for row in ingress
        .released
        .iter_mut()
        .chain(ingress.dropped.iter_mut())
    {
        if let lash_core::store::IngressRowId::Batch(batch) = row {
            alias(batch);
        }
    }
    if aliases.is_empty() {
        return None;
    }
    Some((
        commit
            .turn_commit_hash()
            .expect("hash the differential root's commit"),
        neutral
            .turn_commit_hash()
            .expect("hash the backend-neutral differential commit"),
    ))
}
