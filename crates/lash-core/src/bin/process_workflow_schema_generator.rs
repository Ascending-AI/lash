//! Publish process workflow projections and kernel carry/park refusals.

use lash_core::ProcessWorkflowOverlay;
use lash_execution_contracts::process::PROCESS_EVENT_VOCABULARY_VERSION;
use schemars::JsonSchema;
use serde_json::{Value, json};

fn document<T: JsonSchema>(
    shape: &str,
    version: u32,
    constant: &str,
) -> Result<Value, serde_json::Error> {
    let mut schema = serde_json::to_value(schemars::schema_for!(T))?;
    schema["$id"] = json!(format!("https://lash.dev/schemas/{shape}/v{version}"));
    schema["x-lash-schema-version"] = json!(version);
    schema["x-lash-version-constant"] = json!(constant);
    Ok(json!({
        "shape": shape,
        "version": version,
        "version_constant": constant,
        "schema": schema,
    }))
}

fn main() -> Result<(), serde_json::Error> {
    println!(
        "{}",
        json!([document::<ProcessWorkflowOverlay>(
            "process-workflow-overlay",
            PROCESS_EVENT_VOCABULARY_VERSION,
            "PROCESS_EVENT_VOCABULARY_VERSION"
        )?,])
    );
    Ok(())
}
