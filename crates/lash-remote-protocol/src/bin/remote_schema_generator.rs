use lash_remote_protocol::{
    REMOTE_PROTOCOL_VERSION, RemoteProcessEventsRequest, RemoteProcessEventsResponse,
    RemoteProcessObservationItem, RemoteProcessObservationRequest, RemoteSessionObservationEvent,
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
    if let Some(definitions) = root.get_mut("definitions").and_then(Value::as_object_mut) {
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

fn documents() -> Result<[Document; 5], String> {
    Ok([
        document::<RemoteProcessEventsRequest>("remote-process-events-request")?,
        document::<RemoteProcessEventsResponse>("remote-process-events-response")?,
        document::<RemoteProcessObservationRequest>("remote-process-observation-request")?,
        document::<RemoteProcessObservationItem>("remote-process-observation-item")?,
        document::<RemoteSessionObservationEvent>("remote-session-observation-event")?,
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
    fn observation_item_types_the_snapshot_graph_and_node_event_record() {
        let item = documents()
            .expect("schemas generate")
            .into_iter()
            .find(|document| document.shape == "remote-process-observation-item")
            .expect("observation item is registered");
        let definitions = &item.schema["definitions"];
        assert_eq!(
            definitions["RemoteProcessObservationProjection"]["properties"]["graph"],
            json!({
                "anyOf": [{ "$ref": "#/definitions/TraceLashlangGraph" }, { "type": "null" }]
            })
        );
        let event = item.schema["oneOf"]
            .as_array()
            .expect("item variants")
            .iter()
            .find(|variant| variant["properties"]["type"]["enum"] == json!(["event"]))
            .expect("event variant");
        assert_eq!(
            event["properties"]["record"],
            json!({ "$ref": "#/definitions/TraceRecord" })
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
