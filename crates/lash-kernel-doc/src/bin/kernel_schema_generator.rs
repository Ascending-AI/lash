//! Emits the checked-in JSON Schema documents for the kernel's stored
//! shapes: the workflow [`Document`], its [`Annotations`] and a
//! [`FunctionDefinition`].
//!
//! All three are owned by [`KERNEL_VERSION`]. Every object in them is
//! closed: an unknown member is refused, as the decoders refuse it.

use lash_kernel_doc::{Annotations, Document, FunctionDefinition, KERNEL_VERSION};
use schemars::JsonSchema;
use serde_json::{Value, json};

const VERSION_CONSTANT: &str = "KERNEL_VERSION";

fn main() -> Result<(), String> {
    let documents = vec![
        document::<Document>("kernel-document")?,
        document::<Annotations>("kernel-annotations")?,
        document::<FunctionDefinition>("kernel-function")?,
    ];
    println!("{}", Value::Array(documents));
    Ok(())
}

fn document<T: JsonSchema>(shape: &'static str) -> Result<Value, String> {
    let mut schema = serde_json::to_value(schemars::schema_for!(T))
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
