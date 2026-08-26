/// Upgrade response-shaped values carried by pre-v3 session-node bodies.
///
/// Protocol events deliberately carry opaque JSON, so the node decoder walks
/// the body and recognizes an `LlmResponse` by its complete stable field set
/// before consuming the retired `full_text` field. This keeps unrelated plugin
/// payloads with coincidental `full_text` and `parts` keys untouched.
pub(super) fn upgrade_session_node_llm_responses(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                upgrade_session_node_llm_responses(value);
            }
        }
        serde_json::Value::Object(object) => {
            for value in object.values_mut() {
                upgrade_session_node_llm_responses(value);
            }

            let is_llm_response = [
                "full_text",
                "parts",
                "usage",
                "terminal_reason",
                "terminal_diagnostic",
                "provider_usage",
                "request_body",
                "http_summary",
                "execution_evidence",
            ]
            .into_iter()
            .all(|field| object.contains_key(field));
            if !is_llm_response {
                return;
            }

            let Some(serde_json::Value::String(full_text)) = object.remove("full_text") else {
                return;
            };
            if full_text.is_empty() {
                return;
            }
            let parts_project_no_text = object
                .get("parts")
                .cloned()
                .and_then(|parts| serde_json::from_value::<Vec<crate::LlmOutputPart>>(parts).ok())
                .is_some_and(|parts| {
                    crate::facade_support::visible_response_text_from_parts(&parts).is_empty()
                });
            if !parts_project_no_text {
                return;
            }
            let Some(parts) = object
                .get_mut("parts")
                .and_then(serde_json::Value::as_array_mut)
            else {
                return;
            };
            parts.push(
                serde_json::to_value(crate::LlmOutputPart::Text {
                    text: full_text,
                    response_meta: None,
                })
                .expect("LlmOutputPart serialization is infallible"),
            );
        }
        _ => {}
    }
}
