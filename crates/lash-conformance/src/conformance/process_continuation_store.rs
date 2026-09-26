//! Cross-backend conformance for substrate-scoped process continuations.

use pretty_assertions::assert_eq;
use std::sync::Arc;

use crate::{
    BoundaryReason, PersistedSegmentHandover, ProcessAwaitOutput, ProcessCompletionAuthority,
    ProcessContinuationStore, ProcessInput, ProcessProvenance, ProcessRegistration,
    ProcessRegistry, ProjectionWatermark, RecoveryContract, SegmentHandover,
};

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn process_continuation_store(
    registry: Arc<dyn ProcessRegistry>,
    store: Arc<dyn ProcessContinuationStore>,
) {
    let continuation_conformance_record = registry
        .register_process(ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryContract::Rerunnable,
            ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register continuation owner");
    let process_id = continuation_conformance_record.id.clone();
    let handover = PersistedSegmentHandover {
        writer: String::new(),
        segment_ordinal: 1,
        written_generation: Some(lash_core::engine::BuildGeneration::for_test("t0")),
        route: "LashProcessWorkflow".to_string(),
        handover: SegmentHandover {
            reason: BoundaryReason::JournalBudget,
            program_hash: "program-v1".to_string(),
            engine_state: vec![1, 2, 3],
        },
    };

    store
        .put_segment_handover(&process_id, handover.clone())
        .await
        .expect("persist handover");
    store
        .put_segment_handover(&process_id, handover.clone())
        .await
        .expect("identical replay is idempotent");
    assert_eq!(
        store
            .get_segment_handover(&process_id, 1)
            .await
            .expect("read handover"),
        Some(handover.clone())
    );
    assert_eq!(
        store
            .latest_segment_handover(&process_id)
            .await
            .expect("read latest handover"),
        Some(handover.clone())
    );

    let mut conflicting = handover;
    conflicting.handover.engine_state.push(4);
    assert!(
        store
            .put_segment_handover(&process_id, conflicting)
            .await
            .is_err(),
        "same ordinal with different bytes must conflict"
    );

    // A writer's own retried write keeps the parked bytes; another writer's
    // handover at the same ordinal still conflicts (FIG-3809).
    let written = PersistedSegmentHandover {
        writer: "segment-nonce-a".to_string(),
        segment_ordinal: 2,
        written_generation: Some(lash_core::engine::BuildGeneration::for_test("t0")),
        route: "LashProcessWorkflow".to_string(),
        handover: SegmentHandover {
            reason: BoundaryReason::JournalBudget,
            program_hash: "program-v1".to_string(),
            engine_state: vec![5, 6],
        },
    };
    store
        .put_segment_handover(&process_id, written.clone())
        .await
        .expect("persist the writer's handover");
    let mut rederived = written.clone();
    rederived.handover.engine_state.push(7);
    store
        .put_segment_handover(&process_id, rederived.clone())
        .await
        .expect("the writer's own retried write is idempotent");
    assert_eq!(
        store
            .get_segment_handover(&process_id, 2)
            .await
            .expect("read the writer's handover"),
        Some(written.clone()),
        "the parked bytes stay"
    );
    let mut other_writer = rederived;
    other_writer.writer = "segment-nonce-b".to_string();
    assert!(
        store
            .put_segment_handover(&process_id, other_writer)
            .await
            .is_err(),
        "another writer's handover at the same ordinal must conflict"
    );

    // FIG-3588: a retained handover carries its segment's start marker,
    // written set-if-absent. The first nonce stays; a second write reads it
    // back unchanged, which is how a different execution learns it lost.
    let segment = crate::ProcessSegmentKey::new(process_id.clone(), 1);
    assert_eq!(
        store
            .segment_start(&segment)
            .await
            .expect("read unstarted marker"),
        None,
        "a handed-over segment has not started"
    );
    let first = crate::SegmentStartMarker {
        nonce: "nonce-first".to_string(),
        started_at_ms: 10,
        build_generation: Some(lash_core::engine::BuildGeneration::for_test("conformance")),
    };
    assert_eq!(
        store
            .mark_segment_started(&segment, first.clone())
            .await
            .expect("mark the segment started"),
        first
    );
    assert_eq!(
        store
            .mark_segment_started(&segment, first.clone())
            .await
            .expect("the same execution re-marks idempotently"),
        first
    );
    assert_eq!(
        store
            .mark_segment_started(
                &segment,
                crate::SegmentStartMarker {
                    nonce: "nonce-other".to_string(),
                    started_at_ms: 20,
                    build_generation: None,
                },
            )
            .await
            .expect("a second execution's mark reads the first back"),
        first,
        "the recorded marker is never replaced"
    );
    assert_eq!(
        store.segment_start(&segment).await.expect("read marker"),
        Some(first)
    );
    assert!(
        store
            .mark_segment_started(
                &crate::ProcessSegmentKey::new(process_id.clone(), 7),
                crate::SegmentStartMarker {
                    nonce: "nonce-unretained".to_string(),
                    started_at_ms: 30,
                    build_generation: None,
                },
            )
            .await
            .is_err(),
        "a segment with no retained handover cannot be marked started"
    );
    assert_eq!(
        store
            .segment_start(&crate::ProcessSegmentKey::new(process_id.clone(), 7))
            .await
            .expect("read an unretained segment"),
        None
    );

    store
        .delete_segment_handovers(&process_id)
        .await
        .expect("delete handovers");
    assert_eq!(
        store
            .segment_start(&segment)
            .await
            .expect("read after delete"),
        None,
        "the marker goes with its handover"
    );
    assert!(
        store
            .latest_segment_handover(&process_id)
            .await
            .expect("read after delete")
            .is_none()
    );

    let pruned_continuation_conformance_record = registry
        .register_process(ProcessRegistration::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            RecoveryContract::ExternallyOwned,
            ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        ))
        .await
        .expect("register prunable continuation owner");
    let pruned_process_id = pruned_continuation_conformance_record.id.clone();
    let pruned_handover = PersistedSegmentHandover {
        writer: String::new(),
        segment_ordinal: 1,
        written_generation: Some(lash_core::engine::BuildGeneration::for_test("t0")),
        route: "LashProcessWorkflow".to_string(),
        handover: SegmentHandover {
            reason: BoundaryReason::JournalBudget,
            program_hash: "pruned-program-v1".to_string(),
            engine_state: vec![8, 1, 1],
        },
    };
    store
        .put_segment_handover(&pruned_process_id, pruned_handover)
        .await
        .expect("persist handover until terminal retention pruning");
    let terminal = registry
        .complete_process(
            &pruned_process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete prunable continuation owner");
    // FIG-3820: the stored terminal is the revocation. A successor's handover
    // put on an ended process is refused typed, in the transaction that would
    // park it, and the retained handover stays the latest.
    let refused = store
        .put_segment_handover(
            &pruned_process_id,
            PersistedSegmentHandover {
                segment_ordinal: 2,
                written_generation: Some(lash_core::engine::BuildGeneration::for_test("t0")),
                route: "LashProcessWorkflow".to_string(),
                writer: String::new(),
                handover: SegmentHandover {
                    reason: BoundaryReason::JournalBudget,
                    program_hash: "pruned-program-v1".to_string(),
                    engine_state: vec![8, 1, 2],
                },
            },
        )
        .await;
    assert!(
        matches!(
            refused,
            Err(crate::PluginError::ProcessAlreadyTerminal { .. })
        ),
        "a handover put on an ended process is refused typed: {refused:?}"
    );
    assert_eq!(
        store
            .latest_segment_handover(&pruned_process_id)
            .await
            .expect("read handover after the refused put")
            .map(|handover| handover.segment_ordinal),
        Some(1),
        "the refused put parks nothing"
    );
    // FIG-3819: an ended process starts no segment. The marker write is
    // refused typed in its own transaction and records nothing.
    let pruned_segment = crate::ProcessSegmentKey::new(pruned_process_id.clone(), 1);
    let refused = store
        .mark_segment_started(
            &pruned_segment,
            crate::SegmentStartMarker {
                nonce: "nonce-after-terminal".to_string(),
                started_at_ms: 40,
                build_generation: None,
            },
        )
        .await;
    assert!(
        matches!(
            refused,
            Err(crate::PluginError::ProcessAlreadyTerminal { .. })
        ),
        "a segment start on an ended process is refused typed: {refused:?}"
    );
    assert_eq!(
        store
            .segment_start(&pruned_segment)
            .await
            .expect("read the marker after the refused start"),
        None,
        "the refused start records no marker"
    );
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
            .latest_segment_handover(&pruned_process_id)
            .await
            .expect("read handover after terminal prune")
            .is_none(),
        "terminal process pruning must remove its retained handovers"
    );
}
