use super::*;

pub(super) fn bodies_then_ref_only() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::CheckpointBodiesThenRefOnly,
        operations: vec![
            StoreOperation::Commit {
                label: "commit_checkpoint_component_bodies",
                expected_head_revision: 0,
                graph: append(
                    vec![NodeSpec::new("active-frame", None, "checkpoint")],
                    Some("active-frame"),
                ),
                turn_commit: Some(TurnCommitSpec {
                    turn_id: "checkpoint-bodies",
                }),
                checkpoint: CheckpointSpec::Bodies,
                usage: true,
                adopt_attachment: false,
            },
            StoreOperation::Commit {
                label: "commit_checkpoint_refs_with_clean_source",
                expected_head_revision: 1,
                graph: append(Vec::new(), Some("active-frame")),
                turn_commit: Some(TurnCommitSpec {
                    turn_id: "checkpoint-refs",
                }),
                checkpoint: CheckpointSpec::PriorRefs,
                usage: false,
                adopt_attachment: false,
            },
            StoreOperation::ColdReopenSession,
        ],
    }
}

pub(super) fn bodies_then_cleared() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::CheckpointBodiesThenCleared,
        operations: vec![
            StoreOperation::Commit {
                label: "commit_checkpoint_bodies_before_clear",
                expected_head_revision: 0,
                graph: append(
                    vec![NodeSpec::new("active-frame", None, "checkpoint-clear")],
                    Some("active-frame"),
                ),
                turn_commit: Some(TurnCommitSpec {
                    turn_id: "checkpoint-bodies-before-clear",
                }),
                checkpoint: CheckpointSpec::Bodies,
                usage: false,
                adopt_attachment: false,
            },
            StoreOperation::Commit {
                label: "commit_checkpoint_with_cleared_execution_state",
                expected_head_revision: 1,
                graph: append(Vec::new(), Some("active-frame")),
                turn_commit: Some(TurnCommitSpec {
                    turn_id: "checkpoint-cleared",
                }),
                checkpoint: CheckpointSpec::ClearedComponents,
                usage: false,
                adopt_attachment: false,
            },
            StoreOperation::ColdReopenSession,
        ],
    }
}
