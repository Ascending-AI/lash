use super::*;
use lash_core_llm::stream_assembly::LlmStreamAccumulator;

fn request_disposition() -> crate::GenerationReceipt {
    crate::GenerationReceipt {
        output_token_cap: crate::GenerationOptionOutcome::Applied,
        temperature: crate::GenerationOptionOutcome::NotRequested,
        seed: crate::GenerationOptionOutcome::NotRequested,
        stop_sequences: crate::GenerationOptionOutcome::NotRequested,
        cache: crate::GenerationOptionOutcome::NotRequested,
        ..crate::GenerationReceipt::default()
    }
}

#[test]
fn abort_persists_request_disposition_and_typed_interruption() {
    let mut accumulator = LlmStreamAccumulator::default();
    accumulator.push_text("accepted protocol output");
    let evidence = crate::LlmStreamEvidence {
        request_body: Some(r#"{"stream":true,"stop":[]}"#.to_string()),
        generation_disposition: Some(request_disposition()),
        ..Default::default()
    };

    let response = protocol_handoff_response(
        &crate::LlmCallId("call-under-abort".to_string()),
        &accumulator,
        &evidence,
    );

    assert_eq!(
        response
            .generation_disposition
            .expect("request disposition")
            .stop_sequences,
        crate::GenerationOptionOutcome::NotRequested
    );
    assert_eq!(response.request_body, evidence.request_body);
    assert_eq!(
        response
            .execution_evidence
            .expect("typed interrupted evidence")
            .collection_interruption,
        Some(crate::ExecutionEvidenceCollectionInterruption::ProtocolAbort)
    );
}

#[test]
fn abort_suppression_updates_response_and_attempt_together() {
    let response = protocol_handoff_response(
        &crate::LlmCallId("call-under-abort".to_string()),
        &LlmStreamAccumulator::default(),
        &crate::LlmStreamEvidence {
            generation_disposition: Some(request_disposition()),
            ..Default::default()
        },
    );
    let mut record = crate::LlmCallRecord {
        call_id: crate::LlmCallId("call-under-abort".to_string()),
        label: None,
        replay_drops: Vec::new(),
        attempts: vec![crate::AttemptRecord {
            ordinal: 1,
            outcome: crate::AttemptOutcome::Completed,
            protocol_position: crate::ProtocolPosition::TerminalObserved,
            retry_budget_consumed: true,
            retry_decision: None,
            error: None,
            evidence: None,
            generation_disposition: Some(request_disposition()),
            usage: None,
        }],
    };
    let mut result = Ok(response);

    record_protocol_owned_stop_suppression(&mut result, Some(&mut record));

    assert_eq!(
        result
            .expect("abort response")
            .generation_disposition
            .expect("response disposition")
            .stop_sequences,
        crate::GenerationOptionOutcome::SuppressedProtocolOwned
    );
    assert_eq!(
        record.attempts[0]
            .generation_disposition
            .expect("attempt disposition")
            .stop_sequences,
        crate::GenerationOptionOutcome::SuppressedProtocolOwned
    );
}
