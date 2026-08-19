use super::*;

fn request_disposition() -> crate::GenerationReceipt {
    crate::GenerationReceipt {
        output_token_cap: crate::GenerationOptionOutcome::Applied,
        temperature: crate::GenerationOptionOutcome::NotRequested,
        seed: crate::GenerationOptionOutcome::NotRequested,
        stop_sequences: crate::GenerationOptionOutcome::NotRequested,
        cache: crate::GenerationOptionOutcome::NotRequested,
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

    let (response, record) = synthesize_protocol_abort(
        &accumulator,
        LlmUsage::default(),
        &evidence,
        17,
        std::time::Duration::from_millis(3),
        Vec::new(),
    );

    assert_eq!(
        response
            .generation_disposition
            .expect("request disposition")
            .stop_sequences,
        crate::GenerationOptionOutcome::NotRequested
    );
    assert_eq!(response.request_body, evidence.request_body);
    assert_eq!(record.attempts[0].outcome, crate::AttemptOutcome::Aborted);
    assert_eq!(
        response
            .execution_evidence
            .expect("typed interrupted evidence")
            .collection_interruption,
        Some(crate::ExecutionEvidenceCollectionInterruption::ProtocolAbort)
    );
}

#[test]
fn abort_retains_provider_usage_delivered_before_preemption() {
    let usage = LlmUsage {
        input_tokens: 11,
        output_tokens: 7,
        ..Default::default()
    };
    let provider_usage = serde_json::json!({
        "input_tokens": 11,
        "output_tokens": 7
    });
    let mut evidence = crate::LlmStreamEvidence {
        generation_disposition: Some(request_disposition()),
        ..Default::default()
    };
    evidence
        .merge(crate::LlmStreamEvidence {
            provider_usage: Some(provider_usage.clone()),
            ..Default::default()
        })
        .expect("provider usage without execution identity remains mergeable");

    let (response, record) = synthesize_protocol_abort(
        &LlmStreamAccumulator::default(),
        usage.clone(),
        &evidence,
        19,
        std::time::Duration::from_millis(5),
        Vec::new(),
    );

    assert_eq!(response.usage, usage);
    assert_eq!(response.provider_usage, Some(provider_usage));
    assert_eq!(record.attempts[0].usage, Some(response.usage));
}

#[test]
fn abort_suppression_updates_response_and_attempt_together() {
    let (response, mut record) = synthesize_protocol_abort(
        &LlmStreamAccumulator::default(),
        LlmUsage::default(),
        &crate::LlmStreamEvidence {
            generation_disposition: Some(request_disposition()),
            ..Default::default()
        },
        23,
        std::time::Duration::ZERO,
        Vec::new(),
    );
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
