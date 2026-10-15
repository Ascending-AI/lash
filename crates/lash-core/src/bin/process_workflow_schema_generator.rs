//! Publish the process workflow projection of the committed event vocabulary.

use lash_core::{PROCESS_EVENT_VOCABULARY_VERSION, ProcessWorkflowOverlay};
use serde_json::json;

fn main() -> Result<(), serde_json::Error> {
    let version = PROCESS_EVENT_VOCABULARY_VERSION;
    let mut schema = serde_json::to_value(schemars::schema_for!(ProcessWorkflowOverlay))?;
    schema["$id"] = json!(format!(
        "https://lash.dev/schemas/process-workflow-overlay/v{version}"
    ));
    schema["x-lash-schema-version"] = json!(version);
    schema["x-lash-version-constant"] = json!("PROCESS_EVENT_VOCABULARY_VERSION");
    println!(
        "{}",
        json!([{
            "shape": "process-workflow-overlay",
            "version": version,
            "version_constant": "PROCESS_EVENT_VOCABULARY_VERSION",
            "schema": schema,
        }])
    );
    Ok(())
}
