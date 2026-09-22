use schemars::schema_for;
use serde_json::Value;
use workflow_graph_roundtrip::WorkflowDocument;

fn main() -> Result<(), String> {
    let mut schema = serde_json::to_value(schema_for!(WorkflowDocument))
        .map_err(|error| format!("cannot serialize WorkflowDocument schema: {error}"))?;
    schema["$id"] = Value::String(
        "https://lash.dev/examples/workflow-graph-roundtrip/workflow-document".to_string(),
    );
    println!(
        "{}",
        serde_json::to_string(&schema)
            .map_err(|error| format!("cannot encode WorkflowDocument schema: {error}"))?
    );
    Ok(())
}
