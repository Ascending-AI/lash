use super::WorkloadSpec;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) fn positive_rate_schema(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = f64::json_schema(generator);
    schema.insert("exclusiveMinimum".to_string(), json!(0.0));
    schema
}

pub(super) fn confidence_schema(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = positive_rate_schema(generator);
    schema.insert("exclusiveMaximum".to_string(), json!(1.0));
    schema
}

pub(super) fn saturation_schema(generator: &mut SchemaGenerator) -> Schema {
    let mut schema = Vec::<f64>::json_schema(generator);
    schema.insert("minItems".to_string(), json!(1));
    schema.insert("items".to_string(), positive_rate_schema(generator).into());
    schema
}

pub(super) fn generate() -> Schema {
    let mut schema = schemars::schema_for!(WorkloadSpec);
    let mut paths = BTreeSet::new();
    paths_from_schema(schema.as_value(), "", &schema, &mut paths);
    let mut evidence = String::json_schema(&mut SchemaGenerator::default());
    evidence.insert("pattern".to_string(), json!("^[IV](: .+)?$"));
    if let Some(fields) = schema.pointer_mut("/$defs/Provenance/properties/fields") {
        *fields = json!({
            "type": "object",
            "additionalProperties": false,
            "required": paths,
            "properties": paths.iter().map(|path| (path.clone(), evidence.clone().into())).collect::<serde_json::Map<_, _>>()
        });
    }
    schema
}

fn paths_from_schema(value: &Value, path: &str, root: &Schema, paths: &mut BTreeSet<String>) {
    if let Some(reference) = value.get("$ref").and_then(Value::as_str)
        && let Some(resolved) = root.pointer(reference)
    {
        paths_from_schema(resolved, path, root, paths);
        return;
    }
    if let Some(properties) = value.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            if key != "provenance" {
                paths_from_schema(child, &format!("{path}/{key}"), root, paths);
            }
        }
        return;
    }
    paths.insert(path.to_owned());
}
