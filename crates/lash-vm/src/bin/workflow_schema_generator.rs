use lash_vm::{
    WORKFLOW_GRAPH_SCHEMA_VERSION, WORKFLOW_TYPE_FACET_SCHEMA_VERSION, WorkflowGraph,
    WorkflowNodeTypeFacets,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value, json};

const GRAPH_NAME: &str = "workflow-graph";
const FACET_NAME: &str = "workflow-type-facets";
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
    merge_container_node_schemas(root)?;
    Ok(schema)
}

/// Merge the node's `kind` tag into each container alternative and retain
/// its closed-object rule so unknown container fields remain invalid.
fn merge_container_node_schemas(root: &mut Map<String, Value>) -> Result<(), String> {
    let Some(node_kind) = root
        .get_mut("$defs")
        .and_then(Value::as_object_mut)
        .and_then(|definitions| definitions.get_mut("WorkflowNodeKind"))
    else {
        return Ok(());
    };
    let alternatives = node_kind
        .get_mut("oneOf")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "WorkflowNodeKind schema has no alternatives".to_string())?;
    let container_index = alternatives
        .iter()
        .position(|alternative| alternative["properties"]["kind"]["const"] == json!("container"))
        .ok_or_else(|| "WorkflowNodeKind schema has no container alternative".to_string())?;
    let container = alternatives.remove(container_index);
    let variants = container
        .get("oneOf")
        .and_then(Value::as_array)
        .ok_or_else(|| "container node schema has no container variants".to_string())?;
    let mut merged = Vec::with_capacity(variants.len());
    for variant in variants {
        let mut variant = variant.clone();
        let object = variant
            .as_object_mut()
            .ok_or_else(|| "container variant schema is not an object".to_string())?;
        object.insert("additionalProperties".to_string(), json!(false));
        object
            .entry("properties")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| "container variant properties are not an object".to_string())?
            .insert(
                "kind".to_string(),
                json!({ "const": "container", "type": "string" }),
            );
        let required = object
            .entry("required")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| "container variant required list is not an array".to_string())?;
        required.push(Value::String("kind".to_string()));
        required.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
        merged.push(variant);
    }
    alternatives.splice(container_index..container_index, merged);
    Ok(())
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
