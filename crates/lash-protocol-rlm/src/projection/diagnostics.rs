/// Read committed extraction decisions in source order.
pub fn recorded_extraction_decisions(records: &[lash_core::SessionHistoryRecord]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| {
            let lash_core::SessionHistoryRecord::Protocol(event) = record else {
                return None;
            };
            let Some(lash_rlm_types::RlmProtocolEvent::RlmDiagnostic(diagnostic)) =
                super::context::decode_rlm_protocol_event(event)
            else {
                return None;
            };
            if !matches!(
                diagnostic.phase.as_str(),
                "llm_extraction" | "native_extraction"
            ) {
                return None;
            }
            serde_json::from_value::<ExtractionDecision>(diagnostic.payload)
                .ok()
                .map(|payload| payload.decision)
        })
        .collect()
}

/// The one field toolbench reads from an extraction diagnostic's payload.
#[derive(serde::Deserialize)]
struct ExtractionDecision {
    decision: String,
}
