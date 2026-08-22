use std::collections::HashMap;

use super::*;
use crate::REMOTE_PROTOCOL_VERSION;
use crate::registry_errors::RemoteProtocolError;

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
fn in_progress_turn_report_is_refused_by_the_decoder_not_by_version_negotiation() {
    // Documents the actual refusal mechanism for a version 43 report.
    // `RemoteTurnReport` has no probe-first decoder: the removed status value
    // fails in serde, and only a report that decodes reaches the exact-version
    // check in `validate`.
    let mut payload = serde_json::to_value(RemoteTurnReport {
        protocol_version: 43,
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

    let decoded = serde_json::from_value::<RemoteTurnReport>(payload.clone())
        .expect("a version 43 report still decodes");
    assert!(matches!(
        decoded.validate(),
        Err(RemoteProtocolError::UnsupportedProtocolVersion {
            actual: 43,
            expected: REMOTE_PROTOCOL_VERSION,
        })
    ));

    payload["status"] = serde_json::json!("in_progress");
    let error = serde_json::from_value::<RemoteTurnReport>(payload)
        .expect_err("in_progress must not decode into a version 44 report");
    assert!(
        error.to_string().contains("in_progress"),
        "decoder must name the refused status: {error}"
    );
}
