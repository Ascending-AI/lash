//! Emits the checked-in JSON Schema documents for the runtime-owned durable
//! process-event payloads: `process.effect_outcome`,
//! `process.effect_omissions` and `process.ran`.
//!
//! Each document is the payload schema the runtime registers and validates
//! appends against, stamped with [`PROCESS_EVENT_VOCABULARY_VERSION`]. There is
//! no second description of these payloads to drift from.

use lash_execution_contracts::process::PROCESS_EFFECT_OMISSIONS_EVENT_TYPE;
use lash_execution_contracts::process::PROCESS_EFFECT_OUTCOME_EVENT_TYPE;
use lash_execution_contracts::process::PROCESS_EVENT_VOCABULARY_VERSION;
use lash_execution_contracts::process::PROCESS_RAN_EVENT_TYPE;
use lash_execution_contracts::process::ProcessEventKind;
use serde::Serialize;
use serde_json::{Value, json};

const VERSION_CONSTANT: &str = "PROCESS_EVENT_VOCABULARY_VERSION";
const SHAPES: [(&str, &str, &str); 3] = [
    (
        "process-effect-outcome",
        PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
        "ProcessEffectOccurrence",
    ),
    (
        "process-effect-omissions",
        PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
        "ProcessEffectOmissions",
    ),
    ("process-ran", PROCESS_RAN_EVENT_TYPE, "ProcessRan"),
];

#[derive(Serialize)]
struct Document {
    shape: &'static str,
    version: u32,
    version_constant: &'static str,
    schema: Value,
}

fn main() -> Result<(), String> {
    let encoded = serde_json::to_string(&documents()?)
        .map_err(|error| format!("cannot encode process event schemas: {error}"))?;
    println!("{encoded}");
    Ok(())
}

fn documents() -> Result<Vec<Document>, String> {
    SHAPES
        .into_iter()
        .map(|(shape, event_type, title)| {
            let mut schema = ProcessEventKind::parse(event_type)
                .and_then(ProcessEventKind::payload_schema)
                .ok_or_else(|| format!("{event_type} is not a runtime-owned event kind"))?
                .into_value();
            let root = schema
                .as_object_mut()
                .ok_or_else(|| format!("{event_type} payload schema is not an object"))?;
            root.insert(
                "$schema".to_string(),
                json!("http://json-schema.org/draft-07/schema#"),
            );
            root.insert(
                "$id".to_string(),
                json!(format!(
                    "https://lash.dev/schemas/{shape}/v{PROCESS_EVENT_VOCABULARY_VERSION}"
                )),
            );
            root.insert("title".to_string(), json!(title));
            root.insert("x-lash-event-type".to_string(), json!(event_type));
            root.insert(
                "x-lash-schema-version".to_string(),
                json!(PROCESS_EVENT_VOCABULARY_VERSION),
            );
            root.insert(
                "x-lash-version-constant".to_string(),
                json!(VERSION_CONSTANT),
            );
            Ok(Document {
                shape,
                version: PROCESS_EVENT_VOCABULARY_VERSION,
                version_constant: VERSION_CONSTANT,
                schema,
            })
        })
        .collect()
}
