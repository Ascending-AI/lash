/// Read committed extraction decisions in source order.
pub fn recorded_extraction_decisions(
    records: &[lash_core::SessionHistoryRecord],
) -> Result<Vec<String>, lash_core::StoredDataCorruption> {
    let mut decisions = Vec::new();
    for record in records {
        let lash_core::SessionHistoryRecord::Protocol(event) = record else {
            continue;
        };
        let Some(lash_rlm_types::RlmProtocolEvent::RlmDiagnostic(diagnostic)) =
            super::context::decode_rlm_protocol_event(event)?
        else {
            continue;
        };
        if !matches!(
            diagnostic.phase.as_str(),
            "llm_extraction" | "native_extraction"
        ) {
            continue;
        }
        let payload =
            serde_json::from_value::<ExtractionDecision>(diagnostic.payload).map_err(|error| {
                lash_core::StoredDataCorruption {
                    record_kind: "RLM extraction diagnostic".into(),
                    message: error.to_string(),
                }
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
