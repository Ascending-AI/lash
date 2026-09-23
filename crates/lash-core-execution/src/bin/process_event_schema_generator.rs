//! Emits the checked-in JSON Schema documents for the runtime-owned durable
//! process-event payloads: `process.effect_outcome` and
//! `process.effect_omissions`.
//!
//! Each document is the payload schema the runtime registers and validates
//! appends against, stamped with [`PROCESS_EVENT_VOCABULARY_VERSION`]. There is
//! no second description of these payloads to drift from.

use lash_core_execution::runtime::process::{
    PROCESS_EFFECT_OMISSIONS_EVENT_TYPE, PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
    PROCESS_EVENT_VOCABULARY_VERSION, runtime_lifecycle_event_type,
};
use serde::Serialize;
use serde_json::{Value, json};

const VERSION_CONSTANT: &str = "PROCESS_EVENT_VOCABULARY_VERSION";
const SHAPES: [(&str, &str, &str); 2] = [
    (
        "process-effect-outcome",
        PROCESS_EFFECT_OUTCOME_EVENT_TYPE,
        "ProcessEffectSummaryOccurrence",
    ),
    (
        "process-effect-omissions",
        PROCESS_EFFECT_OMISSIONS_EVENT_TYPE,
        "ProcessEffectOmissions",
    ),
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
            let registered = runtime_lifecycle_event_type(event_type)
                .ok_or_else(|| format!("{event_type} is not a runtime-owned event kind"))?;
            let mut schema = registered.payload_schema.schema;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_the_registered_payload_schemas_stamped_with_the_vocabulary() {
        let documents = documents().expect("schemas generate");
        assert_eq!(documents.len(), SHAPES.len());
        for (document, (_, event_type, _)) in documents.iter().zip(SHAPES) {
            let registered = runtime_lifecycle_event_type(event_type)
                .expect("runtime-owned kind")
                .payload_schema
                .schema;
            let mut unstamped = document.schema.clone();
            let root = unstamped.as_object_mut().expect("object schema");
            for stamp in [
                "$schema",
                "$id",
                "title",
                "x-lash-event-type",
                "x-lash-schema-version",
                "x-lash-version-constant",
            ] {
                assert!(root.remove(stamp).is_some(), "{event_type} lacks {stamp}");
            }
            assert_eq!(
                unstamped, registered,
                "{event_type} publishes its validator"
            );
            assert_eq!(
                document.schema["properties"]["vocabulary_version"],
                json!({ "const": PROCESS_EVENT_VOCABULARY_VERSION })
            );
            assert_eq!(document.schema["additionalProperties"], json!(false));
        }
    }
}
