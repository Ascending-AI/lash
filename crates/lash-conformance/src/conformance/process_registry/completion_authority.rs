//! Cross-backend laws for ADR 0027: `complete_process` carries an explicit
//! completion authority, and each backend validates it against the row's
//! input class inside the terminal transaction — `ExternalOwner` closes an
//! externally-owned row, the workflow authorities close an engine-executed
//! row, and every other pairing is refused.

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

/// The granted half of the matrix: each authority commits on the input class
/// it names, and the committed terminal event carries the authority.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_completion_authority_matching_its_input_class_commits(
    registry: Arc<dyn ProcessRegistry>,
) {
    async fn granted(
        registry: &Arc<dyn ProcessRegistry>,
        registration: ProcessRegistration,
        authority: ProcessCompletionAuthority,
    ) {
        let process_id = registry
            .register_process(registration)
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
            .expect("an authority commits on the input class it names");
        assert!(
            matches!(
                committed,
                crate::ProcessCompletionOutcome::Committed(ref stored) if stored.is_terminal()
            ),
            "the terminal write committed: {committed:?}"
        );
        assert_eq!(
            completion_authority_evidence(registry, &process_id).await,
            serde_json::to_value(&authority).expect("a completion authority serializes"),
            "the terminal event records the validated authority as audit evidence"
        );
    }

    granted(
        &registry,
        registration("completion-authority-external"),
        ProcessCompletionAuthority::external_owner(),
    )
    .await;
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
        granted(&registry, executed_registration(label), authority).await;
    }
}

/// The refused half of the matrix: an external owner never closes an
/// engine-executed row, and a workflow authority never closes an
/// externally-owned row. Each refusal is typed and writes no terminal.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_completion_authority_for_the_wrong_input_class_is_refused(
    registry: Arc<dyn ProcessRegistry>,
) {
    let engine_id = registry
        .register_process(executed_registration("completion-authority-engine"))
        .await
        .expect("register the engine-executed process")
        .id;
    assert_session_refusal(
        registry
            .complete_process(
                &engine_id,
                settled_success(serde_json::Value::Null),
                ProcessCompletionAuthority::external_owner(),
            )
            .await,
        &format!(
            "process `{engine_id}` cannot be completed with external-owner authority: only \
             externally-owned rows may be completed by an external owner; a lash-executed row has \
             its engine as its single writer"
        ),
    );

    let external_id = registry
        .register_process(registration("completion-authority-external"))
        .await
        .expect("register the externally-owned process")
        .id;
    for authority in [
        ProcessCompletionAuthority::workflow_key("suite:completion-authority"),
        ProcessCompletionAuthority::WorkflowKeyRecovery {
            workflow_key: "suite:completion-authority".to_string(),
            segment_ordinal: 0,
        },
    ] {
        assert_session_refusal(
            registry
                .complete_process(
                    &external_id,
                    settled_success(serde_json::Value::Null),
                    authority.clone(),
                )
                .await,
            &format!(
                "process `{external_id}` cannot be completed with {} authority: externally-owned \
                 rows are never executed by a workflow substrate; they close through their \
                 external owner",
                authority.label()
            ),
        );
    }

    for process_id in [&engine_id, &external_id] {
        assert!(
            !registry
                .get_process(process_id)
                .await
                .expect("read the refused process")
                .expect("the refused process is retained")
                .is_terminal(),
            "a refused authority writes no terminal"
        );
    }
}
