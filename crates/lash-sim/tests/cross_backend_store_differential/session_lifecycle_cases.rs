use super::*;

/// An attachment intent recorded before a commit is adopted by that commit's
/// row when it names the turn as its owner.
pub(super) fn attachment_adoption_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::AttachmentAdoption,
        operations: vec![
            StoreOperation::RecordAttachmentWrite,
            StoreOperation::Commit {
                label: "adopt_attachment_in_runtime_commit",
                expected_head_revision: 0,
                graph: append(
                    vec![NodeSpec::new("active-frame", None, "attachment-prefix")],
                    Some("active-frame"),
                ),
                turn_commit: Some(TurnCommitSpec {
                    turn_id: "attachment-adoption",
                }),
                checkpoint: CheckpointSpec::Empty,
                usage: true,
                adopt_attachment: true,
            },
            StoreOperation::PinLeaf,
            StoreOperation::Rewind,
            StoreOperation::ReclaimRetainedEvidence,
            StoreOperation::UnpinLeaf,
        ],
    }
}

/// A deleted session refuses a later admission attempt identically on every
/// backend.
pub(super) fn delete_then_attempt_admission_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::DeleteThenAttemptAdmission,
        operations: vec![
            StoreOperation::DeleteSession,
            StoreOperation::AttemptAdmission,
        ],
    }
}

/// A store handle minted before its session was deleted answers every later
/// operation the same way on every backend.
pub(super) fn stale_handle_after_delete_case() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::StaleHandleAfterDelete,
        operations: vec![
            StoreOperation::EnqueueQueuedWork,
            StoreOperation::CreateHandle {
                handle_alias: "handle-1",
            },
            StoreOperation::DeleteSessionThroughFactory,
            StoreOperation::AdmitOnHandle {
                handle_alias: "handle-1",
            },
            StoreOperation::SaveMetaOnHandle {
                handle_alias: "handle-1",
            },
            StoreOperation::CommitOnHandle {
                handle_alias: "handle-1",
            },
            StoreOperation::ObserveSessionAbsent,
        ],
    }
}
