//! Emits the checked-in JSON Schema documents for the kernel's edit data:
//! an edit [`Transaction`] and the [`Correspondence`] it publishes.
//!
//! Both are owned by [`KERNEL_VERSION`]: their payloads are the forms and
//! sites of that version. Every object in them is closed.

use lash_kernel_doc::KERNEL_VERSION;
use lash_kernel_edit::{Correspondence, Transaction};
use schemars::JsonSchema;
use serde_json::{Value, json};

const VERSION_CONSTANT: &str = "KERNEL_VERSION";

fn main() -> Result<(), String> {
    let documents = vec![
        document::<Transaction>("kernel-edit-transaction")?,
        document::<Correspondence>("kernel-correspondence")?,
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
