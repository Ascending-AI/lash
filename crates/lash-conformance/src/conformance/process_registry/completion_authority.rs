//! Cross-backend laws for ADR 0027: `complete_process` carries an explicit
//! completion authority, each backend validates it inside the terminal
//! transaction, and the committed terminal event records it.

use super::*;
use pretty_assertions::assert_eq;

/// The validated authority a committed terminal event records as audit
/// evidence (ADR 0027: the event records the validated authority).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn completion_authority_evidence(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> serde_json::Value {
    registry
        .full_event_window(process_id, 0)
        .await
        .expect("read the terminal event log")
        .into_iter()
        .find(|event| event.event_type == "process.completed")
        .and_then(|event| event.payload.get("completion_authority").cloned())
        .expect("the terminal event records its validated authority")
}

/// Each workflow authority commits on a process lash executes, and the
/// committed terminal event carries the authority.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_completion_authority_commits_and_records_its_evidence(
    registry: Arc<dyn ProcessRegistry>,
) {
    for (label, authority) in [
        (
            "completion-authority-workflow-key",
            ProcessCompletionAuthority::workflow_key("suite:completion-authority"),
        ),
        (
            "completion-authority-workflow-recovery",
            ProcessCompletionAuthority::WorkflowKeyRecovery {
                workflow_key: "suite:completion-authority".to_string(),
                segment_ordinal: 0,
            },
        ),
    ] {
        let process_id = registry
            .register_process(executed_registration(label))
            .await
            .expect("register the completion-authority process")
            .id;
        let committed = registry
            .complete_process(
                &process_id,
                settled_success(serde_json::json!({"closed_by": authority.label()})),
                authority.clone(),
            )
            .await
            .expect("a workflow authority commits on a process lash executes");
        assert!(
            matches!(
                committed,
                crate::ProcessCompletionOutcome::Committed(ref stored) if stored.is_terminal()
            ),
            "the terminal write committed: {committed:?}"
        );
        assert_eq!(
            completion_authority_evidence(&registry, &process_id).await,
            serde_json::to_value(&authority).expect("a completion authority serializes"),
            "the terminal event records the validated authority as audit evidence"
        );
    }
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub(super) async fn terminal_completion_replay_keeps_original_authority_and_writes_nothing(
    registry: Arc<dyn ProcessRegistry>,
) {
    for carrier in [None, Some(0), Some(2)] {
        let authorities = [
            ProcessCompletionAuthority::workflow_key("original-workflow"),
            ProcessCompletionAuthority::WorkflowKeyRecovery {
                workflow_key: "original-recovery".into(),
                segment_ordinal: carrier.unwrap_or(0),
            },
        ];
        for original in &authorities {
            let record = registry
                .register_process(executed_registration("terminal-authority-engine"))
                .await
                .expect("register authority case");
            let id = record.id;
            if let Some(ordinal) = carrier {
                registry
                    .set_external_ref(
                        &id,
                        crate::ProcessExternalRef {
                            backend: "workflow-engine".into(),
                            id: "carrier".into(),
                            metadata: None,
                            segment_ordinal: Some(ordinal),
                        },
                    )
                    .await
                    .expect("record segment carrier");
            }
            let output = settled_success(serde_json::json!({"original": true}));
            let committed = registry
                .complete_process(&id, output.clone(), original.clone())
                .await
                .expect("original authority commits");
            let before = serde_json::to_value(&*committed).expect("serialize retained process");
            let events = registry
                .full_event_window(&id, 0)
                .await
                .expect("original events");
            let evidence = completion_authority_evidence(&registry, &id).await;
            let replay_authorities = [
                ProcessCompletionAuthority::workflow_key("different-workflow"),
                ProcessCompletionAuthority::WorkflowKeyRecovery {
                    workflow_key: "different-recovery".into(),
                    segment_ordinal: 0,
                },
                ProcessCompletionAuthority::WorkflowKeyRecovery {
                    workflow_key: "different-recovery".into(),
                    segment_ordinal: 2,
                },
                ProcessCompletionAuthority::WorkflowKeyRecovery {
                    workflow_key: "different-recovery".into(),
                    segment_ordinal: 3,
                },
            ];
            for authority in replay_authorities {
                let superseded = matches!(authority,
                    ProcessCompletionAuthority::WorkflowKeyRecovery { segment_ordinal, .. }
                        if carrier.unwrap_or(0) > segment_ordinal);
                for proposed in [
                    output.clone(),
                    settled_success(serde_json::json!({"drifted": true})),
                ] {
                    let result = registry
                        .complete_process_with_prelude(
                            &id,
                            proposed.clone(),
                            vec![crate::ProcessEventAppendRequest::new(
                                "replay.must.not.append",
                                serde_json::json!({"changed": true}),
                            )],
                            authority.clone(),
                        )
                        .await;
                    if superseded {
                        assert!(
                            matches!(result, Err(PluginError::ProcessHandedOver { ref process_id, segment_ordinal: 2 }) if process_id == id),
                            "later segment refuses replay: {result:?}"
                        );
                    } else {
                        let result = result.expect("valid repeat returns retained terminal");
                        assert_eq!(
                            serde_json::to_value(&*result).expect("serialize replay"),
                            before
                        );
                        assert!(if proposed == output {
                            matches!(
                                result,
                                crate::ProcessCompletionOutcome::AlreadyApplied { .. }
                            )
                        } else {
                            matches!(result, crate::ProcessCompletionOutcome::Superseded { .. })
                        });
                    }
                    assert_eq!(
                        serde_json::to_value(
                            registry
                                .get_process(&id)
                                .await
                                .expect("read process")
                                .expect("process retained")
                        )
                        .expect("serialize process"),
                        before
                    );
                    assert_eq!(
                        serde_json::to_value(
                            registry
                                .full_event_window(&id, 0)
                                .await
                                .expect("read events")
                        )
                        .expect("serialize events"),
                        serde_json::to_value(&events).expect("serialize original events")
                    );
                    assert_eq!(
                        completion_authority_evidence(&registry, &id).await,
                        evidence
                    );
                }
            }
        }
    }
}
