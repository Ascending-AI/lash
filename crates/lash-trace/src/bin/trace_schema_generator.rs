//! Emits the checked-in JSON Schema documents for the trace shapes a host
//! decodes: the NDJSON [`TraceRecord`] (with its [`lash_trace::TraceEvent`]
//! payload) and the trace-derived [`TraceLashlangGraph`] snapshot.
//!
//! Both documents are owned by [`TRACE_SCHEMA_VERSION`]. The version property
//! is pinned because a reader checks it exactly before decoding the shape.
//! Neither root is closed: the trace policy tolerates additive fields on a
//! known record, event or snapshot, while every enum stays closed.

use lash_trace::{TRACE_SCHEMA_VERSION, TraceLashlangGraph, TraceRecord};
use schemars::JsonSchema;
use serde_json::{Value, json};

const RECORD_NAME: &str = "trace-record";
const GRAPH_NAME: &str = "trace-lashlang-graph";
const VERSION_CONSTANT: &str = "TRACE_SCHEMA_VERSION";

/// One generated document. The registration the drift script reads is built
/// with `json!` rather than a Serde derive: this binary sits under the trace
/// crate's guarded sources, and its output envelope is not a trace shape.
struct Document {
    shape: &'static str,
    version: u32,
    version_constant: &'static str,
    schema: Value,
}

impl Document {
    fn registration(self) -> Value {
        json!({
            "shape": self.shape,
            "version": self.version,
            "version_constant": self.version_constant,
            "schema": self.schema,
        })
    }
}

fn main() -> Result<(), String> {
    let registrations = documents()?
        .into_iter()
        .map(Document::registration)
        .collect::<Vec<_>>();
    println!("{}", Value::Array(registrations));
    Ok(())
}

fn documents() -> Result<Vec<Document>, String> {
    Ok(vec![
        document::<TraceRecord>(RECORD_NAME)?,
        document::<TraceLashlangGraph>(GRAPH_NAME)?,
    ])
}

fn document<T: JsonSchema>(shape: &'static str) -> Result<Document, String> {
    let mut schema = serde_json::to_value(schemars::schema_for!(T))
        .map_err(|error| format!("cannot serialize {shape} schema: {error}"))?;
    let root = schema
        .as_object_mut()
        .ok_or_else(|| format!("{shape} schema root is not an object"))?;
    root.insert(
        "$id".to_string(),
        json!(format!(
            "https://lash.dev/schemas/{shape}/v{TRACE_SCHEMA_VERSION}"
        )),
    );
    root.insert(
        "x-lash-schema-version".to_string(),
        json!(TRACE_SCHEMA_VERSION),
    );
    root.insert(
        "x-lash-version-constant".to_string(),
        json!(VERSION_CONSTANT),
    );
    root.get_mut("properties")
        .and_then(|properties| properties.get_mut("schema_version"))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("{shape} schema has no schema_version property"))?
        .insert("enum".to_string(), json!([TRACE_SCHEMA_VERSION]));
    Ok(Document {
        shape,
        version: TRACE_SCHEMA_VERSION,
        version_constant: VERSION_CONSTANT,
        schema,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_stamped_and_pin_the_trace_version() {
        let documents = documents().expect("schemas generate");
        assert_eq!(
            documents
                .iter()
                .map(|document| document.shape)
                .collect::<Vec<_>>(),
            [RECORD_NAME, GRAPH_NAME]
        );
        for document in documents {
            assert_eq!(document.version, TRACE_SCHEMA_VERSION);
            assert_eq!(
                document.schema["x-lash-schema-version"],
                json!(TRACE_SCHEMA_VERSION)
            );
            assert_eq!(
                document.schema["x-lash-version-constant"],
                json!(VERSION_CONSTANT)
            );
            assert_eq!(
                document.schema["properties"]["schema_version"]["enum"],
                json!([TRACE_SCHEMA_VERSION])
            );
            // Additive fields are tolerated on a known record or snapshot.
            assert!(document.schema.get("additionalProperties").is_none());
        }
    }
}
