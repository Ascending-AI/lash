use std::collections::HashMap;

use super::*;
use crate::REMOTE_PROTOCOL_VERSION;
use crate::registry_errors::RemoteProtocolError;

#[test]
fn remote_turn_status_projects_explicit_stopped_outcome_as_failed() {
    assert_eq!(
        RemoteTurnStatus::from(&RemoteTurnOutcome::Stopped {
            stop: RemoteTurnStop::Incomplete,
        }),
        RemoteTurnStatus::Failed
    );
}

#[test]
fn remote_turn_status_no_longer_accepts_in_progress_on_the_wire() {
    // Version 44 removed the variant; a version 43 peer can still emit the
    // literal, so pin that the decoder and the published schema both refuse
    // it rather than mapping it onto a terminal status.
    let error = serde_json::from_value::<RemoteTurnStatus>(serde_json::json!("in_progress"))
        .expect_err("in_progress is no longer a remote turn status");
    assert!(
        error.to_string().contains("in_progress"),
        "decoder must name the refused value: {error}"
    );

    let schema = serde_json::to_value(schemars::schema_for!(RemoteTurnStatus))
        .expect("serialize turn status schema");
    assert!(
        !schema.to_string().contains("in_progress"),
        "published schema still advertises in_progress: {schema}"
    );
}

#[test]
fn in_progress_turn_report_is_refused_by_version_negotiation_before_body_decode() {
    // A version 43 report is refused before its removed status value reaches
    // the current body decoder.
    let mut payload = serde_json::to_value(RemoteTurnReport {
        session_id: "session".to_string(),
        turn_id: "turn".to_string(),
        status: RemoteTurnStatus::Completed,
        outcome: RemoteTurnOutcome::Finished {
            finish: RemoteTurnFinish::AssistantMessage {
                text: "done".to_string(),
            },
        },
        cancellation: None,
        assistant_output: RemoteAssistantOutput::default(),
        usage: RemoteTurnUsageReport::default(),
        execution: RemoteTurnExecutionMetrics::default(),
        tool_calls: Vec::new(),
        llm_calls: Vec::new(),
        issues: Vec::new(),
        activities: Vec::new(),
        metadata: HashMap::new(),
    })
    .expect("serialize version 43 report");
    payload["protocol_version"] = serde_json::json!(43);

    let wire = serde_json::to_vec(&payload).expect("serialize version 43 report");
    assert!(matches!(
        RemoteTurnReport::decode_json(&wire),
        Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 43,
            expected: REMOTE_PROTOCOL_VERSION,
        })
    ));

    payload["status"] = serde_json::json!("in_progress");
    let wire = serde_json::to_vec(&payload).expect("serialize version 43 report");
    assert!(matches!(
        RemoteTurnReport::decode_json(&wire),
        Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 43,
            expected: REMOTE_PROTOCOL_VERSION,
        })
    ));
}
