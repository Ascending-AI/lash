use lash_remote_protocol::{
    REMOTE_PROTOCOL_VERSION, RemoteConfigCommandCatalog, RemoteConfigTransactionOutcome,
    RemoteConfigTransactionRequest, RemotePersistProcessEnvRequest, RemoteProcessEventsRequest,
    RemoteProcessEventsResponse, RemoteProcessObservationItem, RemoteProcessObservationRequest,
    RemoteProcessRecord, RemoteSendOutcome, RemoteSessionObservationEvent,
};
use lash_trace::TRACE_SCHEMA_VERSION;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

/// Trace shapes a remote document embeds. Their own version is exact before
/// decode, so each embedded definition pins it like the trace documents do.
const TRACE_DEFINITIONS: [&str; 2] = ["TraceRecord", "TraceLashlangGraph"];

#[derive(Serialize)]
struct Document {
    shape: &'static str,
    version: u32,
    version_constant: &'static str,
    schema: Value,
}

fn document<T: JsonSchema>(shape: &'static str) -> Result<Document, String> {
    let mut schema = serde_json::to_value(schemars::schema_for!(T))
        .map_err(|error| format!("cannot serialize {shape} schema: {error}"))?;
    let root = schema
        .as_object_mut()
        .ok_or("schema root is not an object")?;
    root.insert(
        "$id".to_string(),
        json!(format!(
            "https://lash.dev/schemas/{shape}/v{REMOTE_PROTOCOL_VERSION}"
        )),
    );
    root.insert(
        "x-lash-schema-version".to_string(),
        json!(REMOTE_PROTOCOL_VERSION),
    );
    root.insert(
        "x-lash-version-constant".to_string(),
        json!("REMOTE_PROTOCOL_VERSION"),
    );
    if let Some(definitions) = root.get_mut("$defs").and_then(Value::as_object_mut) {
        for name in TRACE_DEFINITIONS {
            if let Some(definition) = definitions.get_mut(name) {
                definition
                    .get_mut("properties")
                    .and_then(|properties| properties.get_mut("schema_version"))
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| format!("{shape}: {name} has no schema_version property"))?
                    .insert("enum".to_string(), json!([TRACE_SCHEMA_VERSION]));
            }
        }
    }
    Ok(Document {
        shape,
        version: REMOTE_PROTOCOL_VERSION,
        version_constant: "REMOTE_PROTOCOL_VERSION",
        schema,
    })
}

fn documents() -> Result<[Document; 11], String> {
    Ok([
        document::<RemotePersistProcessEnvRequest>("remote-persist-process-env-request")?,
        document::<RemoteConfigTransactionRequest>("remote-config-transaction-request")?,
        document::<RemoteConfigTransactionOutcome>("remote-config-transaction-outcome")?,
        document::<RemoteConfigCommandCatalog>("remote-config-command-catalog")?,
        document::<RemoteProcessRecord>("remote-process-record")?,
        document::<RemoteProcessEventsRequest>("remote-process-events-request")?,
        document::<RemoteProcessEventsResponse>("remote-process-events-response")?,
        document::<RemoteProcessObservationRequest>("remote-process-observation-request")?,
        document::<RemoteProcessObservationItem>("remote-process-observation-item")?,
        document::<RemoteSessionObservationEvent>("remote-session-observation-event")?,
        document::<RemoteSendOutcome>("remote-send-outcome")?,
    ])
}

