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
        usage_disposition: Default::default(),
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
fn cap_of(disposition: Option<crate::GenerationReceipt>) -> crate::GenerationOptionOutcome {
    disposition
        .expect("a reported disposition")
        .output_token_cap
}

/// A failed call still leaves accounts of itself behind: the ledger
/// attempt, and the partial response an adapter salvaged onto the error.
/// Narrowing one and not the other is how the same attempt comes to say
/// two different things.
#[test]
fn a_failed_calls_partial_response_agrees_with_its_ledger_attempt() {
    let mut result: Result<LlmResponse, LlmCallError> = Err(LlmCallError {
        message: "stream ended early".to_string(),
        retryable: false,
        kind: crate::ProviderFailureKind::Unknown,
        raw: None,
        code: None,
        terminal_reason: crate::LlmTerminalReason::ProviderError,
        request_body: None,
        partial_response: Some(Box::new(LlmResponse {
            generation_disposition: applied(),
            ..LlmResponse::default()
        })),
    });
    let mut call_record = call_record(vec![crate::AttemptRecord {
        ordinal: 1,
        outcome: crate::AttemptOutcome::Failed,
        protocol_position: crate::ProtocolPosition::OutputStarted,
        retry_budget_consumed: true,
        retry_decision: None,
        error: None,
        evidence: None,
        generation_disposition: applied(),
        usage: None,
        usage_disposition: Default::default(),
    }]);

    record_clamped_output_token_cap(&mut result, Some(&mut call_record));

    let partial = result
        .expect_err("the call failed")
        .partial_response
        .expect("the adapter salvaged a partial");
    assert_eq!(
        cap_of(partial.generation_disposition),
        crate::GenerationOptionOutcome::ClampedToCapacity
    );
    assert_eq!(
        cap_of(call_record.attempts[0].generation_disposition),
        crate::GenerationOptionOutcome::ClampedToCapacity
    );
}

/// An adapter that reports nothing keeps reporting nothing, and an option
/// the adapter dropped is not overwritten with a clamp it never applied.
#[test]
fn narrowing_only_touches_a_cap_the_adapter_reported_as_applied() {
    let mut unreported: Result<LlmResponse, LlmCallError> = Ok(LlmResponse::default());
    record_clamped_output_token_cap(&mut unreported, None);
    assert!(
        unreported.expect("ok").generation_disposition.is_none(),
        "None means unreported, not an invitation to invent a report"
    );

    let mut dropped: Result<LlmResponse, LlmCallError> = Ok(LlmResponse {
        generation_disposition: Some(crate::GenerationReceipt {
            output_token_cap: crate::GenerationOptionOutcome::OmittedUnsupported,
            ..Default::default()
        }),
        ..LlmResponse::default()
    });
    record_clamped_output_token_cap(&mut dropped, None);
    assert_eq!(
        cap_of(dropped.expect("ok").generation_disposition),
        crate::GenerationOptionOutcome::OmittedUnsupported
    );
}

#[test]
fn protocol_stop_suppression_updates_response_and_attempt_ledger() {
    let mut result: Result<LlmResponse, LlmCallError> = Ok(LlmResponse {
        generation_disposition: applied(),
        ..LlmResponse::default()
    });
    let mut call_record = call_record(vec![attempt(applied())]);

    record_protocol_owned_stop_suppression(&mut result, Some(&mut call_record));

    let response = result.expect("response");
    assert_eq!(
        response
            .generation_disposition
            .expect("response disposition")
            .stop_sequences,
        crate::GenerationOptionOutcome::SuppressedProtocolOwned
    );
    assert_eq!(
        call_record.attempts[0]
            .generation_disposition
            .expect("attempt disposition")
            .stop_sequences,
        crate::GenerationOptionOutcome::SuppressedProtocolOwned
    );
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
