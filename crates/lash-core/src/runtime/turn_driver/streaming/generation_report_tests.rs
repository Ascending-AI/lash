use super::*;

fn applied() -> Option<crate::GenerationReceipt> {
    Some(crate::GenerationReceipt {
        output_token_cap: crate::GenerationOptionOutcome::Applied,
        temperature: crate::GenerationOptionOutcome::Applied,
        seed: crate::GenerationOptionOutcome::NotRequested,
        stop_sequences: crate::GenerationOptionOutcome::NotRequested,
        cache: crate::GenerationOptionOutcome::NotRequested,
        ..crate::GenerationReceipt::default()
    })
}

fn attempt(generation_disposition: Option<crate::GenerationReceipt>) -> crate::AttemptRecord {
    crate::AttemptRecord {
        ordinal: 1,
        outcome: crate::AttemptOutcome::Completed,
        protocol_position: crate::ProtocolPosition::OutputStarted,
        retry_budget_consumed: false,
        retry_decision: None,
        error: None,
        evidence: None,
        generation_disposition,
        usage: None,
    }
}
fn call_record(attempts: Vec<crate::AttemptRecord>) -> crate::LlmCallRecord {
    crate::LlmCallRecord {
        call_id: crate::LlmCallId("call".to_string()),
        label: None,
        replay_drops: Vec::new(),
        attempts,
    }
}

#[test]
fn protocol_stop_suppression_leaves_unreported_attempts_absent() {
    let mut result: Result<LlmResponse, LlmCallError> = Ok(LlmResponse {
        generation_disposition: applied(),
        ..LlmResponse::default()
    });
    let mut call_record = call_record(vec![attempt(None), attempt(applied())]);

    record_protocol_owned_stop_suppression(&mut result, Some(&mut call_record));

    assert!(call_record.attempts[0].generation_disposition.is_none());
    assert_eq!(
        call_record.attempts[1]
            .generation_disposition
            .expect("reported attempt disposition")
            .stop_sequences,
        crate::GenerationOptionOutcome::SuppressedProtocolOwned
    );
}