fn main() -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string(&documents()?)
            .map_err(|error| format!("cannot encode remote schemas: {error}"))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_policy_schema_requires_both_recorded_controls() {
        use lash_remote_protocol::{RemoteProcessExecutionEnvSpec, RemoteTurnBudget};

        let schema =
            document::<RemotePersistProcessEnvRequest>("remote-persist-process-env-request")
                .expect("environment schema generates")
                .schema;
        let validator = jsonschema::validator_for(&schema).expect("schema compiles");
        let request = RemotePersistProcessEnvRequest {
            env_spec: RemoteProcessExecutionEnvSpec::new(
                RemoteTurnBudget::Unbounded,
                lash_sansio::MaxToolCalls::new(1024).non_zero(),
            ),
        };
        let value = serde_json::to_value(request).expect("environment serializes");
        assert!(validator.is_valid(&value));
        for field in ["max_tool_calls", "no_progress_budget", "charge_safety"] {
            let mut incomplete = value.clone();
            incomplete["env_spec"]["policy"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(!validator.is_valid(&incomplete), "schema requires {field}");
            assert!(
                serde_json::from_value::<RemotePersistProcessEnvRequest>(incomplete).is_err(),
                "the peer decoder requires {field}"
            );
        }
        let mut zero = value;
        zero["env_spec"]["policy"]["no_progress_budget"] = json!({"bounded": 0});
        assert!(!validator.is_valid(&zero));
        assert!(serde_json::from_value::<RemotePersistProcessEnvRequest>(zero).is_err());
        let mut no_calls = serde_json::to_value(RemotePersistProcessEnvRequest {
            env_spec: RemoteProcessExecutionEnvSpec::new(
                RemoteTurnBudget::Unbounded,
                lash_sansio::MaxToolCalls::new(1024).non_zero(),
            ),
        })
        .expect("environment serializes");
        no_calls["env_spec"]["policy"]["max_tool_calls"] = json!(0);
        assert!(
            !validator.is_valid(&no_calls),
            "zero is not a tool-call limit"
        );
        assert!(serde_json::from_value::<RemotePersistProcessEnvRequest>(no_calls).is_err());
    }

    #[test]
    fn published_integer_schemas_preserve_their_existing_validation_range() {
        let documents = documents().expect("schemas generate");
        for (shape, name, mut value, path) in [
            (
                "remote-process-events-response",
                "AttachmentSource",
                json!({"source": "inline", "media_type": "application/octet-stream", "bytes": [256]}),
                "/bytes/0",
            ),
            (
                "remote-session-observation-event",
                "RemoteNormalizedError",
                json!({"class": "transport", "http_status": 65536}),
                "/http_status",
            ),
            (
                "remote-session-observation-event",
                "RemoteToolIntentRefusalReason",
                json!({"reason": "unsupported_protocol_version", "recorded": 65536}),
                "/recorded",
            ),
        ] {
            let schema = &documents
                .iter()
                .find(|document| document.shape == shape)
                .unwrap()
                .schema;
            let schema = json!({
                "$schema": schema["$schema"],
                "$defs": schema["$defs"],
                "$ref": format!("#/$defs/{name}"),
            });
            let validator = jsonschema::validator_for(&schema).expect("schema compiles");
            assert!(
                validator.is_valid(&value),
                "{name} gained an integer upper bound"
            );
            *value.pointer_mut(path).unwrap() = json!(-1);
            assert!(
                !validator.is_valid(&value),
                "{name} lost its zero lower bound"
            );
        }
    }

    #[test]
    fn observation_event_schema_validates_serialized_variants_and_rejects_unknown_fields() {
        use lash_remote_protocol::{
            RemoteSessionObservationEventPayload as Payload, RemoteSessionProcessEventKind,
            RemoteSessionQueueEventKind, RemoteTurnActivity, RemoteTurnEvent,
        };
        let schema = document::<RemoteSessionObservationEvent>("remote-session-observation-event")
            .expect("observation event schema generates")
            .schema;
        let validator = jsonschema::validator_for(&schema).expect("schema compiles");
        let events = [
            Payload::TurnActivity {
                activity: Box::new(RemoteTurnActivity {
                    sequence: 1,
                    id: "activity".to_string(),
                    correlation_id: "correlation".to_string(),
                    event: RemoteTurnEvent::TurnStarted {
                        turn_id: "turn".into(),
                    },
                }),
            },
            Payload::Committed,
            Payload::ResidentChanged,
            Payload::AgentFrameSwitched {
                frame_id: "frame".to_string(),
            },
            Payload::QueueChanged {
                kind: RemoteSessionQueueEventKind::Enqueued,
                batch_ids: vec!["batch".to_string()],
            },
            Payload::ProcessChanged {
                kind: RemoteSessionProcessEventKind::Started { sequence: 1 },
                process_ids: Vec::new(),
            },
        ];
        for event in events {
            let event = RemoteSessionObservationEvent {
                session_id: "session".into(),
                replay_incarnation_id: "incarnation".to_string(),
                turn_id: Some("turn".into()),
                revision: 1,
                cursor: "cursor".to_string(),
                event,
            };
            event.validate().expect("valid event document");
            let mut value = serde_json::to_value(&event).expect("event serializes");
            assert!(
                validator.is_valid(&value),
                "schema rejected {}",
                value["type"]
            );
            value.as_object_mut().unwrap().remove("turn_id");
            assert!(
                validator.is_valid(&value),
                "optional turn_id may be omitted"
            );
            value["unexpected"] = json!(true);
            assert!(
                !validator.is_valid(&value),
                "schema accepted an unknown event field"
            );
            value.as_object_mut().unwrap().remove("unexpected");
            value.as_object_mut().unwrap().remove("cursor");
            assert!(
                !validator.is_valid(&value),
                "schema accepted an incomplete envelope"
            );
        }
    }

    #[test]
    fn observation_item_types_the_snapshot_graph_and_node_event_record() {
        let item = documents()
            .expect("schemas generate")
            .into_iter()
            .find(|document| document.shape == "remote-process-observation-item")
            .expect("observation item is registered");
        let definitions = &item.schema["$defs"];
        assert_eq!(
            definitions["RemoteProcessObservationProjection"]["properties"]["graph"],
            json!({
                "anyOf": [{ "$ref": "#/$defs/TraceLashlangGraph" }, { "type": "null" }]
            })
        );
        let event = item.schema["oneOf"]
            .as_array()
            .expect("item variants")
            .iter()
            .find(|variant| variant["properties"]["type"]["const"] == json!("event"))
            .expect("event variant");
        assert_eq!(
            event["properties"]["record"],
            json!({ "$ref": "#/$defs/TraceRecord" })
        );
        for name in TRACE_DEFINITIONS {
            assert_eq!(
                definitions[name]["properties"]["schema_version"]["enum"],
                json!([TRACE_SCHEMA_VERSION]),
                "{name} pins the trace version"
            );
        }
    }
}
