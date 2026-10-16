//! Emits the transitive page exchange and carried-session wire contract.
use lash_vm_client::wire::StartWire;
use lash_vm_protocol::WORKER_PROTOCOL_VERSION;
use lash_vm_protocol::page::{PageExchangeId, PageReply, PageRequest, SealBatch};
use schemars::JsonSchema;
use serde_json::{Value, json};

fn document<T: JsonSchema>(shape: &str) -> Result<Value, serde_json::Error> {
    let mut schema = serde_json::to_value(schemars::schema_for!(T))?;
    schema["$id"] = json!(format!(
        "https://lash.dev/schemas/{shape}/v{WORKER_PROTOCOL_VERSION}"
    ));
    schema["x-lash-schema-version"] = json!(WORKER_PROTOCOL_VERSION);
    schema["x-lash-version-constant"] = json!("WORKER_PROTOCOL_VERSION");
    Ok(json!({
        "shape": shape,
        "version": WORKER_PROTOCOL_VERSION,
        "version_constant": "WORKER_PROTOCOL_VERSION",
        "schema": schema,
    }))
}

fn main() -> Result<(), serde_json::Error> {
    println!(
        "{}",
        json!([
            document::<StartWire>("worker-start")?,
            document::<(PageExchangeId, PageRequest)>("worker-page-request")?,
            document::<(PageExchangeId, PageReply)>("worker-page-reply")?,
            document::<SealBatch>("worker-seal-batch")?,
        ])
    );
    Ok(())
}
