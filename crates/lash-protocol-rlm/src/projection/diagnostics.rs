/// Read committed extraction decisions in source order.
pub fn recorded_extraction_decisions(
    records: &[lash_core::SessionHistoryRecord],
) -> Result<Vec<String>, lash_protocol_types::allocation::ProjectionError> {
    let mut decisions = Vec::new();
    for record in records {
        let lash_core::SessionHistoryRecord::Protocol(event) = record else {
            continue;
        };
        let decoded = super::context::decode_rlm_protocol_event(event)?;
        let Some(lash_rlm_types::RlmProtocolEvent::RlmDiagnostic(diagnostic)) =
            decoded.as_deref().map(AsRef::as_ref)
        else {
            continue;
        };
        if !matches!(
            diagnostic.phase,
            lash_rlm_types::RlmDiagnosticPhase::LlmExtraction
                | lash_rlm_types::RlmDiagnosticPhase::NativeExtraction
        ) {
            continue;
        }
        let payload = serde_json::from_value::<ExtractionDecision>(diagnostic.payload.clone())
            .map_err(|error| lash_core::StoredDataCorruption {
                record_kind: "RLM extraction diagnostic".into(),
                message: error.to_string(),
            })?;
        decisions.push(payload.decision);
    }
    Ok(decisions)
}

/// The one field toolbench reads from an extraction diagnostic's payload.
#[derive(serde::Deserialize)]
struct ExtractionDecision {
    decision: String,
}
