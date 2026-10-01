use lash::persistence::SessionNodePayload;
use serde_json::Value;

/// Read the recovery record from committed history, including prior frames.
pub fn recovery_record(payload: &SessionNodePayload, body: &Value) -> bool {
    let SessionNodePayload::Plugin {
        plugin_type,
        body: recorded,
    } = payload
    else {
        return false;
    };
    plugin_type == "standard_compaction.overflow_recovery" && recorded.as_ref() == body
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash::persistence::SessionHistoryRecord;
    use serde_json::json;

    #[test]
    fn recovery_evidence_requires_typed_committed_plugin_records() {
        for kind in ["pending", "completed"] {
            let body = json!({"kind": kind});
            let payload = SessionNodePayload::Plugin {
                plugin_type: "standard_compaction.overflow_recovery".to_string(),
                body: lash::messages::SharedJsonValue::new(body.clone()),
            };
            assert!(recovery_record(&payload, &body), "missing durable {kind}");
            assert!(!recovery_record(&payload, &json!({"kind": "failed"})));
            assert!(!recovery_record(
                &SessionNodePayload::Plugin {
                    plugin_type: "another_plugin".to_string(),
                    body: lash::messages::SharedJsonValue::new(body.clone()),
                },
                &body
            ));
            assert!(!recovery_record(
                &SessionNodePayload::Plugin {
                    plugin_type: "standard_compaction.overflow_recovery".to_string(),
                    body: lash::messages::SharedJsonValue::new(
                        json!({"kind": kind, "unexpected": true})
                    ),
                },
                &body
            ));
        }
        let prose = SessionNodePayload::Event {
            event: SessionHistoryRecord::Conversation(lash_core::facade_support::ConversationRecord {
                id: "marker".to_string(),
                role: lash_core::MessageRole::System,
                parts: std::sync::Arc::new(vec![lash_core::Part::text(
                    "marker-part".to_string(),
                    "Standard-compaction context-overflow recovery marker (pending):\n{\"kind\":\"pending\"}".to_string(),
                    None,
                )]),
                origin: Some(lash_core::MessageOrigin::Plugin {
                    plugin_id: "standard_compaction".to_string(),
                    transient: false,
                }),
            }),
        };
        assert!(!recovery_record(&prose, &json!({"kind": "pending"})));
    }
}
