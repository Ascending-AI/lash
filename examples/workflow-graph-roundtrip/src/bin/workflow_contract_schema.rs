use std::env;

use schemars::{JsonSchema, schema_for};
use serde_json::Value;
use workflow_graph_roundtrip::{ErrorBody, WorkflowDocument};

fn main() -> Result<(), String> {
    let shape = env::args()
        .nth(1)
        .unwrap_or_else(|| "workflow-document".to_string());
    let (id, mut schema) = match shape.as_str() {
        "workflow-document" => (
            "https://lash.dev/examples/workflow-graph-roundtrip/workflow-document",
            schema_value::<WorkflowDocument>()?,
        ),
        "error-response" => (
            "https://lash.dev/examples/workflow-graph-roundtrip/error-response",
            schema_value::<ErrorBody>()?,
        ),
        _ => return Err(format!("unknown workflow contract schema shape `{shape}`")),
    };
    schema["$id"] = Value::String(id.to_string());
    println!(
        "{}",
        serde_json::to_string(&schema)
            .map_err(|error| format!("cannot encode {shape} schema: {error}"))?
    );
    Ok(())
}

fn schema_value<T: JsonSchema>() -> Result<Value, String> {
    serde_json::to_value(schema_for!(T))
        .map_err(|error| format!("cannot serialize contract schema: {error}"))
}
