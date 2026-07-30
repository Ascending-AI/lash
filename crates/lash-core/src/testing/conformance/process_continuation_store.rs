//! Cross-backend conformance for substrate-scoped process continuations.

use std::sync::Arc;

use crate::{
    BoundaryReason, PersistedSegmentHandover, ProcessAwaitOutput, ProcessCompletionAuthority,
    ProcessContinuationStore, ProcessInput, ProcessProvenance, ProcessRegistration,
    ProcessRegistry, ProjectionWatermark, RecoveryDisposition, SegmentHandover,
};

pub async fn process_continuation_store(
    registry: Arc<dyn ProcessRegistry>,
    store: Arc<dyn ProcessContinuationStore>,
) {
    let process_id = "continuation-conformance";
    registry
        .register_process(ProcessRegistration::new(
            process_id,
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryDisposition::Rerunnable,
            ProcessProvenance::host(),
        ))
        .await
        .expect("register continuation owner");
    let handover = PersistedSegmentHandover {
        segment_ordinal: 1,
        program_hash: "program-v1".to_string(),
        handover: SegmentHandover {
            reason: BoundaryReason::JournalBudget,
            program_hash: Some("program-v1".to_string()),
            engine_state: vec![1, 2, 3],
        },
    };

    store
        .put_segment_handover(process_id, handover.clone())
        .await
        .expect("persist handover");
    store
        .put_segment_handover(process_id, handover.clone())
        .await
        .expect("identical replay is idempotent");
    assert_eq!(
        store
            .get_segment_handover(process_id, 1)
            .await
            .expect("read handover"),
        Some(handover.clone())
    );
    assert_eq!(
        store
            .latest_segment_handover(process_id)
            .await
            .expect("read latest handover"),
        Some(handover.clone())
    );

    let mut conflicting = handover;
    conflicting.handover.engine_state.push(4);
    assert!(
        store
            .put_segment_handover(process_id, conflicting)
            .await
            .is_err(),
        "same ordinal with different bytes must conflict"
    );

    store
        .delete_segment_handovers(process_id)
        .await
        .expect("delete handovers");
    assert!(
        store
            .latest_segment_handover(process_id)
            .await
            .expect("read after delete")
            .is_none()
    );

    let pruned_process_id = "pruned-continuation-conformance";
    registry
        .register_process(ProcessRegistration::new(
            pruned_process_id,
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryDisposition::ExternallyOwned,
            ProcessProvenance::host(),
        ))
        .await
        .expect("register prunable continuation owner");
    let pruned_handover = PersistedSegmentHandover {
        segment_ordinal: 1,
        program_hash: "pruned-program-v1".to_string(),
        handover: SegmentHandover {
            reason: BoundaryReason::JournalBudget,
            program_hash: Some("pruned-program-v1".to_string()),
            engine_state: vec![8, 1, 1],
        },
    };
    store
        .put_segment_handover(pruned_process_id, pruned_handover)
        .await
        .expect("persist handover until terminal retention pruning");
    let terminal = registry
        .complete_process(
            pruned_process_id,
            ProcessAwaitOutput::Success {
                value: serde_json::Value::Null,
                control: None,
            },
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete prunable continuation owner");
    registry
        .prune_terminal_processes(
            terminal.updated_at_ms.saturating_add(1),
            None,
            ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune terminal continuation owner");
    assert!(
        store
            .latest_segment_handover(pruned_process_id)
            .await
            .expect("read handover after terminal prune")
            .is_none(),
        "terminal process pruning must remove its retained handovers"
    );
}
