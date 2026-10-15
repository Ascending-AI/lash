//! Emits the checked-in JSON Schema document of the [`WorkflowView`] a host
//! draws and edits against, with the run [`WorkflowCoverage`] it reads
//! through `coverage`.
//!
//! It is owned by [`KERNEL_VERSION`]: its payloads are the forms and sites of
//! that version. Every object is closed and no position admits arbitrary
//! JSON.

use lash_kernel_doc::KERNEL_VERSION;
use lash_kernel_view::{WorkflowCoverage, WorkflowView};
use schemars::JsonSchema;
use serde_json::{Value, json};

const VERSION_CONSTANT: &str = "KERNEL_VERSION";

fn main() -> Result<(), String> {
    // The view document also ships the run coverage a host reads through
    // `coverage`, so its declaration names every type a run's JSON answers
    // with.
    let documents = vec![document::<WorkflowView>(
        "kernel-workflow-view",
        |generator| {
            generator.subschema_for::<WorkflowCoverage>();
        },
    )?];
    println!("{}", Value::Array(documents));
    Ok(())
}

fn document<T: JsonSchema>(
    shape: &'static str,
    extra: impl FnOnce(&mut schemars::SchemaGenerator),
) -> Result<Value, String> {
    let mut generator = schemars::SchemaGenerator::default();
    extra(&mut generator);
    let mut schema = serde_json::to_value(generator.into_root_schema_for::<T>())
        .map_err(|error| format!("cannot serialize {shape} schema: {error}"))?;
    let root = schema
        .as_object_mut()
        .ok_or_else(|| format!("{shape} schema root is not an object"))?;
    root.insert(
        "$id".to_string(),
        Value::String(format!(
            "https://lash.dev/schemas/{shape}/v{KERNEL_VERSION}"
        )),
    );
    root.insert(
        "x-lash-schema-version".to_string(),
        Value::Number(KERNEL_VERSION.into()),
    );
    root.insert(
        "x-lash-version-constant".to_string(),
        Value::String(VERSION_CONSTANT.to_string()),
    );
    Ok(json!({
        "shape": shape,
        "version": KERNEL_VERSION,
        "version_constant": VERSION_CONSTANT,
        "schema": schema,
    }))
}
