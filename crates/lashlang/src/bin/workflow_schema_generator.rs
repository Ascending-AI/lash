use lashlang::{
    WORKFLOW_GRAPH_SCHEMA_VERSION, WORKFLOW_TYPE_FACET_SCHEMA_VERSION, WorkflowGraph,
    WorkflowNodeTypeFacets,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value, json};

const GRAPH_NAME: &str = "workflow-graph";
const FACET_NAME: &str = "workflow-type-facets";
const TOLERANT_FACET_OBJECTS: &[&str] = &[
    "WorkflowExpectedArgument",
    "WorkflowNodeTypeFacets",
    "WorkflowTypeDiagnostic",
    "WorkflowTypedVariable",
];

#[derive(Serialize)]
struct Document {
    shape: &'static str,
    version: u32,
    version_constant: &'static str,
    schema: Value,
}

fn main() -> Result<(), String> {
    let encoded = serde_json::to_string(&documents()?)
        .map_err(|error| format!("cannot encode generated schemas: {error}"))?;
    println!("{encoded}");
    Ok(())
}

fn documents() -> Result<Vec<Document>, String> {
    Ok(vec![
        Document {
            shape: GRAPH_NAME,
            version: WORKFLOW_GRAPH_SCHEMA_VERSION,
            version_constant: "WORKFLOW_GRAPH_SCHEMA_VERSION",
            schema: stamp_schema::<WorkflowGraph>(
                GRAPH_NAME,
                WORKFLOW_GRAPH_SCHEMA_VERSION,
                "WORKFLOW_GRAPH_SCHEMA_VERSION",
                true,
            )?,
        },
        Document {
            shape: FACET_NAME,
            version: WORKFLOW_TYPE_FACET_SCHEMA_VERSION,
            version_constant: "WORKFLOW_TYPE_FACET_SCHEMA_VERSION",
            schema: stamp_schema::<WorkflowNodeTypeFacets>(
                FACET_NAME,
                WORKFLOW_TYPE_FACET_SCHEMA_VERSION,
                "WORKFLOW_TYPE_FACET_SCHEMA_VERSION",
                false,
            )?,
        },
    ])
}

fn stamp_schema<T: JsonSchema>(
    shape: &str,
    version: u32,
    version_constant: &str,
    closed_root: bool,
) -> Result<Value, String> {
    let mut schema = serde_json::to_value(schemars::schema_for!(T))
        .map_err(|error| format!("cannot serialize {shape} schema: {error}"))?;
    let root = schema
        .as_object_mut()
        .ok_or_else(|| format!("{shape} schema root is not an object"))?;
    root.insert(
        "$id".to_string(),
        Value::String(format!("https://lash.dev/schemas/{shape}/v{version}")),
    );
    root.insert(
        "x-lash-schema-version".to_string(),
        Value::Number(version.into()),
    );
    root.insert(
        "x-lash-version-constant".to_string(),
        Value::String(version_constant.to_string()),
    );
    if closed_root {
        root.insert("additionalProperties".to_string(), Value::Bool(false));
        pin_property(root, "schema_version", version)?;
    }
    close_known_objects(root);
    Ok(schema)
}

fn close_known_objects(root: &mut Map<String, Value>) {
    let Some(definitions) = root.get_mut("definitions").and_then(Value::as_object_mut) else {
        return;
    };
    for (name, definition) in definitions {
        if TOLERANT_FACET_OBJECTS.contains(&name.as_str()) {
            continue;
        }
        close_object_schemas(definition);
    }
}

fn close_object_schemas(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                close_object_schemas(value);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("object") {
                object.insert("additionalProperties".to_string(), Value::Bool(false));
            }
            for value in object.values_mut() {
                close_object_schemas(value);
            }
        }
        _ => {}
    }
}

fn pin_property(root: &mut Map<String, Value>, name: &str, version: u32) -> Result<(), String> {
    let property = root
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|properties| properties.get_mut(name))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("schema has no object property `{name}`"))?;
    property.insert("enum".to_string(), json!([version]));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_stamped_with_their_owners() {
        let documents = documents().expect("schemas generate");
        assert_eq!(documents.len(), 2);
        for document in documents {
            assert_eq!(
                document.schema["x-lash-schema-version"],
                json!(document.version)
            );
            assert_eq!(
                document.schema["x-lash-version-constant"],
                json!(document.version_constant)
            );
        }
    }

    #[test]
    fn graph_schema_pins_the_decode_fence() {
        let graph = documents()
            .expect("schemas generate")
            .into_iter()
            .find(|document| document.shape == GRAPH_NAME)
            .expect("graph schema is registered");
        assert_eq!(graph.schema["additionalProperties"], json!(false));
        assert_eq!(
            graph.schema["properties"]["schema_version"]["enum"],
            json!([WORKFLOW_GRAPH_SCHEMA_VERSION])
        );
        assert_eq!(
            graph.schema["definitions"]["AssignTarget"]["additionalProperties"],
            json!(false)
        );
        assert!(
            graph.schema["definitions"]["WorkflowNodeTypeFacets"]
                .get("additionalProperties")
                .is_none()
        );
    }
}
