use super::*;

#[test]
fn every_generation_receipt_row_crosses_the_boundary_and_the_pinned_disposition_is_refused() {
    let receipt = core_llm::GenerationReceipt {
        output_token_cap: core_llm::GenerationOptionOutcome::ClampedToCapacity,
        temperature: core_llm::GenerationOptionOutcome::Applied,
        seed: core_llm::GenerationOptionOutcome::NotRequested,
        stop_sequences: core_llm::GenerationOptionOutcome::SuppressedProtocolOwned,
        cache: core_llm::GenerationOptionOutcome::OmittedUnsupported,
        reasoning: core_llm::GenerationOptionOutcome::Applied,
        reasoning_retention: core_llm::GenerationOptionOutcome::Applied,
        parallel_tool_calls: core_llm::GenerationOptionOutcome::Applied,
        thinking_summary: core_llm::GenerationOptionOutcome::Applied,
        thinking_visibility: core_llm::GenerationOptionOutcome::Applied,
        passthrough: core_llm::GenerationOptionOutcome::Applied,
    };
    let wire = serde_json::to_value(RemoteGenerationReceipt::from(receipt)).expect("serialize");
    assert_eq!(wire["reasoning"], serde_json::json!("applied"));
    assert_eq!(wire["reasoning_retention"], serde_json::json!("applied"));
    assert_eq!(wire["parallel_tool_calls"], serde_json::json!("applied"));
    assert_eq!(wire["thinking_summary"], serde_json::json!("applied"));
    assert_eq!(wire["thinking_visibility"], serde_json::json!("applied"));
    assert_eq!(wire["passthrough"], serde_json::json!("applied"));
    let decoded: RemoteGenerationReceipt = serde_json::from_value(wire).expect("deserialize");
    assert_eq!(core_llm::GenerationReceipt::from(decoded), receipt);

    // The removed sampling-pinned disposition is refused, not mapped: pinned
    // sampling now refuses the call before any I/O.
    assert!(
        serde_json::from_value::<RemoteGenerationOptionOutcome>(serde_json::json!(
            "omitted_sampling_pinned"
        ))
        .is_err()
    );
}
