use lash_remote_protocol::{
    REMOTE_PROTOCOL_VERSION, RemoteProcessEventsRequest, RemoteProcessEventsResponse,
    RemoteProcessObservationItem, RemoteProcessObservationRequest, RemoteSessionObservationEvent,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

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
    Ok(Document {
        shape,
        version: REMOTE_PROTOCOL_VERSION,
        version_constant: "REMOTE_PROTOCOL_VERSION",
        schema,
    })
}

fn main() -> Result<(), String> {
    let documents = [
        document::<RemoteProcessEventsRequest>("remote-process-events-request")?,
        document::<RemoteProcessEventsResponse>("remote-process-events-response")?,
        document::<RemoteProcessObservationRequest>("remote-process-observation-request")?,
        document::<RemoteProcessObservationItem>("remote-process-observation-item")?,
        document::<RemoteSessionObservationEvent>("remote-session-observation-event")?,
    ];
    println!(
        "{}",
        serde_json::to_string(&documents)
            .map_err(|error| format!("cannot encode remote schemas: {error}"))?
    );
    Ok(())
}
